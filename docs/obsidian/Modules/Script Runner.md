---
last-verified: 2026-09-06
---

# Script Runner

## Quick Reference

- **Files:** `src/script_runner/mod.rs`, `script.rs`, `command.rs`, `command_types.rs`, `options.rs`, `agent_command.rs`, `fallback_command.rs`, `operating_system.rs`, `secret.rs`
- **Used by:** [[Built-in Commands]] (`main.rs` calls `script_runner::execute`)
- **Depends on:** [[Utilities]] (`utils::file_to_script` parses a file into a `Script` before `execute` ever runs), [[Ctx Subsystem]] and [[Ctx Supervisors]] (an `Agent` step drives `zirv ctx exec`'s own entry point in-process)
- **Tests:** inline `#[cfg(test)] mod tests` in every file listed above (e.g. `script_runner::mod::tests`, `command_types::tests`, `agent_command::tests`)
- **If changed:** [[Script Files]], [[Script Resolution]], [[Built-in Commands]], [[Ctx Adapters]]
- **Gotchas:** `CommandTypes` is deserialized by hand (not serde's `untagged`) so a step's error names the missing/misspelled key; `${var}` left unresolved after substitution is a hard error, not a silent pass-through; an `Agent` step is validated at *load* time, and `--dry-run` runs `CommandTypes::check` per step, so `--dry-run` and a real run reject the same scripts; `Options` is `deny_unknown_fields` (a mistyped option key is an error, not a silently ignored filter) while `Command`/`AgentCommand`/`Script` stay permissive. `substitute()`/`check_unresolved()` operate over the *template* in a single pass — a param/secret/capture value that itself contains `${...}`-shaped text is never re-scanned for further placeholders and never misreported as unresolved (fixed 2026-09-05, was hash-order dependent and could splice a secret's own value into a command line).

## Purpose

Executes a parsed `Script` — the runtime for everything under `.zirv/commands/` (and `~/.zirv/commands/`; see [[Script Resolution]] for where a script file is found). Builds a substitution context from CLI parameters and secrets, then runs each step in order: a shell command, a supervised AI-agent task, or a batch of commands opened in separate terminal windows.

## How It Works

### Data model (`script.rs`)

`Script { name, description, params: Option<Vec<String>>, secrets: Option<Vec<Secret>>, commands: Vec<CommandTypes> }`. `params` entries ending in `?` are optional; `commands` deserializes through `command_types::deserialize_steps`, which numbers each step so a bad one reports "step N: ...". `Script::run` walks `commands` in order, printing `crate::output::step`/`dry_run` framing for each, and stops at the first error (wrapped with the step index, total, and script name). Under `--dry-run` each step is also passed through `CommandTypes::check(context)` — `check_unresolved` over a `Command`, every entry of a `Commands` block, and an `Agent` step's prompt — so a `${var}` the context cannot resolve fails the dry run exactly as it fails a real run (fixed 2026-09-06; `--dry-run` previously printed `echo ${missing}` and exited 0). It is the run-time counterpart to `AgentCommand::validate`'s load-time checks: same principle, applied to the one thing that needs the resolved context. Review round 1 (2026-09-06) closed the mirror-image gap that check opened: only `execute` registers a `capture:` variable, so a dry run rejected every later step naming one — refusing a script the real run completes. After a step's `check` passes, `--dry-run` registers that step's `CommandTypes::captured_var()` into the context as the visible stand-in `<capture:NAME>` (never a plausible-looking value; a dry run must not appear to know what it cannot). Only a plain `Command` capture is registered: an agent step rejects `capture` at load time, and a concurrent block spawns a window it never reads back.

### Context building (`mod.rs`)

`execute(script, params, dry_run)` calls `build_context` then `script.run`. `build_context`:
- Validates `params` ordering (all optional after all required), rejects duplicate names, and checks the CLI arg count falls in `[required_count, total_count]`.
- Maps each positional CLI arg to its param name (stripping the `?` suffix), missing optional args default to `""`.
- Resolves each `secrets` entry from its named environment variable, hard-erroring if absent.

The result is a flat `HashMap<String, String>` that every step's `${var}` substitution reads and writes (e.g. `capture`, `cd`, an agent's own `cwd`).

### Step dispatch (`command_types.rs`)

`CommandTypes` has three variants: `Command`, `Commands(Vec<Command>)`, `Agent`. Parsing dispatches on which key a step's mapping has (`command` vs `agent`), rather than serde's `untagged` fallback, because untagged silently picks the first variant that fits and reports only "data did not match any variant" — a step with both `command` and `agent` used to run as a shell command and threw the agent half away with no warning. `Commands` (a plain YAML sequence of command strings) is the "concurrent commands" feature: at *load* time it hard-errors if any entry carries `capture`, `options.fallback`, or `options.interactive` — none of the three can be honored once a command runs detached inside its own terminal window (`validate_concurrent_block`, same load-time-not-run-time treatment as `AgentCommand::validate`). At run time, `build_concurrent_command` first drops any entry whose `options.operating_system` filters out the current platform (the block skips entirely, printing "Command skipped due to OS filter", if every entry is filtered out), then substitutes `${var}` in what's left through the same `command::check_unresolved` + `command::substitute` pair a single `Command` step uses (fixed 2026-09-06; the block kept its own hash-ordered replace loop plus a re-scan of the already-substituted text, so a value containing `${...}`-shaped text was either re-expanded — splicing a secret into the spawned window's command line — or falsely reported unresolved, depending on map order), joins with `&&`, and spawns a *new terminal window* — `cmd /K` on Windows, an AppleScript `Terminal` `do script` on macOS, and the first of `gnome-terminal`/`x-terminal-emulator`/`xterm` on Linux (fails clearly if no `DISPLAY`/`WAYLAND_DISPLAY`, i.e. a headless/SSH session).

### Single command execution (`command.rs`)

`Command { command, capture, description, options }`. `execute`:
- Skips (with a message, not an error) when `options.operating_system` doesn't match the current OS.
- Substitutes `${var}` via the shared `substitute()`, then hard-errors on any placeholder still present via `check_unresolved()` — both are `pub(crate)` and reused by `AgentCommand`. Both operate over the *template* (`self.command`) in one `Regex::replace_all` pass: `substitute()` never rescans a substituted-in value for further `${...}` text, and `check_unresolved()` checks the template's own placeholder names against the context directly instead of re-scanning the substituted output — so a captured/param value that happens to contain `${...}`-shaped text (e.g. a literal secret placeholder) is neither expanded a second time nor misreported as an unresolved placeholder the template never actually left open.
- Special-cases a leading `cd `: only when the rest of the line is a *bare single-argument* directory (`bare_cd_target` — one whitespace-free token, or a token wrapped in one pair of matching quotes with the quotes stripped) does it update the context's `cwd` key (canonicalized) instead of spawning a process; `cd dir && next` and `cd /d C:\path` don't match and fall through to the real shell instead of hard-failing on a literal-string canonicalize.
- Otherwise spawns via `powershell -Command` (Windows) or `sh -c` (Unix) through Tokio's async `Command`, honoring `cwd` from the context and `options.interactive` (inherits stdio).
- `capture` stores trimmed stdout into the context under that variable name instead of streaming it to the terminal.
- On failure: runs any `options.fallback` commands in order, then respects `options.proceed_on_failure` (converts the failure — main command, or main command plus a fallback that also failed — to a skip message) before finally erroring. `proceed_on_failure` applies even when the fallback itself also fails; it used to short-circuit with a hard error before `proceed_on_failure` was ever consulted.
- `options.delay_ms` sleeps after a successful run.

### Options (`options.rs`, `fallback_command.rs`, `operating_system.rs`, `secret.rs`)

`Options { proceed_on_failure, delay_ms, interactive, operating_system, fallback }`. `operating_system` accepts the legacy `os` key as a serde alias (the README once documented that name; it used to be silently ignored as an unknown key). `Options` is `#[serde(deny_unknown_fields)]`: it is a closed key set, so a typo like `operatingsystem: linux` is a named parse error rather than a step that silently runs on every platform (since 2026-09-06). This applies to `Options` **only** — `Command`, `AgentCommand` and `Script` stay permissive, since existing user scripts may carry extra keys on them. `skip_for_os()` is the shared "does this filter exclude the current platform" check used by both `Command` and `AgentCommand`. `FallbackCommand` is a smaller sibling of `Command` run when the main command's `invoke` fails — it now takes the script's own context and substitutes `${var}` in its command, honors the tracked `cwd`, `options.operating_system` (skipped, not run, when filtered), `options.proceed_on_failure` (a failing fallback with this set is itself treated as success), and `options.delay_ms`; it has no `capture` field of its own. `OperatingSystem` is a three-value enum (`Linux`/`Windows`/`MacOS`) matched against `std::env::consts::OS`. `Secret { name, env_var }` is the params-file declaration resolved during context building.

### Agent steps (`agent_command.rs`)

`AgentCommand { agent, prompt, flags, description, options, capture }` runs a *supervised* AI-agent task in-process, through the exact same entry point `zirv ctx exec` uses — pacing against usage windows, rot detection, and restart-with-handoff. `capture` and `options.interactive` are declared only so misusing them produces a named error instead of being silently ignored; both are rejected by `validate()`, which also rejects an empty prompt, an unknown `agent` name, and any `flags` entry that doesn't start with `-` (a bare leading word would be read as the launched program). Validation runs at parse time (inside `CommandTypes::from_value`), not at execution time, so `--dry-run` rejects exactly what a real run would.

`execute` substitutes the prompt, then calls `invoke`, which moves everything onto a blocking thread via `tokio::task::spawn_blocking` — `run_supervised` (and the exec supervisor it calls) spawns child processes and sleeps synchronously, so it must not run on the async executor. `run_supervised` loads `CtxConfig`, selects the adapter (surfacing an unready or disabled adapter's own error before any supervision starts), builds an `ExecArgs` with the prompt carried as *data* (not encoded into argv, so a prompt shaped like a flag can't be misread as one), and calls `ctx::exec::run_with` directly. A non-zero exit is decoded by `ctx::exec::describe_exit` (defined in `exec.rs` itself, not here, alongside `EXIT_ROT_EXHAUSTED`/`EXIT_TIMEOUT`; `zirv ctx agent` (`agent.rs`) shares the same function for the same reason): the supervisor's own two exit codes read as "the session kept rotting" / "hit its wall-clock timeout" rather than a generic agent failure.

```mermaid
flowchart LR
    A[main.rs] --> B[script_runner::execute]
    B --> C[build_context: params + secrets]
    C --> D[Script::run loop]
    D --> E{CommandTypes}
    E -->|Command| F[shell exec, ${var} substitution, capture/fallback]
    E -->|Commands| G[spawn terminal window per OS]
    E -->|Agent| H[spawn_blocking: ctx exec::run_with]
    H --> I[Ctx Supervisors: pacing, rot, restart+handoff]
```
