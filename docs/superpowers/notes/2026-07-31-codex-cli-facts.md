# codex CLI facts (verified 2026-07-31)

codex version: verified: `codex-cli 0.146.0` (`codex --version`)
Install method: verified: `brew install codex` (Homebrew cask; not published under `@openai/codex` on npm as the plan guessed -- brew succeeded on the first try, so npm was never attempted). Binary: `/opt/homebrew/bin/codex`, package `/opt/homebrew/Caskroom/codex/0.146.0`.

**Addendum (2026-08-15):** the "not published on npm" line above is stale --
`@openai/codex` is on npm after all. Verified on a second machine (Windows):
`npm install -g @openai/codex` installs `codex-cli 0.105.0` as
`%APPDATA%\npm\codex.cmd` (a `#!/bin/sh` shim invoking `node
node_modules/@openai/codex/bin/codex.js`), exactly the argv-reparse-prone
shape `CodexAdapter::base()`'s Windows shim handling (`resolve_program`) now
routes through `cmd.exe /c`. Left as an addendum rather than an edit to the
original line: the original capture (brew, 2026-07-31) is still accurate for
what it actually tested, npm was just never attempted there, not
unavailable.

**Addendum (2026-08-15, round 3):** this file now deliberately cites two
different codex-cli versions' `codex exec --help` output side by side, not
by accident -- the original `--sandbox`/`-m` capture above is 0.146.0
(brew), while `--ignore-rules`/`--ignore-user-config` (quoted verbatim
further down, in that same 0.146.0 capture) do **not** exist on 0.105.0, the
version actually verified installed on the Windows machine above via `npm
install -g @openai/codex`. `CodexAdapter::distiller_cmd`'s own doc comment
in `codex.rs` is written against 0.105.0 specifically (the version most
operators get), so it does not add those two flags even though this file
documents them for 0.146.0 -- see that doc comment and Known Issues for the
residual this leaves (the distiller still reads the repo's `.rules` and the
operator's own config). If the two versions' flag sets ever converge, this
split stops being a real discrepancy to track.

## Headless invocation

verified: `codex exec [OPTIONS] [PROMPT]` -- matches the spec's guess. If `[PROMPT]` is omitted (or is `-`), instructions are read from stdin.

verified: running from a non-git directory (`/tmp/codex-probe`) is refused ("Not inside a trusted directory and --skip-git-repo-check was not specified.") unless `--skip-git-repo-check` is passed. Not an issue for real use since the adapter always runs inside the target repo, but needed for probing.

verified: a real `codex exec` run prints a preamble to stdout before any model output:
```
OpenAI Codex v0.146.0
--------
workdir: /private/tmp/codex-probe
model: gpt-5.6-sol
provider: openai
approval: never
sandbox: read-only
reasoning effort: none
reasoning summaries: none
session id: 019fb964-0989-7073-9e93-dec46e692346
--------
```

verified: `codex exec --json <prompt>` streams a *different* JSONL schema to stdout: `{"type":"thread.started","thread_id":"..."}`, `{"type":"turn.started"}`, `{"type":"item.completed","item":{...}}`, `{"type":"error","message":"..."}`, `{"type":"turn.failed","error":{...}}`. This is NOT the same schema as the persisted rollout file (see below) -- it is a live event stream, not the transcript. The adapter parses the rollout file on disk, not this stream, so this is recorded for completeness only.

### Verbatim `codex exec --help` (re-run 2026-07-31, unchanged from the original capture)

This is quoted in full, not paraphrased, so the flags `distiller_cmd` relies on (`-m`/`--model`) are auditable directly rather than taken on trust:

```
$ codex exec --help
Run Codex non-interactively

Usage: codex exec [OPTIONS] [PROMPT]
       codex exec [OPTIONS] <COMMAND> [ARGS]

Commands:
  resume  Resume a previous session by id or pick the most recent with --last
  review  Run a code review against the current repository
  help    Print this message or the help of the given subcommand(s)

Arguments:
  [PROMPT]
          Initial instructions for the agent. If not provided as an argument (or if `-` is used),
          instructions are read from stdin. If stdin is piped and a prompt is also provided, stdin
          is appended as a `<stdin>` block

Options:
  -c, --config <key=value>
          Override a configuration value that would otherwise be loaded from `~/.codex/config.toml`.
          Use a dotted path (`foo.bar.baz`) to override nested values. The `value` portion is parsed
          as TOML. If it fails to parse as TOML, the raw string is used as a literal.
          
          Examples: - `-c model="o3"` - `-c 'sandbox_permissions=["disk-full-read-access"]'` - `-c
          shell_environment_policy.inherit=all`

      --enable <FEATURE>
          Enable a feature (repeatable). Equivalent to `-c features.<name>=true`

      --disable <FEATURE>
          Disable a feature (repeatable). Equivalent to `-c features.<name>=false`

      --strict-config
          Error out when config.toml contains fields that are not recognized by this version of
          Codex

  -i, --image <FILE>...
          Optional image(s) to attach to the initial prompt

  -m, --model <MODEL>
          Model the agent should use

      --oss
          Use open-source provider

      --local-provider <OSS_PROVIDER>
          Specify which local provider to use (lmstudio or ollama). If not specified with --oss,
          will use config default or show selection

  -p, --profile <CONFIG_PROFILE_V2>
          Layer $CODEX_HOME/<name>.config.toml on top of the base user config

  -s, --sandbox <SANDBOX_MODE>
          Select the sandbox policy to use when executing model-generated shell commands
          
          [possible values: read-only, workspace-write, danger-full-access]

      --dangerously-bypass-approvals-and-sandbox
          Skip all confirmation prompts and execute commands without sandboxing. EXTREMELY
          DANGEROUS. Intended solely for running in environments that are externally sandboxed

      --dangerously-bypass-hook-trust
          Run enabled hooks without requiring persisted hook trust for this invocation. DANGEROUS.
          Intended only for automation that already vets hook sources

  -C, --cd <DIR>
          Tell the agent to use the specified directory as its working root

      --add-dir <DIR>
          Additional directories that should be writable alongside the primary workspace

      --skip-git-repo-check
          Allow running Codex outside a Git repository

      --ephemeral
          Run without persisting session files to disk

      --ignore-user-config
          Do not load `$CODEX_HOME/config.toml`; auth still uses `CODEX_HOME`

      --ignore-rules
          Do not load user or project execpolicy `.rules` files

      --output-schema <FILE>
          Path to a JSON Schema file describing the model's final response shape

      --color <COLOR>
          Specifies color settings for use in the output
          
          [default: auto]
          [possible values: always, never, auto]

      --json
          Print events to stdout as JSONL

  -o, --output-last-message <FILE>
          Specifies file where the last message from the agent should be written

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version
```

verified: the `-m, --model <MODEL>` flag ("Model the agent should use") is real on `codex exec` -- confirmed above and independently reconfirmed with `codex exec --help 2>&1 | grep -B1 -A6 -- '-m, --model'` on the same install, both returning the identical block. It is also present on top-level `codex --help` with the same description. `distiller_cmd`'s `.arg("--model")` is backed by this flag, not an assumption.

## Session id handling

verified: `codex exec --help` has **no `--session-id` (or any session-id) flag**. Codex always mints its own session id (UUID-shaped, e.g. `019fb964-0989-7073-9e93-dec46e692346`) and reports it only in the stdout preamble and inside the rollout file's `session_meta` event. The only session-id-accepting subcommands are `codex exec resume <SESSION_ID> [PROMPT]` and `codex resume <SESSION_ID>` (interactive) -- both **resume an existing** session, neither lets a caller pre-assign an id for a new one.

This breaks the plan's general assumption (global-constraints.md: "Session ids must be generated up front so transcript paths are known before launch") for codex specifically: a codex session id is only knowable *after* `codex exec` has started, by reading its stdout preamble or the rollout file, not before. `CodexAdapter::headless_cmd` therefore cannot use its `session: &SessionId` parameter the way `ClaudeAdapter` does (there is no flag to put it on); the parameter is accepted for trait-shape parity but unused, exactly as recorded here.

## Session file path template

verified: `~/.codex/sessions/<YYYY>/<MM>/<DD>/rollout-<YYYY-MM-DDTHH-MM-SS>-<session-uuid>.jsonl`, e.g.:
```
~/.codex/sessions/2026/07/31/rollout-2026-07-31T20-16-08-019fb964-0989-7073-9e93-dec46e692346.jsonl
```
Confirmed stable across two separate `codex exec` runs on the same day (both landed in `2026/07/31/`). The cwd is **not** part of the path (unlike claude's per-project slug directory) -- cwd is only recorded inside the `session_meta`/`turn_context` event payloads, not the file path. Because the directory depends on the session's start date/time, which `SessionRef` does not carry, `transcript_path` cannot compute the path directly the way `ClaudeAdapter` does; it must scan `~/.codex/sessions/**/*-<uuid>.jsonl` for a filename ending in the session id. This mirrors claude's existing scan-fallback, just as the only strategy rather than a fallback.

## Event types and shapes

verified: top-level `type` values in the rollout file: `session_meta`, `event_msg`, `response_item`, `world_state`, `turn_context`.

### Turn boundary

verified: `event_msg` rows carry a nested `payload.type`; `task_started` and `task_complete` bracket a turn and share a `turn_id`. `task_complete` carries `last_agent_message` (`null` observed on failure) and, on failure, an `error: {message, codex_error_info}` object plus `started_at`/`completed_at`/`duration_ms`.

### Assistant text

BLOCKED: every real invocation attempted failed with HTTP 401 (no Codex credentials configured; see notify section for why authentication was not attempted -- see guardrail in task brief) before the model produced any output. No successful turn was observed, so the `response_item` shape that would carry assistant text (`payload.type == "message"`, presumably `role == "assistant"`, by analogy with the observed `role: "developer"` / `role: "user"` message shapes) is **unverified**. Do not guess this shape.

### Tool call

BLOCKED: same reason -- no successful turn means no tool-call-equivalent `response_item` was ever emitted or observed.

### Tool result / error flag

BLOCKED: same reason.

### Token usage

BLOCKED: same reason. The one `task_complete` event observed (on a failed turn) has no usage field at all; a successful turn's usage field name(s) are unverified.

## notify contract (argv vs stdin, payload fields)

BLOCKED. This codex version appears to have **replaced** the `notify` mechanism the plan assumed with something else entirely. Evidence:
- `~/.codex/config.toml` does not exist on this fresh install, so there was no existing `notify = [...]` entry to inspect.
- `codex --help 2>&1 | grep -i notify` and `codex exec --help 2>&1 | grep -i notify` both return **nothing** -- zero mentions of "notify" anywhere in this CLI's own help text.
- `codex features list` shows a `hooks` feature flag (`stable`, currently enabled), and the top-level `--help` documents `--dangerously-bypass-hook-trust` ("Run enabled hooks without requiring persisted hook trust for this invocation. DANGEROUS."). This implies codex 0.146.0 has moved to a "hooks" mechanism with a trust/approval step, not the plan's assumed bare `notify = ["program"]` config array.
- No documentation ships with the brew cask (`codex-resources/` only contains a `zsh` completions directory) and there is no `codex hooks` subcommand to introspect the config shape non-interactively.

Per the task brief's explicit instruction not to guess this fact, the payload's delivery mechanism (argv vs stdin), field names, and the field carrying the rollout/session path are all **unverified**. `NOTIFY_TRANSCRIPT_KEYS` / `CODEX_NOTIFY_SAMPLE` are therefore left untouched at their Task A16 placeholder values (note: as of this task, A16 has not yet landed on this branch -- batch 3 runs before batch 5 in the plan's sequencing -- so `hook.rs` does not yet define these symbols; see the codex adapter report for detail. This does not block A9/A10 since the notify-mapping step is itself skipped.).

## Interactive quit command

BLOCKED: interactive `codex` (bare, no subcommand) requires a real TTY -- probing it non-interactively from this environment produced `Error: stdin is not a terminal` and exited immediately, so no interactive session could be started to observe its slash commands. Grepped all text captured from a real (failed) session transcript, including the full developer/system prompt content, for slash-command mentions; none of `/quit`, `/exit`, `/compact`, or similar appear anywhere. The existing placeholder (`/quit\r`) is left as-is, unverified.

## Cheap model alias for distillation

verified: `codex debug models` (a local catalog render, works without auth) lists, in priority order: `gpt-5.6-sol` ("Latest frontier agentic coding model" -- confirmed as the default used when no `-m` is given, from the preamble above), `gpt-5.6-terra` ("Balanced agentic coding model for everyday work"), `gpt-5.6-luna` ("Fast and affordable agentic coding model", `visibility: list`), and an older, hidden `gpt-5.4-mini` ("Small, fast, and cost-efficient model for simpler coding tasks", `visibility: hide`). `gpt-5.6-luna` is the current-generation fast/cheap tier and is recorded as the distillation alias, analogous to claude's `haiku`.

## Capabilities conclusion (marker_signal / token_usage / turn_signal)

- `marker_signal`: `false` -- spec-mandated regardless of CLI behavior (codex gets no marker signal in v1); unchanged from the existing placeholder.
- `token_usage`: BLOCKED, left at the existing neutral value `false` -- no successful turn was observed to confirm a usage field exists or its name.
- `turn_signal`: BLOCKED, left at the existing neutral value `false` -- the `task_started`/`task_complete` boundary is structurally plausible, but the plan's turn-signal delivery mechanism rides on the notify/hook contract above, which is unverified and appears to have changed shape entirely (notify -> hooks).

## Follow-up

Filed as a follow-up rather than guessed at: **"codex adapter: parse rollout events and map notify payload once codex authentication and the real hooks-notify contract are available."** Two independent blockers must clear before Task A10 can be attempted for real: (1) authenticated access to run a real `codex exec` turn (to observe assistant/tool-call/tool-result/token-usage shapes), and (2) understanding of the replacement `hooks` mechanism (`--dangerously-bypass-hook-trust`, `features list` shows `hooks: stable`) since the plan's assumed `notify = [...]` config array does not exist in this codex version.

**Addendum (2026-08-22, approval-policy flag, harness/model parity round):**
verified against the actually-installed `codex-cli 0.147.0` at
`~\AppData\Local\Programs\OpenAI\Codex\bin` (a real standalone install, not
npm or brew -- `codex.exe --version`), both `codex --help` (top-level,
interactive launch) and `codex exec --help` (headless launch) were captured
in full. Both now show:

```
  -a, --ask-for-approval <APPROVAL_POLICY>
          Configure when the model requires human approval before executing a command

          Possible values:
          - untrusted:  Only run "trusted" commands (e.g. ls, cat, sed) without asking for user approval. Will escalate to the user if the model proposes a command
            that is not in the "trusted" set
          - on-request: The model decides when to ask the user for approval
          - never:      Never ask for user approval Execution failures are immediately returned to the model
```

This flag is **not** in this file's original 2026-07-31 verbatim capture of
`codex exec --help` (0.146.0, brew) above, and the notes for that capture
explicitly enumerate every flag it *did* find -- `-a`/`--ask-for-approval` is
absent from that list, not merely unquoted. So this flag postdates 0.146.0;
treat this addendum, not the original block, as authoritative for its
existence and exact values. (The top-level `codex --help` also gained
`--approve-for-me` and `--remote`/`--remote-auth-token-env` since the
original capture; recorded here for completeness, not used by any adapter
code.) `-s, --sandbox <read-only|workspace-write|danger-full-access>` and
`-m, --model <MODEL>` are unchanged and still present identically on both
subcommands.

**Why this matters:** `-s, --sandbox` alone does not stop codex from
*asking* -- it only scopes what an executed command may touch. A command the
sandbox would refuse still triggers the `untrusted` policy's own "escalate
to the user" behaviour first (per the value's own description above) unless
`--ask-for-approval never` is also set. `CodexAdapter::policy_args`
(`src/commands/ctx/adapters/codex.rs`) pins `--sandbox read-only
--ask-for-approval never` together for exactly this reason when zirv's own
`[policy]` denies `shell_exec`/`repo_fs_write` for a launch -- `never` here
only suppresses the *prompt*, it does not widen what the sandbox allows, and
is not `--dangerously-bypass-approvals-and-sandbox` (the one flag verified to
remove sandboxing entirely, never emitted by this codebase).
