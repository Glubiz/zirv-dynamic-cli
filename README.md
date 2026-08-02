# Zirv CLI
[![Release](https://img.shields.io/github/v/release/Glubiz/zirv-dynamic-cli)](https://github.com/Glubiz/zirv-dynamic-cli/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

> **Zirv CLI** is a cross-platform command-line interface for developers to automate and streamline workflows with YAML, JSON, or TOML scripts.

---

## Table of Contents

- [Features](#features)
- [Installation](#installation)
- [Upgrading](#upgrading)
- [Usage](#usage)
  - [Initialize a Project](#initialize-a-project)
  - [Creating a New Script](#creating-a-new-script)
  - [Running Scripts](#running-scripts)
  - [Passing Parameters](#passing-parameters)
  - [Optional Parameters](#optional-parameters)
  - [Capture Output](#capture-output)
  - [Failure Hooks](#failure-hooks)
  - [Dry Run](#dry-run)
  - [Chaining Scripts](#chaining-scripts)
- [Configuration](#configuration)
  - [Directory Structure](#directory-structure)
  - [Schema Examples](#schema-examples)
- [Shortcuts](#shortcuts)
- [Reserved Command Names](#reserved-command-names)
- [Context Management (zirv ctx)](#context-management-zirv-ctx)
- [Supported Platforms](#supported-platforms)
- [Contribution](#contribution)
- [License](#license)
- [Contact](#contact)

---

## Features

- **YAML-Driven Scripts**: Define commands in `.zirv/` files with metadata (name, description, params, secrets).  
- **Capture Output**: Use `capture: var_name` on any step to grab its stdout into `${var_name}` for later substitution.  
- **Failure Hooks**: On a step failure you can declare a `fallback` sub-chain of commands to run as a side action; the original command is never retried.  
- **Flexible Options**: Interactive mode, OS filters, `proceed_on_failure`, delays, and secret support.  
- **Multi-Format**: Supports YAML, JSON, and TOML, extendable.  
- **Cross-Platform**: Compatible with Windows, macOS, and Linux.
- **Helpful Errors**: A mistyped script or shortcut name gets up to 3 "did you mean" suggestions instead of a bare failure.

---

## Installation

Choose one of the following methods:

### Homebrew (macOS & Linux)

```bash
brew tap glubiz/homebrew-tap
brew install zirv
```

### Chocolatey (Windows)

```bash
choco install zirv
```

### Install Script (macOS & Linux)

```bash
curl -sSfL https://raw.githubusercontent.com/Glubiz/zirv-dynamic-cli/main/install.sh | sh
```

To install a specific version:

```bash
curl -sSfL https://raw.githubusercontent.com/Glubiz/zirv-dynamic-cli/main/install.sh | sh -s -- 2.5.0
```

### Cargo (All Platforms)

```bash
cargo build --release
# Add `target/release` to your PATH
```

### Precompiled Binaries
Download the latest release from the [GitHub Releases]:
https://github.com/Glubiz/zirv-dynamic-cli/releases

## Upgrading

### Homebrew

```bash
brew upgrade zirv
```

### Chocolatey

```bash
choco upgrade zirv
```

### Install Script

Re-run the install script to get the latest version:

```bash
curl -sSfL https://raw.githubusercontent.com/Glubiz/zirv-dynamic-cli/main/install.sh | sh
```

### Cargo

```bash
cargo build --release
```

## Usage

`zirv help` (also `zirv h`, `zirv --help`, `zirv -h`) lists every available
script and shortcut, local and global.

### Initialize a Project

Run:
```bash
zirv init
```
Creates a `.zirv/` directory with a sample script. This directory is where you will define your scripts. The `.zirv/` directory is created in the current working directory or in the HOME directory depending on the commandline interactions.

### Creating a New Script
```bash
zirv create
```
Interactively asks for the script name, an optional shortcut key, and whether
to create it locally or in the global `~/.zirv` folder, then writes a
template script (and shortcut entry, if given).

To script the creation (e.g. in CI or setup scripts), pass any of the three
answers as flags to skip the corresponding prompt; passing all three skips
every prompt:

```bash
zirv create --name build --shortcut b --global false
```

- `--name <name>` — the script name (file is written as `<name>.yaml`).
- `--shortcut <key>` — a shortcut key, or an empty string for "no shortcut".
- `--global` — create in `~/.zirv` instead of the current directory. Bare
  `--global` means true; pass `--global false` to answer "no" without a prompt.

If the name or shortcut collides with a [reserved command name](#reserved-command-names),
zirv warns and asks for confirmation before creating an unreachable script; in
non-interactive mode (all three flags given) a collision is an error instead,
since there is no prompt to fall back on.

### Running Scripts
Place your script files in `.zirv/` (e.g., `build.yaml`):
  
```yaml
name: Build
description: Build the application.
commands:
  - command: cargo build --release
    options:
      proceed_on_failure: false
  - command: cargo test
    options:
      proceed_on_failure: false
```

Execute the script with:
```bash
zirv build
```

If the name doesn't match any script or shortcut (checked locally in `.zirv/`,
then globally in `~/.zirv/`), zirv suggests up to 3 close matches by edit
distance and points you to `zirv help`:

```
error: No script or shortcut found for 'buld'. Did you mean: build? Run `zirv help` to see available scripts and shortcuts.
```

### Passing Parameters
If a script declares parameters;

```yaml
name: Commit Changes
params:
  - commit_message
commands:
  - command: git add .
  - command: git commit -m "${commit_message}"
  - command: git push origin
```

Run with:
```bash
zirv commit "Your commit message here"
```

### Optional Parameters
A parameter name ending in `?` is optional and resolves to an empty string
when omitted. Optional parameters must be declared after all required ones:

```yaml
name: Greet
params:
  - name
  - greeting?
commands:
  - command: echo "${greeting} ${name}"
```

```bash
zirv greet Alice            # greeting = "" -> prints " Alice"
zirv greet Alice "Welcome"  # greeting = "Welcome" -> prints "Welcome Alice"
```

Declaring an optional parameter before a required one, giving fewer
arguments than there are required parameters, giving more than the total
number of declared parameters, or reusing a parameter name (with or without
the trailing `?`) are all rejected with an error.

### Capture Output
To capture the output of a command, use the `capture` option:

```yaml
name: Capture Test
commands:
  - command: "echo hello"
    capture: greeting
    options:
      proceed_on_failure: false
  - command: "echo Got: ${greeting}"
```

First step stores `hello` in the variable `${greeting}`, which is then used in the second step to print `Got: hello`.

### Failure Hooks
Declare a failure hook for a command using `fallback`:

```yaml
name: OnFailure Demo
commands:
  - command: "sh -c 'exit 1'"
    options:
      proceed_on_failure: true     # don't stop the script once fallback succeeds
      fallback:
        - command: "echo 'Fallback action'"
```

If the command fails, every `fallback` command runs, in order, once — **the
original command is never retried**. If any fallback command itself fails,
the step fails immediately with an error naming both the original and the
failing fallback command. Otherwise, whether the step's own failure stops the
script is controlled separately by `proceed_on_failure`: `true` continues to
the next step, `false` (the default) stops the script with an error, even
though every fallback succeeded.

### Dry Run
Pass `--dry-run` to preview a script without running anything:

```bash
zirv build --dry-run
```

Each step is printed with its `${...}` parameters substituted, instead of
being executed, so you can check what a script would do first.

### Chaining Scripts
You can chain scripts by calling one script from another. For example, if you have a script `build.yaml` and want to call it from `deploy.yaml`:

```yaml
name: Deploy
description: Deploy the application.
commands:
  - command: zirv test
    options:
      proceed_on_failure: false
  - command: zirv build
    options:
      proceed_on_failure: false
```

Run the `deploy` script with:
```bash
zirv deploy
```

### Concurrent Shells
You can open multiple terminals at once by nesting lists. For example:

```yaml
name: Parallel Commands
commands:
  - - command: "echo 'Running Task A'"
    - command: "echo 'Running Task B'"
  - - command: "echo 'Running Task 1'"
    - command: "echo 'Running Task 2'"
```

Each nested list spawns its own shell window.  Every window executes the commands listed in that group and stays open until they finish.

This needs a desktop/GUI session: macOS uses `osascript` to drive Terminal,
Windows opens a new `cmd` window, and Linux tries `gnome-terminal`, then
`x-terminal-emulator`, then `xterm`. Over a headless or SSH-only connection —
or on Linux specifically, whenever neither `DISPLAY` nor `WAYLAND_DISPLAY` is
set — zirv returns a clear error naming what it tried, instead of failing
cryptically or hanging.

The built-in `cd` command updates the working directory for any following
commands in the same window, allowing scripts like:

```yaml
commands:
  - - command: "cd backend"
    - command: "cargo run"
```

### Agent Steps
A command step can run a supervised AI-agent task instead of a shell command,
using `agent` and `prompt` in place of `command`:

```yaml
commands:
  - command: cargo test
  - agent: claude
    prompt: "Fix the failing tests in ${dir}"
    # optional:
    flags: ["--model", "sonnet"]
  - command: cargo test
```

`prompt` gets the same `${var}` substitution as `command`, including the
unresolved-placeholder error if a variable is missing. `flags` are passed
straight through to the agent CLI. `operating_system`, `proceed_on_failure`,
`delay_ms` and `fallback` work the same as they do for a regular command;
`capture` and `interactive` are not supported and fail the step if set.

The step runs in-process through the same supervision `zirv ctx exec` uses:
pacing against your usage windows, rot detection, and automatic restart with a
distilled handoff if the session rots. A non-zero outcome fails the step like
any other command. Only Claude Code is supported today (see [Context
Management](#context-management-zirv-ctx) below); naming any other agent fails
with that adapter's own error.

## Configuration
### Directory Structure
The `.zirv/` directory contains your scripts and a configuration file. The structure is as follows:

```
.zirv/
├── .shortcuts.yaml
├── ...command files
```

### Schema Examples
Supported schemas are YAML, JSON, and TOML. Below are examples of each:

#### YAML Example
```yaml
name: Example Config
description: An example script.
params:
  - param1
commands:
  - command: "echo Welcome, ${user}"
    capture: welcome_msg
    options:
      interactive: false

  - command: echo ${welcome_msg}
    description: Prints greeting
    options:
      interactive: true
      operating_system: linux
      proceed_on_failure: false
      delay_ms: 2000
      fallback:
        - command: "echo 'Attempting fallback...'"
secrets:
  - name: api_key
    env_var: API_KEY
```

#### JSON Example
```json
{
  "name": "Example Config",
  "description": "An example script.",
  "params": ["param1"],
  "commands": [
    {
      "command": "echo Welcome, ${user}",
      "capture": "welcome_msg",
      "options": {
        "interactive": false
      }
    },
    {
      "command": "echo ${welcome_msg}",
      "description": "Prints greeting",
      "options": {
        "interactive": true,
        "operating_system": "linux",
        "proceed_on_failure": false,
        "delay_ms": 2000,
        "fallback": [
          {
            "command": "echo 'Attempting fallback...'"
          }
        ]
      }
    }
  ],
  "secrets": [
    {
      "name": "api_key",
      "env_var": "API_KEY"
    }
  ]
}
```

#### TOML Example
```toml
name = "Example Config"
description = "An example script."
params = ["param1"]

[[commands]]
command = "echo Welcome, ${user}"
capture = "welcome_msg"
options.interactive = false

[[commands]]
command = "echo Token is ${token}"
options.interactive = true
options.operating_system = "linux"
options.proceed_on_failure = false
options.delay_ms = 2000

[[commands.options.fallback]]
command = "echo 'Attempting fallback...'"

[[secrets]]
name = "api"
env_var = "API_KEY"
```

## Shortcuts
Shortcuts are defined in `.shortcuts.yaml` and allow you to create aliases for your scripts. For example:

```yaml
shortcuts:
  b: build.yaml
  t: test.yaml
  cm: commit.yaml
```
Run zirv b instead of zirv build.yaml.
This will execute the `build.yaml` script.

A shortcut key that collides with a [reserved command name](#reserved-command-names)
(for example `c`, already `create`'s alias) can never be reached; `zirv help`
marks it as shadowed in the listing.

## Reserved Command Names

`help`, `version`, `init`, `create`, `ctx`, and their short aliases `h`, `v`,
`i`, `c`, are handled as built-in commands before zirv ever looks in `.zirv/`.
A script file or shortcut key using one of these names can never be invoked:

- `zirv help` lists it but marks it `(shadowed by a built-in command,
  unreachable)`.
- `zirv create` warns about the collision and asks for confirmation before
  creating it anyway; in non-interactive mode (see
  [Creating a New Script](#creating-a-new-script)) the collision is an error
  instead.

## Context Management (zirv ctx)

`zirv ctx` watches Claude Code sessions for context rot and intervenes before
quality drops: it advises, compacts early, or restarts the session with a
distilled handoff. Scoring is deterministic, and every decision is logged.

**Agent support.** Claude Code is the only agent `ctx` supports today. A
`codex` adapter exists in the tree but is not implemented yet: `--agent codex`
fails with a message saying so, because the event shapes it would have to
parse were never verified against an authenticated CLI, and the `notify`
mechanism the design assumed does not exist in current Codex versions.
Progress is tracked in
[issue #11](https://github.com/Glubiz/zirv-dynamic-cli/issues/11).

**Platform support.** Supervision is unix only. `wrap` and `exec` need unix
domain sockets for turn signals, and `wrap` additionally needs raw terminal
mode and the terminal's window size. On Windows those degrade rather than
fail: `exec` falls back to polling the transcript, and `wrap` runs as pure
passthrough with the inner terminal pinned at 80x24. Everything else,
including `score`, `handoff` and `status`, works on all three platforms.

### Verbs

| Command | What it does |
|---|---|
| `zirv ctx score --transcript <path>` | Rot-scores a transcript and prints JSON |
| `zirv ctx loop --prompt <text>` | Runs a fresh headless session per cycle, so the orchestrator cannot rot |
| `zirv ctx exec -- <agent command>` | Supervises one headless run: kill, distill, restart |
| `zirv ctx wrap -- claude` | Supervises an interactive TUI through a PTY |
| `zirv ctx handoff --transcript <path>` | Distills a handoff and stores it |
| `zirv ctx resume` | Starts a clean session with the latest handoff injected |
| `zirv ctx hook <stop\|prompt\|pre-compact\|notify>` | Agent hook entrypoints |
| `zirv ctx status` | Shows supervised sessions, recent decisions and handoffs |
| `zirv ctx usage` | Shows usage-window state, or `usage tee` to collect it from the statusline |
| `zirv ctx optimize` | Reports redundancy, contradictions and dead references in the files that steer your sessions |

### Signals and verdicts

Four signals over the trailing window (default 10 turns):

1. **Context size** (a gate, not a vote). Below 100000 tokens the verdict is always
   `healthy`; at or above 160000 it is at least `compact`.
2. **Tool-failure rate** (weight 40).
3. **Repetition loops**, three or more identical tool calls with identical input
   (weight 30).
4. **Reply-marker misses** on final answers (weight 30, active only when the
   marker hook is installed and the session is at least 10 turns old).

Verdicts: score 40 or more is `advise`, 60 or more is `compact`, 80 or more is
`restart`. At the token ceiling a score of 60 or more escalates to `restart`.
Without the marker signal (Claude without the prompt hook, or any agent that
cannot carry one) behavioral signals top out at 70, so a restart there comes
only from the token ceiling.

### Configuration

Layered, lowest priority first: `~/.zirv/ctx.toml`, then `<repo>/.zirv/ctx.toml`,
then `ZIRV_CTX_*` environment variables, then flags.

```toml
# .zirv/ctx.toml
agent = "claude"

[score]
window = 10
min_turns = 10
token_floor = 100000
token_ceiling = 160000
marker = "[zirv]"
advise_at = 40
compact_at = 60
restart_at = 80

[wrap]
debounce_ms = 3000
inject_timeout_ms = 20000

[supervise]
max_restarts = 2
interval_secs = 900
max_cycle_secs = 3600
max_failures = 5

[handoff]
model = "haiku"
tail_items = 5
timeout_secs = 30   # the distiller is given this long before the structural
                    # fallback is used instead
```

Handoffs, sockets, logs and scoring checkpoints live in the platform state
directory under `zirv/ctx/`, never in the repo. Override with
`ZIRV_CTX_STATE_DIR`. On unix the state directory is created `0700` and its
files `0600`: it holds transcript paths, prompts and distilled handoffs. The
Stop hook is a fresh process on every turn, so it leaves its parse position and
the scoring state derived from it in `scoring/`, which is what keeps per-turn
scoring proportional to the turn rather than to the whole session. Any doubt
about a checkpoint -- a rewritten or truncated transcript, changed scoring
config, an unreadable file -- silently rebuilds it from a full parse. See
[Usage pacing](#usage-pacing) below for the `[pace]` table that governs
subscription-window waiting.

#### Trust boundary

A repository config is part of a checkout, so cloning a repository must not be
enough to change what zirv executes. `<repo>/.zirv/ctx.toml` may not set
`agent_bin`, `supervise.on_failure` or `handoff.model`; doing so is an error
that names the key. Set those in `~/.zirv/ctx.toml`, or with
`ZIRV_CTX_AGENT_BIN`, `ZIRV_CTX_ON_FAILURE` and `ZIRV_CTX_MODEL`, which come
from the operator rather than the checkout. Everything else, including `agent`
(which chooses between built-in adapters rather than naming an executable) and
every threshold, is still repo-configurable.

#### Environment variables worth knowing

| Variable | Effect |
|---|---|
| `ZIRV_CTX_STATE_DIR` | Where handoffs, sockets, logs and usage state live |
| `ZIRV_CTX_TRANSCRIPT` | Pins the transcript `wrap` watches, overriding what the turn signal reports (see below) |
| `ZIRV_CTX_SOCKET`, `ZIRV_CTX_SESSION` | Exported into the supervised agent so its hook can find the supervisor. Set by zirv, not by you |

Every `[section] key` in the table above also has a `ZIRV_CTX_*` variable; the
names follow the key, for example `ZIRV_CTX_DEBOUNCE_MS` for `wrap.debounce_ms`.

### Hook registration (Claude Code)

Add to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "Stop": [{ "hooks": [{ "type": "command", "command": "zirv ctx hook stop" }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "zirv ctx hook prompt" }] }],
    "PreCompact": [{ "hooks": [{ "type": "command", "command": "zirv ctx hook pre-compact" }] }]
  }
}
```

The Stop hook forwards verdicts to a supervising `wrap` or `exec` when one owns
the session, and otherwise prints a non-blocking advisory. It never blocks a
stop, and it exits 0 even when it is invoked wrongly, because Claude Code reads
a Stop hook's exit 2 as "block the stop".

The Stop hook is also how a supervisor learns which file the agent is writing:
the agent mints its own session id, so the transcript path travels on the turn
signal the hook sends. Register it, or `wrap` has nothing to verify a
compaction against and no context to distil a restart handoff from.

There is a `zirv ctx hook notify` entry point intended for Codex, but no
supported way to reach it: current Codex versions have no `notify` program
setting. Do not wire anything to it yet, and see
[issue #11](https://github.com/Glubiz/zirv-dynamic-cli/issues/11).

### Interactive use

```bash
alias claude='zirv ctx wrap -- claude'
```

The wrapped session is byte-for-byte identical to an unwrapped one until an
intervention, injection happens only at a turn boundary while you are idle, and
any supervision failure drops it back to pure passthrough. Flags you wrap are
kept across a restart, so `zirv ctx wrap -- claude --model opus` comes back as
an opus session.

`wrap` learns the transcript path from the turn signals the Stop hook sends,
and forgets it on a restart because the fresh session writes a new file. Set
`ZIRV_CTX_TRANSCRIPT` to pin a path instead; it outranks every signal and
survives restarts, which is what you want when the agent's hook cannot report
one, and what tests use.

### Headless supervision

```bash
zirv ctx exec --prompt "$PROMPT" -- claude -p "$PROMPT" --session-id "$SID"
```

`--prompt` is what a restart re-sends; without it (and without a `-p`,
`--print` or `exec` argument that `zirv` can read the prompt out of), a rot
verdict ends the run instead of restarting it. `--session-id` names the
session, and `--transcript` points at the first child's transcript when the
adapter cannot derive it. Both describe the first child only: every restart is
a new session whose transcript path is derived again.

### Exit codes for headless supervision

| Code | Meaning |
|---|---|
| the child's own code | the run finished on its own |
| `75` | rot was detected and `exec` could not carry on, either because the restart budget was spent or because no prompt was available to restart with. `loop` also returns it when consecutive cycle failures hit `max_failures` |
| `76` | the same, for a wall-clock timeout rather than rot |

The code names the reason, not which limit ran out: `75` means rot and `76`
means timeout, whether the run stopped because the budget was exhausted or
because there was no prompt to restart with. A usage-limit hit is neither, it
parks and relaunches without consuming the restart budget.

### Migrating an existing loop

Replace a long-lived orchestrator session with a stateless loop, and wrap worker
dispatch so individual runs get restarted rather than merely killed:

```yaml
# .zirv/issue-loop.yaml
name: Issue Loop
commands:
  - command: zirv ctx loop --prompt-file .zirv/issue-loop-prompt.md --interval-secs 900
```

```bash
zirv ctx exec --prompt "$WORKER_PROMPT" -- claude -p "$WORKER_PROMPT" --session-id "$SID"
```

Durable state must live outside the session (GitHub issues and labels, for
example), because every cycle starts with a clean context. Once `zirv ctx hook
stop` is registered, remove any older canary Stop hook from
`~/.claude/settings.json`: two Stop hooks scoring the same session is noise, and
the older one blocks stops, which this one deliberately never does.

### Usage pacing

Long autonomous runs die if a subscription window (5 hour rolling, 7 day) runs
dry mid-task. `zirv ctx loop` and `zirv ctx exec` consult a pacing gate before
every spawn and every restart, and wait instead of exiting when a window is at
or above `pace.max_percent` (default 99).

Three data layers, best available wins:

1. **Collector**, server-authoritative. Claude Code's statusline input carries
   `rate_limits.five_hour` and `rate_limits.seven_day` for Pro and Max sessions
   after the first response. Wire your statusline through the tee and every live
   session keeps machine-wide state fresh:

   ```json
   {
     "statusLine": {
       "type": "command",
       "command": "zirv ctx usage tee -- bash ~/.claude/statusline-command.sh"
     }
   }
   ```

   The tee records the fields, then runs your original command unchanged. It
   always exits 0 and always prints a statusline, so a failure here can never
   leave you looking at a blank one.

2. **Estimator**, an approximation. When no fresh collector reading exists, zirv
   sums token usage across local transcripts (including subagent files) over the
   trailing window. It is off until you set a budget, because a plan's real token
   allowance is undocumented and a made-up default would read as data:

   ```toml
   [pace]
   five_hour_budget_tokens = 0   # set to enable the 5h estimate
   seven_day_budget_tokens = 0   # set to enable the 7d estimate
   count_cache_reads = false     # cache reads are discounted, so excluded
   ```

3. **Circuit breaker**, authoritative on trip. If the agent prints a documented
   limit-hit notice, that is treated as 100% no matter what the other layers say:
   the run is parked until the window resets and then relaunched, **without
   consuming the restart budget**.

Full pacing configuration:

```toml
[pace]
enabled = true
max_percent = 99.0
collector_max_age_secs = 900
estimator = true
jitter_secs = 30
fallback_delay_secs = 900    # used when a window's reset time is unknown
wait_slack_secs = 3600       # head room added to the window's own length
# max_wait_secs = 7200       # optional absolute override, see below
```

#### How long a pause can last

The wait is bounded per window, not by one global clock: at most the window's own
length plus `wait_slack_secs`, so a five-hour trip is bounded near six hours and
a seven-day trip is allowed to wait out the week. That distinction matters,
because resuming a seven-day window every few hours would spend tokens against a
window that has not reset, which is exactly what pacing exists to prevent.

When a window's reset time is known and lands inside that bound, the pause ends
at the reset (plus jitter) and not before. Set `max_wait_secs` only if you would
rather a supervisor give up waiting and proceed after a fixed time; it replaces
the per-window bound entirely and is unset by default.

A pause is announced once, not once per check, and appears in the decision log as
a single `pace-wait` entry. Parks and relaunches are logged too. Check the
current picture, including how fresh each reading is, with `zirv ctx usage`.

### Reviewing your instruction files

`zirv ctx optimize` reads the CLAUDE.md hierarchy and the settings layers that
steer every session, checks them against recent transcripts and the decision log,
and prints a report with proposed edits as unified diffs.

```bash
zirv ctx optimize              # full analysis, one cheap model call
zirv ctx optimize --no-model   # deterministic checks only, no model call
```

It reports four kinds of finding: instructions stated in more than one layer,
instructions naming files or hook programs that no longer exist, contradictions
between layers, and instruction gaps that correlate with repeated tool failures
or user corrections.

**It never edits an analysed file.** Every proposal is a diff you apply yourself.
A finding about a file inside this repository (CLAUDE.md, `.claude/settings.json`)
gets a repo-relative `a/`/`b/` header, so `git apply` works from the repo root. A
finding about a file outside the repo (your global CLAUDE.md, `~/.claude/settings.json`)
gets a plain absolute path with no `a/`/`b/` prefix instead, meant for hand
application: the report says which is which for every diff it prints. A copy of
each report is kept under the state dir,
and each run appends to the decision log. When a finished session shows a high
tool-failure rate, the Stop hook queues an "optimize recommended" entry and
mentions it once in its advisory; it never runs the analysis itself.

### Consistent sessions

When zirv starts an agent through `wrap`, `exec`, `loop` or `resume` it injects a
small system prompt so sessions behave the same way every time. Three layers
concatenate, in order:

1. A shipped default baked into the binary: respect repo conventions, use tools
   deterministically, report failures honestly.
2. `~/.zirv/system-prompt.md`, your own additions.
3. `<repo>/.zirv/system-prompt.md`, the repository's additions.

The repo layer is **untrusted input**, treated the same way `ctx.toml`'s repo
layer is: it is capped in size, labeled in the composed prompt as coming from the
checkout, and stated not to override anything above it. A repository cannot turn
its own layer on or raise its own cap: `prompt.enabled`, `prompt.repo_layer` and
`prompt.max_repo_bytes` are all rejected from a repo config. Set them in
`~/.zirv/ctx.toml`, or with `ZIRV_CTX_PROMPT`, `ZIRV_CTX_PROMPT_REPO` and
`ZIRV_CTX_PROMPT_MAX_REPO_BYTES`.

```toml
[prompt]
enabled = true
repo_layer = true
max_repo_bytes = 4096
```

Pass `--simple` to any of the four verbs to start the agent with no zirv text at
all, shipped default included. Supervision, pacing and hooks are unaffected.
Whether a prompt was injected, and from which layers, is recorded in the decision
log at every session start.

## Supported Platforms
- Windows (see the platform note under [Context Management](#context-management-zirv-ctx): `zirv ctx` supervision is unix only)
- macOS
- Linux

Commands can target specific operating systems using the `operating_system` option in the script configuration.
- `windows`: Windows OS
- `linux`: Linux OS
- `macos`: macOS

## Contribution
Contributions are welcome! Please fork the repository and submit a pull request with your changes. For major changes, please open an issue first to discuss what you would like to change.

## License
Licensed under the [MIT License](LICENSE).

## Contact
Tweet [@Glubiz](https://twitter.com/Glubiz)