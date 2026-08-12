---
last-verified: 2026-08-12
---

# Script Files

> [!tip] Quick Reference
> - A script is a `name`, optional `description`, optional `params`, optional `secrets`, and a required `commands` list, expressible in YAML, JSON, or TOML.
> - Each entry in `commands` is one of three kinds, dispatched by which key it carries: a plain `command` (shell), a nested list of `command`s (a concurrent block opened in a new terminal), or an `agent` + `prompt` (a supervised AI-agent step).
> - `${var}` substitution pulls from a context built out of params, secrets, and any `capture`d output; a `cd <dir>` command is special-cased to update the context's `cwd` rather than spawning a subprocess.
> - Cross-links: parsing and the run loop live in [[Script Runner]]; where a script file is found lives in [[Script Resolution]]; shortcut keys that point at a script live in [[Shortcuts]]; agent steps hand off to the same machinery as [[Ctx Supervisors]] and [[Ctx Adapters]].

> [!warning] If changed
> Update [[Script Runner]] (the model these pages describe lives in `src/script_runner/`) and, if `AgentCommand` semantics change, [[Ctx Adapters]].

## Top-level fields

```yaml
name: "Commit Changes"              # required, shown by `zirv help`
description: "Commits with a message"   # optional
params:                              # optional, ordered
  - "commit_message"
  - "optional_note?"                 # trailing ? = optional
secrets:                             # optional
  - name: "commit_password"
    env_var: "COMMIT_PASSWORD"
commands: [...]                      # required
```

- **`params`**: positional CLI arguments, mapped by position onto these names. A trailing `?` marks a parameter optional; all optional params must come after all required ones, and duplicate names (after stripping `?`) are rejected at run time. An omitted optional parameter's value is the empty string.
- **`secrets`**: each entry pulls `env_var` from the process environment and inserts it into the context under `name`; a missing environment variable is a hard error naming both the secret name and the variable. There is no separate secret-masking — the value is substituted into command strings like any other context variable, so it can appear in `--dry-run` output or process listings.
- **`commands`**: the ordered list of steps described below. Steps are executed in order except concurrent blocks (see below), which spawn and do not block the script.

## Command steps

Each list entry is dispatched by which key it has — `command`, a nested list, or `agent` — not by an untagged-enum guess, so a step naming both `command` and `agent` is a load-time error rather than silently picking one.

### Shell command

```yaml
- command: cargo fmt
  description: "Format the code"      # optional
  capture: fmt_output                 # optional: store trimmed stdout as ${fmt_output}
  options:
    proceed_on_failure: false
    interactive: false
    operating_system: linux           # alias: os
    delay_ms: 500
    fallback:
      - command: echo "fmt failed, continuing anyway"
```

Runs via `powershell -Command` on Windows or `sh -c` elsewhere. `${key}` placeholders in `command` are substituted from the current context before execution; any `${...}` still present afterward (a typo, or a param that doesn't exist) is a hard error naming the unresolved key(s).

**Per-command options** (`script_runner/options.rs`):

| Option | Effect |
|---|---|
| `proceed_on_failure` | if `true`, a failing command doesn't stop the script |
| `delay_ms` | sleep this many milliseconds after the command succeeds |
| `interactive` | inherit stdin/stdout/stderr instead of capturing them |
| `operating_system` (alias `os`) | skip the step entirely unless it matches the current OS (`linux`/`windows`/`macos`) |
| `fallback` | a list of commands run if the main command fails; if a fallback also fails, the error names both; `proceed_on_failure` still applies after fallback runs |

`cd <dir>` is intercepted specially: rather than spawning a subprocess (whose directory change wouldn't outlive it), it resolves `<dir>` against the context's current `cwd` (or the process's actual cwd if none is set yet), canonicalizes it, and stores the result back into `cwd` for every subsequent step.

### Concurrent block

```yaml
commands:
  - - command: cd src
    - command: ls -a
  - - command: cd scripts
    - command: ls -a
```

A list-of-lists entry (`CommandTypes::Commands`) joins its inner commands with `&&` and opens them in a new terminal window (`cmd /K` on Windows, `osascript`/Terminal.app on macOS, `gnome-terminal`/`x-terminal-emulator`/`xterm` on Linux) rather than running inline — it requires a desktop/GUI session and errors clearly when none is available (e.g. over SSH). See `.zirv/test-concurrentcy.yaml` in this repo for a minimal example.

### Agent step

```yaml
- agent: claude
  prompt: "Fix the failing tests in ${dir}"
  flags: ["--model", "sonnet"]
  description: "Let Claude fix the tests"
```

Runs a supervised AI-agent task in-process through the same machinery `zirv ctx exec` uses (pacing, rot detection, automatic restart with handoff). `capture` and `options.interactive` are rejected for agent steps at load time — the same time `--dry-run` validation runs, so a dry run never reports success for a script that could never execute. See [[Ctx Adapters]] and [[Ctx Supervisors]] for what actually runs underneath.

## Script chaining

There is no dedicated "run another script" step type. Because `command` is just a shell string, one script chains into another by literally invoking `zirv <name>`:

```yaml
# .zirv/commit.yaml (this repo)
commands:
  - command: cargo fmt
  - command: zirv t          # re-invokes .zirv/test.yaml (shortcut "t") as a fresh zirv process
    options:
      proceed_on_failure: false
  - command: git add .
  - command: git commit -m "${commit_message}"
  - command: git push origin
```

This goes through the ordinary [[Script Resolution]] path again, including shortcut lookup — the chained call has no special status.

## Annotated example

```yaml
name: "Test Params"
description: "Demonstrates parameter substitution"
params:
  - "test_param1"
  - "test_param2"
commands:
  - command: echo ${test_param1}
    description: "Prints the first test parameter"
    options:
      proceed_on_failure: false
  - command: echo Combined: ${test_param1} and ${test_param2}
    description: "Prints both parameters combined"
```

`zirv test-params foo bar` sets `test_param1=foo`, `test_param2=bar` in the context before the first step runs.

Supported file formats are YAML (`.yaml`/`.yml`), JSON (`.json`), and TOML (`.toml`) — the same `Script` struct deserializes from all three (`utils::parse_script_content`).
