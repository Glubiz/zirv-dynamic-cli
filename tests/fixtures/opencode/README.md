# OpenCode adapter fixtures (issue #385)

Source: `opencode-ai` npm package **v1.18.29** (published 2026-09-07),
GitHub `anomalyco/opencode` at git tag `v1.18.29` (not `dev`, which can be
ahead of what is actually shipped). Every schema fact these fixtures encode
was fetched directly from that tag via `gh api
repos/anomalyco/opencode/contents/<path>?ref=v1.18.29`:

- `packages/core/src/session/sql.ts` -- `session`/`session_message` Drizzle
  table definitions (columns, indexes).
- `packages/schema/src/session-message.ts` -- the `SessionMessage.Message`
  tagged union that `session_message.data` (a JSON-encoded TEXT column)
  decodes to, minus `type`/`id` (which live as their own top-level columns).
- `packages/schema/src/session-id.ts` / `packages/schema/src/identifier.ts`
  -- session/message id shape (`"ses_"`/`"msg_"` + `[0-9A-Za-z]+`).

`shadow-rows.jsonl` is hand-built to the exact row shape
`OpenCodeAdapter::parse_events`/`structural_context` consume: one compact
JSON object per line, keyed `rowid`, `type`, `seq`, `data` (a JSON-encoded
**string**, matching how `ShadowTranscript::sync_sqlite` serializes a TEXT
column -- never a nested object), and `time_created`. It is not a capture
from a real OpenCode run (no installation was available to this pass); it is
built directly from the verified schema above.

The SQLite fixture used by `transcript_path`'s own tests is built inline,
inside the test, with the exact `CREATE TABLE session (...)`/`CREATE TABLE
session_message (...)` column list `session/sql.ts` states -- there is no
static `.db` file in this directory, since a SQLite file is not diffable and
the inline `CREATE TABLE` is itself the citable, reviewable fact.
