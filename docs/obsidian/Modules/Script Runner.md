---
last-verified: 2026-08-15
---

# Script Runner

## Quick Reference

- **Files:** `src/script_runner/mod.rs`, `script.rs`, `command.rs`, `command_types.rs`, `options.rs`, `agent_command.rs`, `fallback_command.rs`, `operating_system.rs`, `secret.rs`
- **Used by:** [[Built-in Commands]] (`main.rs` calls `script_runner::execute`)
- **Depends on:** [[Utilities]] (`utils::file_to_script` parses a file into a `Script` before `execute` ever runs), [[Ctx Subsystem]] and [[Ctx Supervisors]] (an `Agent` step drives `zirv ctx exec`'s own entry point in-process)
- **Tests:** inline `#[cfg(test)] mod tests` in every file listed above (e.g. `script_runner::mod::tests`, `command_types::tests`, `agent_command::tests`)
- **If changed:** [[Script Files]], [[Script Resolution]], [[Built-in Commands]], [[Ctx Adapters]]
- **Gotchas:** `CommandTypes` is deserialized by hand (not serde's `untagged`) so a step's error names the missing/misspelled key; `${var}` left unresolved after substitution is a hard error, not a silent pass-through; an `Agent` step is validated at *load* time so `--dry-run` and a real run reject the same scripts.

## Purpose

Executes a parsed `Script` — the runtime for everything under `.zirv/`. Builds a substitution context from CLI parameters and secrets, then runs each step in order: a shell command, a supervised AI-agent task, or a batch of commands opened in separate terminal windows.

## How It Works

### Data model (`script.rs`)

`Script { name, description, params: Option<Vec<String>>, secrets: Option<Vec<Secret>>, commands: Vec<CommandTypes> }`. `params` entries ending in `?` are optional; `commands` deserializes through `command_types::deserialize_steps`, which numbers each step so a bad one reports "step N: ...". `Script::run` walks `commands` in order, printing `crate::output::step`/`dry_run` framing for each, and stops at the first error (wrapped with the step index, total, and script name).

### Context building (`mod.rs`)

`execute(script, params, dry_run)` calls `build_context` then `script.run`. `build_context`:
- Validates `params` ordering (all optional after all required), rejects duplicate names, and checks the CLI arg count falls in `[required_count, total_count]`.
- Maps each positional CLI arg to its param name (stripping the `?` suffix), missing optional args default to `""`.
- Resolves each `secrets` entry from its named environment variable, hard-erroring if absent.

The result is a flat `HashMap<String, String>` that every step's `${var}` substitution reads and writes (e.g. `capture`, `cd`, an agent's own `cwd`).

### Step dispatch (`command_types.rs`)

`CommandTypes` has three variants: `Command`, `Commands(Vec<Command>)`, `Agent`. Parsing dispatches on which key a step's mapping has (`command` vs `agent`), rather than serde's `untagged` fallback, because untagged silently picks the first variant that fits and reports only "data did not match any variant" — a step with both `command` and `agent` used to run as a shell command and threw the agent half away with no warning. `Commands` (a plain YAML sequence of command strings) is the "concurrent commands" feature: it substitutes `${var}` in each, joins with `&&`, and spawns a *new terminal window* — `cmd /K` on Windows, an AppleScript `Terminal` `do script` on macOS, and the first of `gnome-terminal`/`x-terminal-emulator`/`xterm` on Linux (fails clearly if no `DISPLAY`/`WAYLAND_DISPLAY`, i.e. a headless/SSH session).

### Single command execution (`command.rs`)

`Command { command, capture, description, options }`. `execute`:
- Skips (with a message, not an error) when `options.operating_system` doesn't match the current OS.
- Substitutes `${var}` via the shared `substitute()`, then hard-errors on any placeholder still present via `check_unresolved()` — both are `pub(crate)` and reused by `AgentCommand`.
- Special-cases a leading `cd `: updates the context's `cwd` key (canonicalized) instead of spawning a process, so subsequent steps in the same script inherit the new working directory.
- Otherwise spawns via `powershell -Command` (Windows) or `sh -c` (Unix) through Tokio's async `Command`, honoring `cwd` from the context and `options.interactive` (inherits stdio).
- `capture` stores trimmed stdout into the context under that variable name instead of streaming it to the terminal.
- On failure: runs any `options.fallback` commands in order (a fallback that also fails is itself an error), then respects `options.proceed_on_failure` (converts failure to a skip message) before finally erroring.
- `options.delay_ms` sleeps after a successful run.

### Options (`options.rs`, `fallback_command.rs`, `operating_system.rs`, `secret.rs`)

`Options { proceed_on_failure, delay_ms, interactive, operating_system, fallback }`. `operating_system` accepts the legacy `os` key as a serde alias (the README once documented that name; it used to be silently ignored as an unknown key). `skip_for_os()` is the shared "does this filter exclude the current platform" check used by both `Command` and `AgentCommand`. `FallbackCommand` is a smaller sibling of `Command` (no capture, no OS filter) run when the main command's `invoke` fails. `OperatingSystem` is a three-value enum (`Linux`/`Windows`/`MacOS`) matched against `std::env::consts::OS`. `Secret { name, env_var }` is the params-file declaration resolved during context building.

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
