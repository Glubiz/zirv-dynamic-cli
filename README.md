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
  - [Running Scripts](#running-scripts)
  - [Passing Parameters & Secrets](#passing-parameters--secrets)
  - [Capture Output](#capture-output)
  - [Failure Hooks](#failure-hooks)
  - [Chaining Scripts](#chaining-scripts)
- [Configuration](#configuration)
  - [Directory Structure](#directory-structure)
  - [Schema Examples](#schema-examples)
- [Shortcuts](#shortcuts)
- [Context Management (zirv ctx)](#context-management-zirv-ctx)
- [Supported Platforms](#supported-platforms)
- [Contribution](#contribution)
- [License](#license)
- [Contact](#contact)

---

## Features

- **YAML-Driven Scripts**: Define commands in `.zirv/` files with metadata (name, description, params, secrets).  
- **Capture Output**: Use `capture: var_name` on any step to grab its stdout into `${var_name}` for later substitution.  
- **Failure Hooks**: On a step failure you can declare an `fallback` sub-chain of commands, then retry the original step once.  
- **Flexible Options**: Interactive mode, OS filters, `proceed_on_failure`, delays, and secret support.  
- **Multi-Format**: Supports YAML, JSON, and TOML, extendable.  
- **Cross-Platform**: Compatible with Windows, macOS, and Linux.

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

### Initialize a Project

Run:
```bash
zirv init
```
Creates a `.zirv/` directory with a sample script. This directory is where you will define your scripts. The `.zirv/` directory is created in the current working directory or in the HOME directory depending on the commandline interactions.

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
      proceed_on_failure: true     # continue even if retry also fails
      fallback:
        - command: "echo 'Fallback action'"
```

This will execute the fallback command if the first command fails. The original command will be retried once.

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

The built-in `cd` command updates the working directory for any following
commands in the same window, allowing scripts like:

```yaml
commands:
  - - command: "cd backend"
    - command: "cargo run"
```

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
      os: linux
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
        "os": "linux",
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
options.os = "linux"
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
  c: commit.yaml
```
Run zirv b instead of zirv build.yaml.
This will execute the `build.yaml` script.

## Context Management (zirv ctx)

`zirv ctx` watches AI coding agent sessions (Claude Code, Codex) for context rot
and intervenes before quality drops: it advises, compacts early, or restarts the
session with a distilled handoff. Scoring is deterministic, and every decision is
logged.

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
Without the marker signal (Codex, or Claude without the prompt hook) behavioral
signals top out at 70, so a restart there comes only from the token ceiling.

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
```

Handoffs, sockets and logs live in the platform state directory under
`zirv/ctx/`, never in the repo. Override with `ZIRV_CTX_STATE_DIR`. See
[Usage pacing](#usage-pacing) below for the `[pace]` table that governs
subscription-window waiting.

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
stop. For Codex, point the `notify` program in `~/.codex/config.toml` at
`zirv ctx hook notify`.

### Interactive use

```bash
alias claude='zirv ctx wrap -- claude'
```

The wrapped session is byte-for-byte identical to an unwrapped one until an
intervention, injection happens only at a turn boundary while you are idle, and
any supervision failure drops it back to pure passthrough.

### Exit codes for headless supervision

| Code | Meaning |
|---|---|
| the child's own code | the run finished on its own |
| `75` | the restart budget was spent, or the loop hit its failure cap |
| `76` | a wall-clock timeout with no restarts left |

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

## Supported Platforms
- Windows
- macOS
- Linux

Commands can target specific operating systems using the `os` option in the script configuration.
- `windows`: Windows OS
- `linux`: Linux OS
- `macos`: macOS

## Contribution
Contributions are welcome! Please fork the repository and submit a pull request with your changes. For major changes, please open an issue first to discuss what you would like to change.

## License
Licensed under the [MIT License](LICENSE).

## Contact
Tweet [@Glubiz](https://twitter.com/Glubiz)