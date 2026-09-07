# droid transcript fixture

`session.jsonl` is a trimmed, hand-edited copy of a REAL transcript written by
the actual `droid.exe` binary (`@factory/cli-win32-x64@0.213.0`, downloaded
via `npm pack @factory/cli-win32-x64@0.213.0` on 2026-09-07 into this
worktree's `target/` and run directly -- never installed globally, never a
real `FACTORY_API_KEY`). Only the giant injected system-context text block
was shortened (its own row shape -- `"id":"context-<uuid>"`, `"message":
{...,"visibility":"llm_only"}` -- is preserved verbatim); every other field
name, row envelope, and content-block shape is exactly what the binary wrote.

## How it was produced

1. A local Node.js HTTP server (`mock-llm-server.js`, not committed) served an
   OpenAI-compatible `/v1/chat/completions` endpoint on `127.0.0.1`, replying
   with Server-Sent-Events chunks (droid's own chat-completions request sets
   `"stream":true`, confirmed by inspecting the raw request body) -- first a
   `tool_calls` delta naming the built-in `Execute` tool, then a plain text
   reply.
2. `~/.factory/settings.json` (pointed at an isolated scratch `HOME`/
   `USERPROFILE`, never the real developer profile) registered that server as
   a BYOK custom model, per `docs.factory.ai/model-independence/byok`'s own
   documented schema:
   ```json
   {
     "customModels": [
       {
         "model": "mock-model",
         "displayName": "Mock Model",
         "baseUrl": "http://127.0.0.1:8934/v1",
         "apiKey": "sk-mock",
         "provider": "generic-chat-completion-api",
         "maxOutputTokens": 4096
       }
     ]
   }
   ```
3. `droid.exe exec --model custom:mock-model --auto low --output-format json
   --cwd "<scratch>/work repo" "please fix the bug"` ran end-to-end (exit 0,
   `num_turns: 2`), and the resulting
   `~/.factory/sessions/<cwd-slug>/<session-id>.jsonl` is the source of this
   fixture (see `src/commands/ctx/adapters/droid.rs`'s own module doc comment
   for the full verification narrative, including the two independent
   `session_dir_slug` probes and the doc-vs-binary disagreements this
   surfaced).

## Sources

- Real binary: `@factory/cli-win32-x64@0.213.0` (`droid.exe --help`,
  `droid.exe exec --help`, `droid.exe exec --list-tools`, and the live BYOK
  run above), all captured 2026-09-07.
- `docs.factory.ai/model-independence/byok` (fetched 2026-09-07): the
  `customModels`/`settings.json` schema used to run the live verification.
- `docs.factory.ai/droid-cli/cli-reference`,
  `docs.factory.ai/cli/configuration/custom-slash-commands` (fetched
  2026-09-07): the `/compress`/`/quit` interactive slash commands, which the
  binary's own `--help` never lists (only a live REPL's `/help` would).
- `docs.factory.ai/reference/hooks-reference` (fetched 2026-09-07): cited
  ONLY to note where it disagrees with the real binary (a
  `~/.factory/projects/...` transcript path that the shipped 0.213.0 binary
  does not use -- see the adapter module's own "Doc vs. binary
  disagreements" section).
