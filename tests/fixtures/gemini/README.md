# Gemini CLI chat-session fixture

`chat-session.json` is a hand-built JSONL file (one JSON object per line --
the fixture keeps the `.json` extension because gemini-cli itself accepts
both `.json` and `.jsonl` for a session file, per its own filter in
`getAllSessionFiles`) shaped after the real, verified row shapes
`ChatRecordingService` writes in `@google/gemini-cli@0.58.0`
(`npm pack @google/gemini-cli@latest`, captured 2026-09-07).

JSON has no comment syntax, so the provenance for each line lives here
instead of inline in the fixture:

- **Line 1 (metadata record)**: `{sessionId, projectHash, startTime,
  lastUpdated, kind}`, written once by `ChatRecordingService.initialize()`
  before any message. Verified in `bundle/chunk-FQCNOBUR.js` (also present,
  byte-for-byte equivalent, in `chunk-MFLFXOVQ.js` and `chunk-RTL6OG34.js`).
- **User rows** (`type: "user"`): `{id, timestamp, type, content,
  displayContent}`, from `ChatRecordingService.newMessage`/`recordMessage`.
  Written once per user turn; gemini-cli never re-appends a user row.
- **Gemini rows** (`type: "gemini"`): the same `newMessage` shape plus
  `thoughts`, `tokens`, `model` (`recordMessage`'s own `if (msg.type ===
  "gemini")` branch). `tokens` is `{input, output, cached, thoughts, tool,
  total}`, sourced from the Gemini API's own `promptTokenCount`/
  `candidatesTokenCount`/`cachedContentTokenCount`/`thoughtsTokenCount`/
  `toolUsePromptTokenCount`/`totalTokenCount` (`recordMessageTokens`).
  `toolCalls[]` entries carry at least `id`/`name`/`displayName`/`args` per
  `recordToolCalls`'s own enrichment step -- no verified per-call
  result/status shape was found in the time available, so this fixture's
  `toolCalls` entries are illustrative only and nothing in
  `src/commands/ctx/adapters/gemini.rs` reads them.
- **Duplicate `id` rows**: this is the fixture's own point. `m1` and `m2`
  each appear TWICE with the same `id` -- once from the initial
  `recordMessage`/`recordToolCalls` call (before token counts are known),
  once more after `recordMessageTokens` attaches the real `tokens` object
  and calls `pushMessage` again. `ChatRecordingService.pushMessage` always
  `appendRecord`s the full row again rather than emitting a delta, which is
  why the SAME logical turn can occupy more than one JSONL line before the
  next turn begins. `GeminiAdapter`'s `fold_gemini_rows` (see its own doc
  comment) folds these consecutive same-`id` runs into one logical turn,
  keeping the LATEST non-null `tokens`/non-empty `text` seen for that id --
  which is exactly what this fixture's two `parse_events`/`transcript_usage`
  tests assert (turn 1: `input_tokens == 120`; turn 2: `input_tokens == 340`,
  "the second turn's own later tokens value wins").
- **`{"$set": {...}}` rows**: metadata-only updates (`updateMetadata`,
  e.g. bumping `lastUpdated`) -- carry no `type` field and are skipped by
  every reader in `gemini.rs`.

Source files cited above are inside the tarball at
`package/bundle/chunk-FQCNOBUR.js` (and its near-duplicates
`chunk-MFLFXOVQ.js`, `chunk-RTL6OG34.js`), unpacked from
`@google/gemini-cli@0.58.0` on 2026-09-07. That tarball is not committed to
this repository -- only this fixture and the citations above are.
