# Zirv CLI
[![Release](https://img.shields.io/github/v/release/Glubiz/zirv-dynamic-cli)](https://github.com/Glubiz/zirv-dynamic-cli/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

> **Zirv CLI** is a cross-platform command-line interface for developers to automate and streamline workflows with YAML, JSON, or TOML scripts.

---

## Table of Contents

- [Just Run `zirv`](#just-run-zirv)
  - [AI setup and harness migration](#ai-setup-and-harness-migration)
  - [The dashboard: multiple sessions in one terminal](#the-dashboard-multiple-sessions-in-one-terminal)
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
- [Development Workflows](#development-workflows)
  - [The full verb set](#the-full-verb-set)
  - [Lifecycle and artifacts](#lifecycle-and-artifacts)
  - [Deploy tiers](#deploy-tiers)
  - [Workflow adoption](#workflow-adoption)
  - [Agent registry](#agent-registry)
  - [Maintain loop](#maintain-loop)
  - [Frontend quality](#frontend-quality)
- [Context Management (zirv ctx)](#context-management-zirv-ctx)
  - [Cross-harness fallback and handover](#cross-harness-fallback-and-handover)
  - [Permission auditing and safe-list proposals](#permission-auditing-and-safe-list-proposals-issue-178)
- [Supported Platforms](#supported-platforms)
- [Contribution](#contribution)
- [License](#license)
- [Contact](#contact)

---

## Just Run `zirv`

The fastest way to start a session: run `zirv` with no arguments, in a
zirv-managed repo (one with a **local** `.zirv/` directory), from a real
terminal.

```bash
cd my-project
zirv
```

| Situation | Result |
|---|---|
| a local `./.zirv` exists and both stdin and stdout are a real terminal | starts `zirv ctx chat` — an interactive orchestrator session |
| No local `.zirv`, or stdin/stdout is piped or redirected | shows this same `zirv help` listing, exit 0 |
| `zirv --help` / `zirv -h` | always shows help, even with nothing else on the command line |

This is a deliberate behavior change: before, a bare `zirv` was a clap usage
error (missing the required `command` argument, exit 2). A **global**
`~/.zirv` alone does not count — only a local `./.zirv` says "this directory
is zirv-managed" — and **both** stdin and stdout have to be a real terminal:
piped stdin (`echo hi | zirv`, a CI job) or a redirected stdout (`zirv |
less`) always falls back to help instead, so a bare invocation never blocks
waiting on a chat session, or opens one into a pipe, when nothing interactive
is on the other end.

### AI setup and harness migration

Run the guided setup when moving an existing Claude Code or Codex repository
to Zirv:

```bash
zirv setup
```

The non-interactive equivalent is `zirv setup apply`. It initializes `.zirv/`,
migrates root `CLAUDE.md`/`AGENTS.md` instructions into the canonical
`.zirv/context/` layer without deleting or overwriting the native files,
bootstraps a small shared memory bank, and merges Zirv's Stop,
UserPromptSubmit, PreCompact, and guarded PreToolUse hooks into Claude's
existing `settings.json` and Codex's existing `hooks.json`. Unrelated hooks
are preserved and both files are backed up before modification. Codex asks you
to review new hooks with `/hooks`. An existing Claude statusline is preserved;
when none is configured, setup installs `zirv ctx usage tee`.

```bash
zirv setup status
zirv setup status --json
zirv setup apply --dry-run
zirv setup apply --memory-source /path/to/docs-or-obsidian-vault
```

Canonical common context reaches both Claude and Codex; optional
`.zirv/context/claude.md` and `.zirv/context/codex.md` additions apply only to
that harness. Direct Codex launches receive the compiled prompt through the
CLI's per-run `developer_instructions` config override. Windows shell-shim
launches remain fail-closed: Zirv never puts repository-authored prompt text on
an argv that `cmd.exe` or PowerShell would reparse.

AI-specific settings can be factory-reset separately from Zirv setup. Reset is
refused without `--yes`, supports `--dry-run`, backs up every exact target with
a manifest under `.zirv/backups/ai-reset/` (project) or
`~/.zirv/backups/ai-reset/` (global), and preserves authentication,
sessions/history, and caches unless `--include-auth` is explicitly passed:

```bash
zirv setup reset claude --scope project --dry-run
zirv setup reset codex --scope global --yes
zirv setup reset all --scope all --yes
```

### `zirv chat` and `zirv agent`

`zirv chat` and `zirv agent` are shorter top-level aliases for `zirv ctx
chat` and `zirv ctx agent`. Both are reserved command names, compared
case-insensitively (see [Reserved Command Names](#reserved-command-names)),
so a script or shortcut can never shadow them, and — unlike the
bare-invocation alias above — an explicit `zirv chat` always starts a
session regardless of the local-`.zirv`/terminal checks bare `zirv` applies.
`zirv ctx chat --help` (and `zirv chat --help`) prints `Usage: zirv ctx
chat...` even when reached through the `zirv chat` alias — a cosmetic
side effect of the alias reusing `zirv ctx`'s own clap tree rather than
having a separate one, not a bug in the alias routing itself.

- **`zirv chat`** — the same interactive orchestrator session the bare
  invocation starts.
- **`zirv agent <name> <prompt> [-- flags]`** — delegates one task to a
  supervised headless worker on another enabled harness: the same pacing,
  rot detection and restart-with-handoff behavior `zirv ctx exec` gives a
  hand-written invocation, as one command. Pass `-` as the prompt to read it
  from stdin instead.

#### Nested sessions are refused

`zirv chat` (and `zirv ctx wrap`) refuse to start when they can tell they are
already running *inside* an agent session — `ZIRV_CTX_SESSION` or
`ZIRV_CTX_SOCKET` is set, or Claude Code's own `CLAUDE_PID`+`CLAUDECODE`
pair is:

```
zirv ctx chat: refusing to start inside an existing agent session
(ZIRV_CTX_SESSION=abcdef12). A nested interactive session can post turn
signals into the outer supervisor and get the outer session compacted,
restarted or killed. Run it from a plain terminal, or pass --allow-nested
(or set ZIRV_ALLOW_NESTED=true) to override.
```

This is not a tidiness rule. A nested interactive supervisor shares the outer
session's console, and if its own turn-signal socket fails to bind, its child
would report turn boundaries into the **outer** supervisor's rot engine —
which eventually verdicts a restart and ends the session the human was
actually talking to. Pass `--allow-nested`, or set `ZIRV_ALLOW_NESTED=true`,
if you mean it.

The **headless** verbs — `zirv ctx exec`, `zirv ctx loop` and `zirv agent` —
are deliberately *not* gated: delegating a task to a worker from inside a
session is exactly what they are for, and a worker never takes the shared
console over. Each of them still scrubs `ZIRV_CTX_SESSION`,
`ZIRV_CTX_SOCKET` and `ZIRV_CTX_TRANSCRIPT` off every child it launches
before setting its own, so a worker can never inherit another session's
identity.

#### The dashboard: multiple sessions in one terminal

On a real, large-enough terminal (at least 80x20 — a taller floor than
`wrap`'s), `zirv chat` (bare `zirv` included) opens a session multiplexer
instead of a single wrapped session: a dashboard process owning several
interactive sessions at once, each a supervised PTY child (ConPTY on Windows,
a native PTY elsewhere) rendered through its own embedded terminal-screen
model, behind a persistent header and sidebar. Too small a terminal falls back
to the single-pane `wrap` session instead, with a one-line notice naming the
floor; `--simple` skips the dashboard entirely.

The first pane is always the orchestrator you are talking to. Further panes
come from the `s` (spawn) dashboard command, or from a `zirv ctx agent`
invocation run *inside* one of the dashboard's own panes, which asks the
dashboard to open a fresh pane rather than running headless — an untrusted
request the dashboard re-validates against live configuration (pane cap,
adapter gate, working-directory match) before honoring it, never treated as
authority on its own. Every live pane and its role are tracked in the same
session registry `zirv ctx status` reports (see [Session registry and
nudging](#session-registry-and-nudging) above), so `zirv ctx nudge`/`zirv ctx
send --to-session` can address one pane directly.

All dashboard keybindings live behind one `Ctrl+A` prefix: digits `1`-`9`
switch panes, `Tab`/arrows navigate, `s`/`n`/`m` open spawn/nudge/mail
overlays, `o` opens the handover picker (swap the focused pane's model or
harness in place — see [Cross-harness fallback and
handover](#cross-harness-fallback-and-handover) below), `z` zooms the focused
pane, `e` shows recent errors, `?`/`h` shows help, and `q` quits. On quit, the
dashboard writes a restore roster so a next launch can offer to reopen the
same panes.

Every dashboard control below is repo-forbidden (see [Trust
boundary](#trust-boundary) below — a checkout cannot switch it on/off or
change its own limits) with one deliberate exception: `idle_quiet_ms` is a
pure per-session timing knob over a session the operator already chose to run
interactively, not a cap standing between an untrusted layer and something it
must not raise for itself, so a repository may set it:

```toml
[dash]
enabled = true               # ZIRV_CTX_DASH
sidebar_cols = 24            # ZIRV_CTX_DASH_SIDEBAR_COLS
roster_max_age_secs = 604800 # ZIRV_CTX_DASH_ROSTER_MAX_AGE_SECS
max_panes = 9                 # ZIRV_CTX_DASH_MAX_PANES
mouse = true                  # ZIRV_CTX_DASH_MOUSE
idle_quiet_ms = 10000          # ZIRV_CTX_DASH_IDLE_QUIET_MS -- repo-settable
```

### `zirv memory`

`zirv memory` manages this repository's memory bank without starting an AI
session:

```bash
zirv memory init --dry-run
zirv memory init
zirv memory init --source /path/to/docs --merge
zirv memory status
zirv memory list
zirv memory recall staging-db
zirv memory remember staging-db-creds "the staging DB creds live in 1Password under staging-db"
zirv memory remember deploy-cmd "cargo build --release" --importance high --confidence high --tag deploy --tag release
zirv memory forget staging-db-creds
zirv memory verify staging-db-creds
```

`remember` also takes `--importance <low|normal|high>`, `--confidence
<low|normal|high>`, and a repeatable `--tag <t>` — all optional, unset by
default. They land in the stored entry and feed `zirv memory recall`'s
ranking (`retrieval::score_one`); `zirv ctx remember` has no equivalent
flags. `--importance`/`--confidence` reject any value outside the three
listed.

`memory init` proposes a bounded set of durable shared entries from repository
validation/toolchain surfaces and high-signal Markdown sections. `--dry-run`
changes nothing, `--source` accepts a Markdown file or directory (including an
Obsidian vault), and a non-empty shared bank is refused unless `--merge` is
passed; merge mode adds missing keys and never silently overwrites curated
entries. `--max-entries` and `--max-bytes` cap initialization.

Every verb defaults to the **private** (machine-local) bank; pass `--shared`
to act on the **shared**, repository-owned bank instead — see
[Memory bank](#memory-bank) below for what the two scopes mean. `list` and
`recall` respect each scope's own gate (`memory.enabled` /
`memory.shared_enabled`): a disabled scope lists or recalls empty rather
than showing what it holds. `status` never hides a disabled scope's counts
— it marks the scope `disabled` but still reports its entry count and
stored bytes, since a byte count is not the entry content the gate exists
to withhold. `forget` and `verify` work even while a scope is disabled —
disabling a scope must never trap data behind it. `status` never prints an
entry's key or body, only scope availability, entry counts, stored bytes,
and the configured injection budget. `zirv ctx remember --key <k> --text
<t>` / `zirv ctx recall` / `zirv ctx forget <k>` (flag-based, private-scope
only) are untouched and keep working exactly as before — `zirv memory` is a
newer, scope-aware surface alongside them, not a replacement. `forget` on a
missing key exits `0` (it is idempotent — "already gone" is success);
`verify` on a missing key exits `1` (it is stamping a claim about an entry
that does not exist, which is a real failure) — the asymmetry is
deliberate, not a bug.

### Sending mail between sessions

Agent sessions running on the same machine can leave each other short notes,
scoped to the current repository, with `zirv ctx send` and `zirv ctx inbox`:

```bash
zirv ctx send --message "the webhook route moved to /v2/webhook"
zirv ctx inbox
```

`zirv ctx status` reports how many are waiting (`mail: N unread`). A mail
message is free-form text written by whichever agent session sent it, not an
operator instruction — see the vault's Untrusted Configuration page for how
it's capped and labeled the same way the other untrusted surfaces are.

Add `--to-session <prefix>` to address one specific live session instead of
every session an agent has: `zirv ctx send --to-session abcd1234 --message
"..."` resolves `abcd1234` (a short id, or a unique prefix of one — see
[Session registry and nudging](#session-registry-and-nudging) below) against
the live registry and stores the full resolved id, so the message keeps
finding its target even if the registry record itself is gone by the time it
is read. A session-addressed message is only ever delivered into a
**headless** session's own launch prompt (`exec`/`loop`, when
`[mail] enabled = true`); an **interactive** session (`chat`/`wrap`) only ever
gets a one-line unread-count advisory on its status bar or event channel,
split as broadcast+direct once something is addressed to it specifically —
never the message body itself, the same "advisory, not authority" rule
`zirv ctx nudge` follows below. There is no environment variable for
`--to-session`: unlike the config knobs elsewhere in this document, session
addressing is a per-invocation argument, not something an operator or a repo
would want to pin as a default.

### Session registry and nudging

Every supervised session (`wrap`, `exec`, `loop`, `chat`) registers itself
under the state dir at `<state>/sessions/<short8>.json` for as long as it is
alive — best effort, released when the supervisor exits, and swept
automatically the moment `zirv ctx status` (or anything else that reads the
registry) notices its process is gone. `zirv ctx status` reports it under
`sessions:`, one line per record:

```
sessions:
  abcdef12  claude  exec  pid 48213  3m  live         -work-my-repo
  9a8b7c6d  claude  wrap  pid 19042  40m  stale        -work-my-repo
  1f2e3d4c  claude  wrap  pid 51120  5m  unreachable  -work-other-repo
```

`unreachable` means the process is running but bound no turn-signal socket
(`--no-supervise`, or the socket failed to bind), so it never checks for
wake-ups: `zirv ctx nudge` refuses such a target and says so, while
`zirv ctx send` still leaves a message for its next run.

`<short>` is the same eight-character id `--to-session` and `zirv ctx nudge`
resolve a prefix against. A socket left behind by an older zirv binary that
predates the registry (`s/*.sock` with no matching JSON record) still shows
up, labeled `(no record)`, so a mixed-version machine never silently drops a
live session from the listing.

`zirv ctx nudge <prefix> --message <text>` wakes a live session early instead
of waiting for it to notice on its own:

```bash
zirv ctx nudge abcd --message "please check the new failing test"
```

A nudge prefix must be at least four characters (or a session's whole short
id) — unlike `--to-session`, which only addresses a message, a nudge wakes
and can restart what it resolves to, and on a machine running one session a
single mistyped character is still "unique". A shorter prefix is refused and
the live sessions are named back to you.

The message itself is ordinary, durable mail (visible in `zirv ctx inbox`
even if the wake-up is missed), so the two pieces are decoupled on purpose: a
nudge is a wake-up signal plus a payload, stored separately, and losing the
wake-up never loses the message. For a headless session (`exec`), a nudge
costs the in-flight turn — the session is stopped and relaunched with a
handoff distilled from the transcript so far, the same recovery path a rot
restart uses, just triggered by an operator instead of the rot engine. That
restart is bounded by `[supervise] max_nudges` (default 3, `ZIRV_CTX_MAX_NUDGES`)
so a session cannot be interrupted indefinitely; past the cap a nudge's
message is still queued as mail but the session runs on untouched. The cap
counts *consecutive* nudges — it resets as soon as the session reports a turn
of its own, so a long-running session that keeps making progress can keep
being steered. For an interactive session (`wrap` or `chat`), a nudge is
advisory only: it never restarts or types anything into the agent, and it
never receives the message body — it just surfaces on the status bar and
event channel that a nudge arrived, pointing at `zirv ctx inbox`. `zirv ctx
nudge` says so at send time too when the target it resolved is interactive,
so an advisory delivery never looks like a nudge that silently did nothing. Either way, latency is bounded by
`[supervise] poll_ms` (default 2000ms) — the interval a supervisor's own tick
already runs on, since a nudge just claims a marker file that same tick
checks for.

### Banner, status bar and events

A `zirv ctx chat` session (bare `zirv` included) with a real, large-enough
terminal attached also gets a bit of chrome:

- a one-time **launch banner** naming the resolved harness, the rule that
  chose it, and the session id;
- a reserved **one-row status bar** pinned to the bottom of the terminal;
- an **event channel** on stderr, one line per notable event, in the shape
  `[HH:MM:SS] zirv ▸ <message>`.

All three degrade together and only in one direction: `--simple`,
`--no-supervise`, a terminal narrower than 40 columns or shorter than 8
rows, or a non-terminal stdout turns every piece off, and nothing here ever
upgrades a session mid-run. Turn just the event channel off with `--quiet`,
the `ZIRV_CTX_QUIET` environment variable, or `[chrome] events = false` in
`ctx.toml`; `[chrome] banner` and `[chrome] bar` switch the other two off
the same way. See [.settings.toml](#settingstoml) below for enabling or
disabling the harnesses themselves (claude, codex) — a separate file from
`[chrome]`.

## Features

- **YAML-Driven Scripts**: Define commands in `.zirv/` files with metadata (name, description, params, secrets).  
- **Capture Output**: Use `capture: var_name` on any step to grab its stdout into `${var_name}` for later substitution.  
- **Failure Hooks**: On a step failure you can declare a `fallback` sub-chain of commands to run as a side action; the original command is never retried.  
- **Flexible Options**: Interactive mode, OS filters, `proceed_on_failure`, delays, and secret support.  
- **Multi-Format**: Supports YAML, JSON, and TOML, extendable.  
- **Cross-Platform**: Compatible with Windows, macOS, and Linux.
- **Helpful Errors**: A mistyped script or shortcut name gets up to 3 "did you mean" suggestions instead of a bare failure.
- **Model-Agnostic Workflows**: Compact skills, durable phase state, risk-based lifecycle selection, targeted verification, independent review packages, artifacts, and local telemetry work across supported agent adapters.

---

## Installation

Pick your OS below. Each has one copy-paste command.

### Windows

```bash
choco install zirv
```

### macOS

```bash
brew tap glubiz/homebrew-tap
brew install zirv
```

> If Homebrew reports the tap as untrusted, run `brew trust glubiz/tap` and retry.

The published binary is universal (Intel and Apple Silicon), so this works on either Mac.

### Linux

Recommended — install script:

```bash
curl -sSfL https://raw.githubusercontent.com/Glubiz/zirv-dynamic-cli/main/install.sh | sh
```

To install a specific version:

```bash
curl -sSfL https://raw.githubusercontent.com/Glubiz/zirv-dynamic-cli/main/install.sh | sh -s -- <version>
```

Alternative — Homebrew on Linux, via the same tap:

```bash
brew tap glubiz/homebrew-tap
brew install zirv
```

> If Homebrew reports the tap as untrusted, run `brew trust glubiz/tap` and retry.

The prebuilt Linux release is x86_64-only. On other architectures (aarch64, armv7, ...) the install script fails fast with a pointer to the source build; the Homebrew formula does not guard the architecture, so skip it there and build from source instead (see below). Releases up to 2.39.0 additionally require glibc 2.39+; from 2.39.1 the Linux binary is fully static.

### From source (any platform/arch)

```bash
cargo install --git https://github.com/Glubiz/zirv-dynamic-cli
```

Works today without a crates.io publish, and is the only supported path on architectures the release pipeline doesn't build for (e.g. Linux aarch64).

### Precompiled Binaries
Download the latest release from the [GitHub Releases]:
https://github.com/Glubiz/zirv-dynamic-cli/releases

Assets per version: `zirv-<version>-linux.tar.gz` (x86_64), `zirv-<version>-macos.tar.gz` (universal x86_64+arm64), `zirv-<version>-windows.exe`.

## Upgrading

### Homebrew (macOS & Linux)

```bash
brew upgrade zirv
```

### Chocolatey (Windows)

```bash
choco upgrade zirv
```

### Install Script (Linux)

Re-run the install script to get the latest version:

```bash
curl -sSfL https://raw.githubusercontent.com/Glubiz/zirv-dynamic-cli/main/install.sh | sh
```

### From source

```bash
cargo install --git https://github.com/Glubiz/zirv-dynamic-cli --force
```

## Usage

`zirv help` (also `zirv h`, `zirv --help`, `zirv -h`) lists every available
script and shortcut, local and global.

### Initialize a Project

Run:
```bash
zirv init
```
Creates a `.zirv/` directory, its `.zirv/commands/` subdirectory (where you
will define your scripts, as of zirv 3.0), and a default `.shortcuts.yaml`.
The `.zirv/` directory is created in the current working directory or in the
HOME directory depending on the commandline interactions.

### Creating a New Script
```bash
zirv create
```
Interactively asks for the script name, an optional shortcut key, and whether
to create it locally or in the global `~/.zirv` folder, then writes a
template script into `.zirv/commands/` (or `~/.zirv/commands/`), plus a
shortcut entry in `.zirv/.shortcuts.yaml` if one was given.

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
Place your script files in `.zirv/commands/` (e.g., `build.yaml`):
  
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

If the name doesn't match any script or shortcut (checked locally in
`.zirv/commands/`, then globally in `~/.zirv/commands/`), zirv suggests up to
3 close matches by edit distance and points you to `zirv help`. If a script
was left at the `.zirv` root instead of moved into `commands/` (the pre-3.0
layout), the error names it and says where it needs to move — there is no
fallback lookup at the old location:

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
The `.zirv/` directory contains zirv's configuration; your scripts live in its
`commands/` subdirectory (zirv 3.0). The structure is as follows:

```
.zirv/
├── .shortcuts.yaml
├── ctx.toml
├── commands/
│   └── ...your script files
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

`help`, `version`, `init`, `create`, `ctx`, `memory`, `context`, `setup`, `report`,
`chat`, `agent`, `skill`, `workflow`, `test`, `verify`, `artifact`, `frontend`, and
their short aliases `h`, `v`, `i`, `c`, are handled as built-in commands before zirv ever
looks in `.zirv/`. The comparison is case-insensitive (`Chat`/`CHAT` collide
just as much as `chat`, matching how NTFS/APFS resolve script filenames), so
a differently-cased script or shortcut is caught too, even though only the
exact lowercase spelling is literally intercepted as a routing alias. A
script file or shortcut key using one of these names (in any case) can never
be invoked:

- `zirv help` lists it but marks it `(shadowed by a built-in command,
  unreachable)`.
- `zirv create` warns about the collision and asks for confirmation before
  creating it anyway; in non-interactive mode (see
  [Creating a New Script](#creating-a-new-script)) the collision is an error
  instead.

## Development Workflows

Zirv owns the high-level development lifecycle outside model conversation
memory. Only the current phase's selected skill instructions are injected;
completed phases remain durable private state and are not repeated after a
session restart or compaction.

```bash
zirv skill list
zirv skill show systematic-debugging --agent codex
zirv workflow classify --task "fix authentication race"
zirv workflow start bugfix --task "fix authentication race" --agent codex
zirv workflow start feature --task "use only shipped methodology" --built-in-only
zirv workflow status
zirv frontend profile
zirv frontend capabilities --agent claude
zirv frontend check
zirv frontend render
zirv frontend review --agent codex
zirv frontend benchmark
zirv test changed
zirv verify
zirv workflow stats
```

Built-in workflows cover `feature`, `bugfix`, `refactor`, `spike`, and
`review`. Deterministic intent/complexity/risk classification selects
proportional design, approval, test, and review depth; sensitive auth/security
and database/schema changes cannot be downgraded below High risk.

### The full verb set

```bash
zirv workflow list                              # built-in workflow definitions
zirv workflow show feature                       # one definition's steps
zirv workflow classify --task "..."               # classify without starting
zirv workflow start feature --task "..." [--agent claude] [--built-in-only]
zirv workflow status [id]                         # one instance, or the active one
zirv workflow resume <id>                         # restore as the active workflow
zirv workflow context [id]                        # the current step's resolved skill context
zirv workflow artifacts <id> [--json]              # committed work-product state
zirv workflow agents list|show <id>|dispatch <id> --adapter <name> --prompt <task>
zirv workflow approve <id>                        # approve the current gated step
zirv workflow advance <id> --outcome success|failure
zirv workflow review package <id> | run <id> --agent <name> | add | ...
zirv workflow maintain scan [--repo <path>] [--json]
zirv workflow stats                               # local bounded telemetry
```

### Lifecycle and artifacts

A workflow instance moves through `intent → spec → plan → implement → test →
review → verify → deploy`. Ceremony is proportional to classification: a
trivial bugfix skips straight to the debug/test/verify spine, while
substantial or high-risk feature/refactor work gains intent, plan (and, for
substantial/high-risk work, spec) artifact gates plus an approval-gated design
step. Frontend work overlays the same engine rather than running a separate
one, selected automatically from task language and changed frontend paths.

Artifact steps write fixed templates to `.zirv/work/<workflow-id>/` (`intent.md`,
`spec.md`, `plan.md`, ...) committed to the repository. `zirv workflow approve`
refuses an untouched template, then pins the accepted file's SHA-256 digest and
timestamp in private state; a later step folds only accepted, hash-matching
artifacts into prompt context. Editing or deleting an accepted file after the
fact reopens its acceptance gate and invalidates later completed steps, so
implementation can never silently proceed against a plan that changed
underneath it — `zirv workflow artifacts <id>` shows pending/accepted/drifted
state directly.

Classification is re-measured (never downgraded) whenever a workflow advances
into a review or verify step, so a review/verify gate the initial `workflow
start` measurement missed (an empty tree, before any code existed) still gets
added once the real change exists.

### Deploy tiers

`[workflow.deploy] tier = "development" | "staging" | "production"` in
`~/.zirv/ctx.toml` (`ZIRV_CTX_WORKFLOW_DEPLOY_TIER` for the final override) is
operator-only; a repository may set only `workflow.deploy.minimum_tier`, and a
running workflow's resolved tier only ever ratchets upward:

| Tier | Deploy step |
|---|---|
| `development` | Auto-advances once test/review/verify evidence passes |
| `staging` | Requires an explicit `zirv workflow approve` on the deploy step |
| `production` | Requires approval, plus at least one fresh independent `reviewer`-seat run and fresh final `zirv verify` evidence; an open finding or stale evidence blocks it outright |

### Workflow adoption

`[workflow] adoption = "off" | "advise" | "nudge" | "enforce"` in
`~/.zirv/ctx.toml` (`ZIRV_CTX_WORKFLOW_ADOPTION` for the final override) is
operator-only and detects a session that has done "substantial" edit work --
at least 5 edit-like tool calls, or at least 1 edit-like call over 12 turns --
with no active `zirv workflow`:

| Level | Behavior |
|---|---|
| `off` | No detection, no nudge, no gate. |
| `advise` | A one-time nudge rides the Stop hook's `systemMessage` once substantial work is detected. |
| `nudge` (default) | The same nudge, repeated every 5 turns while the session stays substantial with no active workflow, and also surfaced on the next prompt (`UserPromptSubmit`) if a workflow still has not started. |
| `enforce` | The `nudge` behavior, plus `zirv agent` (`ctx::agent::run_with`) refuses to dispatch for a session recorded as substantial with no active workflow, until one is started. |

### Agent registry

Workflow seats are provider-neutral data, not harness-specific plugins: a
`WorkflowStep.agent` addresses one by id. Built-in seats are `implementer`,
`reviewer`, `doc-keeper`, `security-scanner`, and `explorer` — `reviewer` is
pinned read-only by its own adapter. `~/.zirv/agents/*` (operator-global) may
replace a built-in seat; `.zirv/agents/*` (repository) is disabled unless the
operator sets `workflow.repo_agents_enabled`, and even then may only add
non-colliding ids — a repository manifest can never rewrite `reviewer` or grant
itself capabilities it does not already have. `zirv workflow agents list|show`
inspects the resolved registry and provenance; `zirv workflow agents dispatch
<id> --adapter <name> --prompt <task>` launches that seat directly.

### Maintain loop

`zirv workflow maintain scan` is an invoked scanner, not a daemon: it runs
every operator-configured deterministic detector once, reading detector
commands only from `[workflow.maintain.detectors.<id>]` in the operator's own
`~/.zirv/ctx.toml` (repository config cannot define one). Each detector is a
bounded command judged by exit code/timeout or a stdout line-count threshold;
zirv retains only exit code, timeout state, and line/byte counts, never
detector command or output bodies. A breach parks one bounded incident
workflow at its Intent acceptance gate (`.zirv/work/<id>/intent.md`, committed
with detector metadata) and, when operator-only `[report] repository =
"owner/repo"` is configured, auto-files a title-deduplicated GitHub issue. A
clean scan clears the incident marker, so a later recurrence opens fresh.

### Frontend quality

Frontend tasks are selected automatically from task and path evidence. Zirv
derives a repository-specific design profile, classifies each surface as
persuade/operate/read/experience, and requires a product-grounded design thesis,
signature, justified risk, system, complete user journey, and resilient state
matrix before implementation. A built-in craft floor plus phase skills drive
the work; a 44-rule offline detector checks deterministic accessibility, UX,
responsive, content, motion, internationalization, media, performance, and
anti-slop hazards. Zirv starts and cleans up the discovered dev server, captures
narrow/intermediate/wide screenshots, and requires a fresh AI review with 13
explicit UI/UX scores, each at least 4/5. The score is produced by an isolated,
read-only Zirv reviewer rather than accepted from CLI arguments. `zirv frontend
capabilities --agent <claude|codex> [--json]` reports the same provider-neutral
skill/provenance contract and logical capability matrix `zirv skill show
--agent` reports elsewhere. There is no frontend init command or
questionnaire: the active agent owns routine design, rendering, and review
decisions. Missing, stale, truncated, unavailable, weak, or failed evidence
cannot advance frontend test, review, or verify gates.

Detector waivers are schema-versioned TOML in `.zirv/frontend-waivers.toml` or
the operator-owned `~/.zirv/frontend-waivers.toml`. Every waiver names a rule,
an exact path or `/**` prefix, an optional evidence value, and a reason.
Repository waivers are advisory only: they can disposition advisory craft
findings, but only the operator-owned file can waive a blocking accessibility
finding.

[Glubiz/zirv-generic-frontend](https://github.com/Glubiz/zirv-generic-frontend)
is the reference frontend template built against this contract — it is also
the source behind [cli.zirv.io](https://cli.zirv.io), the site rendering this
README.

Optional repository checks live in `.zirv/verify.toml`. Custom skills may be
shared under `.zirv/skills/` or kept operator-global under
`~/.zirv/skills/`; `zirv skill list/show` reports the winning source and
`--built-in-only` disables custom layers. The same flag on `workflow start`
persists that choice across resume and prompt composition. Repository skills are untrusted:
they can request logical capabilities but never grant themselves filesystem,
shell, network, or other permissions.

Use `zirv workflow review package <id>` for a compact diff/test review input,
`zirv artifact render <path>` for stable static artifact references, and
`zirv workflow stats` for local bounded telemetry. Review results are persisted
from a strict bounded JSON contract and fix/re-review stops after three rounds.
Interactive artifact fallback obeys canonical policy (`ask` needs `--approve`),
and supervised Claude/Codex workflows attribute available transcript token
deltas automatically. Telemetry excludes prompts, source code, diffs, command
output, and model responses by construction.

## Context Management (zirv ctx)

`zirv ctx` watches Claude Code sessions for context rot and intervenes before
quality drops: it advises, compacts early, or restarts the session with a
distilled handoff. Scoring is deterministic, and every decision is logged.

**Agent support.** Claude Code and Codex are both supported for supervised
sessions, but not to the same depth. Claude Code gets the full feature set:
event parsing, a rot score, turn signals and an injected system prompt. Codex
launches and supervises fine -- `--agent codex` succeeds both when `codex`
resolves to a real binary and when nothing named `codex` is installed at all
(that case is left to fail at spawn time with the OS's own "not found"), the
same contract `--agent claude` gives claude -- but with an honestly degraded
surface, because the pieces below were never verified against an
authenticated CLI:

- No event parsing, so no rot score and no structural context for codex
  sessions (`parse_events`/`structural_context` stay empty).
- No usage source: a codex session's usage reads `openai: no usage source`
  rather than a real reading.
- Lifecycle hooks are available and `zirv setup` registers them, but event
  parsing is still absent, so Codex cannot yet produce a meaningful rot score
  or structural context from its rollout.
- Direct launches receive Zirv's composed prompt through Codex's official
  per-run `developer_instructions` override. Windows command/PowerShell shims
  stay fail-closed because inline repository text would be reparsed by a shell.

That direct path covers interactive orchestrators and headless workers.
Shell-shim launches retain the task-prompt fallback for mail and worker
instructions but intentionally withhold repository-authored system layers.

Full event support is tracked in
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
| `zirv ctx hook <stop\|prompt\|pre-compact\|pretool\|notify>` | Agent hook entrypoints |
| `zirv ctx status` | Shows supervised sessions, the resolved chat agent, unread mail, recent decisions and handoffs |
| `zirv ctx usage` | Shows usage-window state, or `usage tee` to collect it from the statusline |
| `zirv ctx optimize` | Reports redundancy, contradictions and dead references in the files that steer your sessions |
| `zirv ctx chat` | Starts an interactive orchestrator session on the resolved adapter (also `zirv chat`, or bare `zirv`; see [Just Run `zirv`](#just-run-zirv)) |
| `zirv ctx agent <name> <prompt>` | Delegates one task to a supervised headless worker on another enabled harness (also `zirv agent`) |
| `zirv ctx send [--to-session <prefix>]` / `zirv ctx inbox` | Leaves or reads short notes between agent sessions on this machine, scoped to the repo, optionally addressed to one live session |
| `zirv ctx nudge <prefix> --message <text>` | Wakes a live supervised session early with a message, instead of waiting for it to poll |
| `zirv ctx remember --key <k> --text <t>` / `zirv ctx recall` / `zirv ctx forget <k>` | Reads and writes this repo's cross-session memory bank |
| `zirv ctx handover [--agent <name>] [--model <tier\|id>] [--dry-run] [--force]` | Swaps the orchestrator seat's harness or model in place mid-session, carrying a handoff packet across the swap — see [Cross-harness fallback and handover](#cross-harness-fallback-and-handover) below |
| `zirv ctx permissions audit\|compile\|propose` | Audits, compiles, or (operator opt-in) proposes command-permission approvals from recent transcripts — see [Permission auditing](#permission-auditing-and-safe-list-proposals-issue-178) below |

### Signals and verdicts

Four signals over the trailing window (default 10 turns):

1. **Context size** (a gate, not a vote). The floor and ceiling scale with the
   model's real context window (issue #155): by default the floor sits at 50%
   of capacity and the ceiling at 80% (`score.token_floor_ratio`/
   `token_ceiling_ratio`), so a 200k-token seat still gates at 100000/160000,
   the pre-ratio absolutes, and a 1M-token seat gates at 500000/800000
   instead of restarting at the same 160000 tokens with 840k of headroom
   left. When no capacity is known (codex today), the absolute
   100000/160000 fallbacks apply unchanged. `score.token_floor`/
   `token_ceiling` still pin an exact number outright, overriding the ratio.
   Below the floor the verdict is always `healthy`; at or above the ceiling
   it is at least `compact`.
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

[mail]
enabled = true
max_message_bytes = 4096      # per-message cap, applied by `zirv ctx send`
max_delivered_bytes = 4096    # cap on a whole batch folded into one launch prompt
keep = 50                     # unread messages kept per repo before the oldest are pruned

[memory]
enabled = true
harvest = false                # opt-in; see "Memory bank" below
max_entries = 50               # entries kept per repo before the oldest (by Written) are pruned
max_entry_bytes = 512          # per-entry body cap
max_injected_bytes = 2048      # superseded by core_max_bytes; kept only so an old config does not error
shared_enabled = true          # whether the repo-owned shared bank (<repo>/.zirv/memory/) is read at all
core_max_bytes = 2048          # cap on the merged private+shared core layer folded into every session
retrieval_max_bytes = 2048     # cap on `zirv memory recall`; reserved for future context-aware session retrieval
retrieval_max_entries = 6      # max number of recalled entries, independent of bytes

[chrome]
banner = true   # the one-time launch banner
bar = true      # the reserved one-row status bar
events = true   # the `zirv ▸` announcement channel on stderr
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
`agent`, `agent_bin`, `supervise.on_failure`, `handoff.model`,
`optimize.model`, `sandbox.enabled`, `prompt.enabled`, `prompt.repo_layer`,
`prompt.max_repo_bytes`, `prompt.harnesses`, `prompt.codex_orchestrator`, `mail.enabled`,
`mail.max_delivered_bytes`, `chrome.events`, any `memory.*` key, any
`dash.*` key, any `pace.*` key, `review`, `worker`, `handover`, or any of the
five keys that feed the token gate (`score.token_floor`,
`score.token_ceiling`, `score.token_floor_ratio`, `score.token_ceiling_ratio`,
`score.model_context_tokens`); doing so is an error
that names the key. Set those in `~/.zirv/ctx.toml`, or with the matching
`ZIRV_CTX_*` variable below, which comes from the operator rather than the
checkout:

| Forbidden repo key | Set instead via |
|---|---|
| `agent` | `ZIRV_CTX_AGENT` |
| `agent_bin` | `ZIRV_CTX_AGENT_BIN` |
| `supervise.on_failure` | `ZIRV_CTX_ON_FAILURE` |
| `handoff.model` | `ZIRV_CTX_MODEL` |
| `optimize.model` | `ZIRV_CTX_OPTIMIZE_MODEL` |
| `sandbox.enabled` | `ZIRV_CTX_SANDBOX` |
| `sandbox.extra_allow` | `ZIRV_CTX_SANDBOX_EXTRA_ALLOW` |
| `prompt.enabled` | `ZIRV_CTX_PROMPT` |
| `prompt.repo_layer` | `ZIRV_CTX_PROMPT_REPO` |
| `prompt.max_repo_bytes` | `ZIRV_CTX_PROMPT_MAX_REPO_BYTES` |
| `prompt.harnesses` | `ZIRV_CTX_PROMPT_HARNESSES` |
| `prompt.codex_orchestrator` | `ZIRV_CTX_PROMPT_CODEX_ORCHESTRATOR` |
| `context.max_common_bytes` | `ZIRV_CTX_CONTEXT_MAX_COMMON_BYTES` |
| `context.max_harness_bytes` | `ZIRV_CTX_CONTEXT_MAX_HARNESS_BYTES` |
| `context.max_harness_roster_bytes` | `ZIRV_CTX_CONTEXT_MAX_HARNESS_ROSTER_BYTES` |
| `mail.enabled` | `ZIRV_CTX_MAIL` |
| `mail.max_delivered_bytes` | `ZIRV_CTX_MAIL_MAX_DELIVERED_BYTES` |
| `chrome.events` | `ZIRV_CTX_QUIET` (see the note below on why this one's name looks different) |
| `memory.enabled` | `ZIRV_CTX_MEMORY` |
| `memory.harvest` | `ZIRV_CTX_MEMORY_HARVEST` |
| `memory.max_entries` | `ZIRV_CTX_MEMORY_MAX_ENTRIES` |
| `memory.max_entry_bytes` | `ZIRV_CTX_MEMORY_MAX_ENTRY_BYTES` |
| `memory.max_injected_bytes` | `ZIRV_CTX_MEMORY_MAX_INJECTED_BYTES` |
| `memory.shared_enabled` | `ZIRV_CTX_MEMORY_SHARED` |
| `memory.core_max_bytes` | `ZIRV_CTX_MEMORY_CORE_MAX_BYTES` |
| `memory.retrieval_max_bytes` | `ZIRV_CTX_MEMORY_RETRIEVAL_MAX_BYTES` |
| `memory.retrieval_max_entries` | `ZIRV_CTX_MEMORY_RETRIEVAL_MAX_ENTRIES` |
| `memory.harvest_max_entries` | `ZIRV_CTX_MEMORY_HARVEST_MAX_ENTRIES` |
| `memory.harvest_max_bytes` | `ZIRV_CTX_MEMORY_HARVEST_MAX_BYTES` |
| `dash.enabled` | `ZIRV_CTX_DASH` |
| `dash.sidebar_cols` | `ZIRV_CTX_DASH_SIDEBAR_COLS` |
| `dash.roster_max_age_secs` | `ZIRV_CTX_DASH_ROSTER_MAX_AGE_SECS` |
| `dash.max_panes` | `ZIRV_CTX_DASH_MAX_PANES` |
| `dash.mouse` | `ZIRV_CTX_DASH_MOUSE` |
| `dash.workdir_roots` | `ZIRV_CTX_DASH_WORKDIR_ROOTS` |
| `supervise.max_heavy_workers` | `ZIRV_CTX_SUPERVISE_MAX_HEAVY_WORKERS` (deprecated alias for `max_heavy_operations`) |
| `supervise.max_heavy_operations` | `ZIRV_CTX_SUPERVISE_MAX_HEAVY_OPERATIONS` |
| `pace.use_credits` | `ZIRV_CTX_PACE_USE_CREDITS_CLAUDE` (the table-node match also blocks `pace.use_credits.codex` alone) |
| `pace.poll_enabled` | `ZIRV_CTX_PACE_POLL` |
| `pace.poll_min_interval_secs` | `ZIRV_CTX_PACE_POLL_MIN_INTERVAL_SECS` |
| `pace.blind_delay_secs` | `ZIRV_CTX_PACE_BLIND_DELAY_SECS` |
| `pace.spawn_soft_pct` | `ZIRV_CTX_PACE_SPAWN_SOFT_PCT` |
| `pace.spawn_hard_pct` | `ZIRV_CTX_PACE_SPAWN_HARD_PCT` |
| `review` (`review.claude`, `review.codex`) | `ZIRV_CTX_REVIEW_MODEL_CLAUDE` / `ZIRV_CTX_REVIEW_MODEL_CODEX` |
| `worker` (`worker.claude`, `worker.codex`) | `ZIRV_CTX_WORKER_MODEL_CLAUDE` / `ZIRV_CTX_WORKER_MODEL_CODEX` |
| `handover` (`handover.<agent>.<tier>`) | `ZIRV_CTX_HANDOVER_<AGENT>_<TIER>` (e.g. `ZIRV_CTX_HANDOVER_CLAUDE_DEEP`) |
| `safety.allow` | `ZIRV_CTX_SAFETY_ALLOW` |
| `safety.escape_allow` | `ZIRV_CTX_SAFETY_ESCAPE_ALLOW` |
| `safety.default` | `ZIRV_CTX_SAFETY_DEFAULT` |
| `safety.interactive_default` | `ZIRV_CTX_SAFETY_INTERACTIVE_DEFAULT` |
| `safety.sql` | `ZIRV_CTX_SAFETY_SQL` |
| `score.token_floor` | `ZIRV_CTX_TOKEN_FLOOR` |
| `score.token_ceiling` | `ZIRV_CTX_TOKEN_CEILING` |
| `score.token_floor_ratio` | `ZIRV_CTX_SCORE_TOKEN_FLOOR_RATIO` |
| `score.token_ceiling_ratio` | `ZIRV_CTX_SCORE_TOKEN_CEILING_RATIO` |
| `score.model_context_tokens` | `ZIRV_CTX_SCORE_MODEL_CONTEXT_TOKENS` |
| `workflow.repo_checks_enabled` | `ZIRV_CTX_WORKFLOW_REPO_CHECKS` |
| `workflow.repo_skills_enabled` | `ZIRV_CTX_WORKFLOW_REPO_SKILLS` |
| `workflow.repo_agents_enabled` | `ZIRV_CTX_WORKFLOW_REPO_AGENTS` |
| `workflow.deploy.tier` | `ZIRV_CTX_WORKFLOW_DEPLOY_TIER` |
| `workflow.adoption` | `ZIRV_CTX_WORKFLOW_ADOPTION` |
| `workflow.maintain` | `~/.zirv/ctx.toml only` |
| `report.repository` | `ZIRV_CTX_REPORT_REPOSITORY` |
| `workflow.telemetry_enabled` | `ZIRV_CTX_WORKFLOW_TELEMETRY` |
| `workflow.telemetry_max_events` | `ZIRV_CTX_WORKFLOW_TELEMETRY_MAX_EVENTS` |
| `workflow.telemetry_retention_days` | `ZIRV_CTX_WORKFLOW_TELEMETRY_RETENTION_DAYS` |
| `workflow.check_env_passthrough` | `ZIRV_CTX_WORKFLOW_CHECK_ENV_PASSTHROUGH` |
| `workflow.review_worker_budget_tokens` | `ZIRV_CTX_WORKFLOW_REVIEW_WORKER_BUDGET_TOKENS` |
| `workflow.review_worker_max_tool_calls` | `ZIRV_CTX_WORKFLOW_REVIEW_WORKER_MAX_TOOL_CALLS` |

The `mail.*`/`chrome.events` entries close the same hole `prompt.max_repo_bytes`
does: mail is folded into a launched worker's prompt as its own layer, so a
repo raising its own delivered-mail cap (or turning delivery back on after an
operator disabled it) would make the operator's choice decorative; a repo
silencing the announcement channel would hide its own degradation notices
from anyone running zirv there. `prompt.harnesses` closes the same loop for
the derived per-adapter harness-roster layer: a repo checkout must not be
able to force that layer back on for an operator who turned it off.
`prompt.codex_orchestrator` closes the same loop once more for codex's own
orchestrator-conventions layer (issue #167): a repo checkout must not be
able to re-enable it for an operator who turned it off.
`memory.*` closes the same hole again for the memory bank's *configuration*
(not its content -- see below): a repo checkout must not be able to switch
either scope's gate on or off for itself, raise its own caps, or switch
automatic harvesting on for anyone who runs zirv there (see
[Memory bank](#memory-bank) below -- the *shared* scope's whole point is
that its *content* is deliberately repo-committed, but a checkout still may
not flip its own `shared_enabled` gate any more than the private scope's
`enabled` gate). `dash.*` closes it once more for the session multiplexer
`zirv chat` opens on a capable terminal: a repo checkout must not be able to
switch it on or off, resize its sidebar, change how long a quit-time restore
roster stays offered, raise its own pane cap, or decide whether the
dashboard captures the mouse. `pace.*` closes it for usage pacing: a repo
must not be able to flip a spend decision, re-enable the active vendor-API
poll fallback an operator turned off, or change its cadence. `review`/`worker`
close it for which model spends the operator's tokens running background
review or delegated-worker sessions. `handover` closes the same hole for
`zirv ctx handover`: a repo checkout must not be able to pick which harness
or model the orchestrator seat swaps onto mid-session. `agent` closes a narrower hole
discovered once codex shipped out of the box: an explicit `agent = "codex"`
reaches `resolve_default`'s *configured* arm, which never consults the
repo-narrowing guard the no-`agent`-configured fallback loop has (see
[.settings.toml](#settingstoml) below) -- without this, a repo checkout could
pick which vendor account gets spent with that guard never in the way.
`workflow.adoption` closes the same hole once more for the workflow-adoption
nudge/enforce gate (issue #223): a repo checkout must not be able to turn its
own adoption pressure down to `off`, or up to `enforce` to hold an operator's
own agent dispatches hostage.
Everything else, including `chrome.banner`/`chrome.bar`,
`supervise.max_nudges`, and every threshold, is still repo-configurable.

#### Environment variables worth knowing

| Variable | Effect |
|---|---|
| `ZIRV_CTX_STATE_DIR` | Where handoffs, sockets, logs and usage state live |
| `ZIRV_CTX_TRANSCRIPT` | Pins the transcript `wrap` watches, overriding what the turn signal reports (see below) |
| `ZIRV_CTX_SOCKET`, `ZIRV_CTX_SESSION` | Exported into the supervised agent so its hook can find the supervisor. Set by zirv, not by you |
| `ZIRV_AGENT_<NAME>_ENABLED` | Enables or disables one adapter (`ZIRV_AGENT_CODEX_ENABLED`); see [.settings.toml](#settingstoml) below. Must be exactly `true` or `false` -- any other value is a hard error naming the variable, matching the strictness of every `ZIRV_CTX_*` boolean |

Every `[section] key` in the tables above also has a `ZIRV_CTX_*` variable;
the names follow the key, for example `ZIRV_CTX_DEBOUNCE_MS` for
`wrap.debounce_ms`, with two deliberate exceptions: a section's own top-level
`enabled` flag drops the `_ENABLED` suffix (`ZIRV_CTX_MAIL` for
`mail.enabled`, `ZIRV_CTX_PROMPT` for `prompt.enabled`, and so on), and
`ZIRV_CTX_QUIET` is a named alias for `chrome.events` set to its *opposite*
(`ZIRV_CTX_QUIET=true` turns events off) rather than `ZIRV_CTX_CHROME_EVENTS`,
because "quiet" is the more natural spelling for the flag most people will
actually reach for.

### .settings.toml

`ctx.toml` tunes how the ctx supervisor *behaves*; `.zirv/.settings.toml` is a
separate, zirv-wide file that only answers yes/no questions about what zirv
may *use*. The rule of thumb: if the question is "yes/no, may zirv use this
thing", it goes in `.settings.toml`; anything else goes in `ctx.toml`. Today
that means one section, per-adapter enable/disable:

```toml
# .zirv/.settings.toml
[agents.codex]
enabled = false
```

Layered the same way as `ctx.toml` -- `~/.zirv/.settings.toml`, then
`<repo>/.zirv/.settings.toml`, then `ZIRV_AGENT_<NAME>_ENABLED` (a boolean) --
but folded per agent rather than deep-merged:

```
final(name) = env(name) if set
            else home(name).unwrap_or(true) && repo(name).unwrap_or(true)
```

Every known adapter defaults to enabled. The environment is the operator, in
both directions: `ZIRV_AGENT_CODEX_ENABLED=true` re-enables an agent a repo
disabled, and `=false` disables one nothing else touched. A repository can
only narrow what it inherited -- `enabled = true` in a repo's own
`.settings.toml` is a silent no-op, since there is nothing there for a repo to
refuse. Disabling an agent is checked before that adapter's own readiness, so
`--agent codex` with codex disabled reports the disable, not codex's own
`ready()` outcome. `zirv ctx status` lists every known adapter, whether it is
enabled, and (when not) which file or variable disabled it. A malformed
*repo* `.settings.toml` never falls back to a fully permissive gate: `zirv ctx
optimize` and the Stop hook both fall back to the operator's own layers only
(home file, then environment) if the full config cannot be loaded, so a broken
repo file can narrow what an operator already disabled but can never revive it.
A repo disable can also never *pick* an agent on the operator's behalf: if a
repo-only `.settings.toml` disables the agent that would otherwise have been
the default (no `--agent`, no `agent =` configured, and that adapter is the
first enabled-and-ready one in registry order), zirv refuses rather than
silently falling back to a different, still-enabled adapter -- naming both
the disabled agent and the one it would have picked, and how to choose
explicitly (`--agent`, `agent =` in your own `~/.zirv/ctx.toml`, or the
`ZIRV_CTX_AGENT` environment variable — the repo's `.zirv/ctx.toml` cannot set
`agent`; it is a forbidden repo key).
Narrowing which agent is *possible* is a repo's call; narrowing which one you
actually get is not.

### Hook registration (Claude Code)

Add to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "Stop": [{ "hooks": [{ "type": "command", "command": "zirv ctx hook stop" }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "zirv ctx hook prompt" }] }],
    "PreCompact": [{ "hooks": [{ "type": "command", "command": "zirv ctx hook pre-compact" }] }],
    "PreToolUse": [{
      "matcher": "Agent|Task",
      "hooks": [{ "type": "command", "command": "zirv ctx hook pretool" }]
    }]
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

Current Codex versions support lifecycle hooks with the same JSON event shape.
`zirv setup apply` merges Zirv's handlers into `~/.codex/hooks.json`; review
and trust new definitions with `/hooks` in Codex. The older
`zirv ctx hook notify` compatibility entry point remains available for Codex
versions configured with the external `notify` program. Rollout event parsing
is still tracked in [issue #11](https://github.com/Glubiz/zirv-dynamic-cli/issues/11).

### Interactive use

```bash
alias claude='zirv ctx wrap -- claude'
```

The wrapped session is byte-for-byte identical to an unwrapped one until an
intervention, injection happens only at a turn boundary while you are idle, and
any supervision failure drops it back to pure passthrough. Flags you wrap are
kept across a restart, so `zirv ctx wrap -- claude --model opus` comes back as
an opus session.

`wrap` types agent-specific text into the session it supervises (`/compact`,
`/exit`), so it has to know which agent it is driving. It recognises a command
whose program is named `claude`; anything else — a wrapper script, `npx
claude`, a differently named binary — needs `--agent claude` to say so
explicitly, or it refuses rather than typing claude syntax into a program that
may not understand it. `--no-supervise` and `--simple` are exempt, since
neither injects anything:

```bash
alias claude='zirv ctx wrap --agent claude -- my-claude-wrapper.sh'
```

Note that a restart relaunches the *adapter's* program, so a wrapped wrapper
script comes back as a bare `claude`.

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

Passing the whole command is optional. With `--prompt` and nothing after `--`
that names a program, `exec` builds the launch from the adapter itself — the
same way every restart does — so the prompt never has to be encoded into argv
and read back out:

```bash
zirv ctx exec --agent claude --prompt "$PROMPT"
zirv ctx exec --agent claude --prompt "$PROMPT" -- --model opus  # extra flags
```

This is what a YAML agent step uses, and it is why a prompt that happens to
begin with `-` or to look like a flag is still just a prompt.

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
# .zirv/commands/issue-loop.yaml
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

### Cross-harness fallback and handover

Beyond waiting out a subscription window, zirv can steer work onto a
*different* harness. `fallback.rs` connects the agent roster, usage windows,
model-tier ladder, and delegation path: a **new** delegation (`zirv ctx
agent`, a dashboard spawn) can be rerouted away from an exhausted or
measured-low-headroom harness before it ever starts, while an **already
running** supervised session can only move once that harness itself stops on
a recognized usage-limit message — steering never interrupts a session that
is still making progress. Either way the alternate harness must be enabled,
ready, capacity-compatible, budget-compatible, and able to provide a verified
equivalent model tier (`cheap`/`standard`/`deep`); zirv never guesses a tier
translation for an operator-pinned model it cannot verify.

`zirv ctx status` reports the resolved policy and, for each harness in the
fallback order, its readiness, capacity and current headroom:

```
fallback: enabled | order claude -> codex | steer below 20% headroom | candidate min 10% | unknown assumes 25%
  fallback claude: enabled / ready / full / 62% measured
  fallback codex: enabled / ready / small-only / 25% assumed
```

Each harness line is `{enabled|disabled} / {ready|unavailable} / {small-only|full}
/ {headroom}`: whether `.settings.toml`/`ZIRV_AGENT_<NAME>_ENABLED` has that
adapter on, whether it currently resolves and is ready to launch, whether
`[agents]` capacity-limits it to small tasks only, and the same measured/
assumed/opted-out headroom reading described above.

Configure it under `[fallback]` in `~/.zirv/ctx.toml`:

```toml
[fallback]
enabled = true
order = ["claude", "codex"]
predictive_headroom_pct = 20.0        # steer new work below this headroom
min_candidate_headroom_pct = 10.0     # a candidate needs at least this much headroom to accept work
unknown_headroom_pct = 25.0           # assumed headroom when no reading exists (0 opts out)
small_task_max_tokens = 40000
small_task_max_tool_calls = 24
```

A repository checkout may only narrow these values (see [Trust
boundary](#trust-boundary) above); `ZIRV_CTX_FALLBACK*` environment variables
are the operator's final override.

**`zirv ctx handover`** performs the swap directly, on demand, mid-session —
the same mechanism the dashboard's `Ctrl+A o` picker and automatic fallback
routing both use underneath:

```bash
zirv ctx handover --agent codex --model standard
zirv ctx handover --model deep      # same harness, a different model tier
zirv ctx handover --dry-run          # print the resolved swap; change nothing
```

`--model` accepts a literal model id or a generic tier (`cheap`/`standard`/
`deep`), resolved per target harness — `claude`'s `deep` is `opus`, `codex`'s
is `gpt-5.6-sol`, for example, each overridable via `[handover.<agent>]` in
`~/.zirv/ctx.toml` or `ZIRV_CTX_HANDOVER_<AGENT>_<TIER>`. The swap carries a
distilled handoff packet across so the successor session picks up task
continuity; by default it waits for a verified-idle turn boundary, and
`--force` swaps mid-turn instead. `handover` (and `handover.<agent>.<tier>`)
is repo-forbidden: swapping the orchestrator seat's harness or model picks
which vendor account gets spent, so only the operator's own `~/.zirv/ctx.toml`,
`ZIRV_CTX_HANDOVER_*`, or flags may set it.

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

### Memory bank

Besides mail (session-to-session notes) and handoffs (task continuity across
a restart), zirv keeps a third, longer-lived store per repository: a small
bank of durable facts about the repository itself, independent of any one
task or session.

```bash
zirv ctx remember --key staging-db-creds --text "the staging DB creds live in 1Password under staging-db"
zirv ctx recall
zirv ctx forget staging-db-creds
```

See [`zirv memory`](#zirv-memory) above for a newer, scope-aware surface over
this same bank (`status`/`list`/`recall <query>`/`remember <key> <text>`/
`forget <key>`/`verify <key>`, `--shared` for the repository-owned scope
below) that works without starting an AI session — the two verbs above keep
working unchanged alongside it.

**The handoff-vs-memory boundary matters.** A handoff is task continuity: what
this specific task was doing, what remains, what to try next — written once
per restart, and only ever useful to the session that picks that particular
task back up. Memory is repository facts: things true regardless of which
task is in progress — a build command, where a credential lives, a
convention, a gotcha about how a dependency behaves. Task state does not
belong in memory, and a repository fact does not belong in a handoff.

By default nothing is added to the bank automatically — `zirv ctx remember`
is a deliberate act, by a session or a human. Set `[memory] harvest = true`
(or `ZIRV_CTX_MEMORY_HARVEST=true`) to let zirv *also* try to extract durable
facts on its own: right after a rot restart distills a handoff (`exec` or
`wrap`, and only from a genuinely distilled handoff, never the mechanical
fallback), one extra cheap-model call looks at the handoff's `Gotchas
learned` and `Files touched` sections and proposes zero or more `key: value`
facts. Harvesting stays off by default because a cheap model can be
confidently wrong: an unreviewed guess landing in a bank every future session
reads is a worse failure mode than simply not harvesting. A harvested entry
replaces any existing entry with the same key rather than duplicating it, the
same as an explicit `remember` does, and a distiller failure or timeout
leaves the bank untouched.

Every entry has a `Verified` stamp alongside its `Written` one; an entry
older than 30 days without being re-verified is flagged as stale wherever the
bank is summarized (`zirv ctx status`, `zirv ctx optimize`). `zirv ctx
optimize`'s report includes a memory-bank summary block — count, total
bytes, oldest/newest age, how many are stale, a duplicate-key check — but
**never quotes an entry's key or body**: that content is repository-scoped,
cross-session data with nothing to do with what `optimize` is reviewing, and
it is read separately from (never folded into) the surfaces sent to the
judgment model.

```toml
[memory]
enabled = true
harvest = false          # off by default; see above
max_entries = 50
max_entry_bytes = 512
max_injected_bytes = 2048       # superseded by core_max_bytes; kept only so an old config does not error
shared_enabled = true
core_max_bytes = 2048           # cap on the merged private+shared core layer folded into every session
retrieval_max_bytes = 2048      # cap on `zirv memory recall`; reserved for future context-aware session retrieval
retrieval_max_entries = 6       # max number of recalled entries, independent of bytes
```

There are two memory scopes, gated separately but not independently:
`enabled` is a master switch that disables both scopes, and `shared_enabled`
is a second, shared-only toggle underneath it -- `enabled = false` always
wins, so an operator who turned memory off before the shared scope existed
does not silently start receiving repo-controlled prompt content on
upgrade. The **private** bank still lives
under the state dir, never in the repo (the same "a checkout is not the
operator" reasoning as handoffs and mail). The **shared** bank is the
opposite by design: `<repo>/.zirv/memory/` is untrusted repository content,
one key-addressed file per entry, meant to be committed, reviewed, and
hand-edited like any other file in the checkout -- that is the whole point of
a Git-friendly shared memory bank. What stays forbidden either way is the
*configuration*, not the content: every `[memory]` key, including
`shared_enabled`, is repo-forbidden outright -- a checkout may not set any of
them to any value at all, so it can neither switch either scope's own gate on
or off for itself, raise its own caps, nor turn on automatic harvesting. Only
`~/.zirv/ctx.toml`, `ZIRV_CTX_*`, or flags (the operator) may. See
[Trust boundary](#trust-boundary) above.

### Consistent sessions

When zirv starts an agent through `wrap`, `exec`, `loop` or `resume` it injects a
small system prompt so sessions behave the same way every time. Three layers
concatenate, in order:

1. A shipped default baked into the binary: respect repo conventions, use tools
   deterministically, report failures honestly.
2. Your own additions, by session role: `~/.zirv/system-prompt.md` for an
   interactive session you drive yourself (`chat`, `wrap`, `resume`), or
   `~/.zirv/system-prompt.worker.md` for a delegated headless worker (`agent`,
   `exec`, `loop`). Both are optional, and neither role reads the
   other's file — if you had worker-facing instructions in `system-prompt.md`,
   copy them into `system-prompt.worker.md`.
3. `<repo>/.zirv/system-prompt.md`, the repository's additions (one file, both
   roles).

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

### Low-noise interactive, fail-closed headless

The permission rule is simple: **everyday and unknown commands run silently in
an interactive session; a short list of genuinely dangerous commands prompts;
a shorter irreversible, credential-exfiltrating, or zirv-self-destructive list
is refused outright. Headless sessions stay fail-closed because nobody is
present to answer.** This is a launch-flag/hook layer, not injected instruction
text, so `--simple` does not remove it—only `--no-supervise` (pure passthrough)
or the explicit opt-out below do.

- **Claude interactive:** `--permission-mode default`, native workspace/tool
  scoping, and a `Bash|PowerShell` `PreToolUse` safety hook attested on every
  Zirv launch through fingerprinted settings and immutable policy snapshots
  under `~/.zirv/runtime/` as the sole per-command gate. The hook evaluates
  both the launch snapshot and the policy resolved now, keeps the stricter
  verdict, and fails closed on a missing or tampered attestation. It emits an
  explicit `allow` for everyday and
  unclassified commands, `ask` only for the closed dangerous list, and `deny`
  for the shorter refusal list. Zirv ships conservative Design B: no blanket
  native `Bash(*)` allow. On macOS, Linux, and WSL2 the same launch layer
  enables Claude's OS sandbox in auto-allow mode, fails closed if it cannot
  start, blocks common credential paths, and scrubs cloud credentials from
  subprocesses. Native Windows receives the hook and credential rules but no
  unsupported OS-sandbox setting.
- **Claude headless:** `--permission-mode dontAsk`; ordinary allow rules are
  pre-approved and both deny and ask rules are disallowed, so no prompt can
  stall automation.
- **Codex interactive:** `--sandbox workspace-write --ask-for-approval
  on-request` when the installed CLI's own bounded capability probe documents
  it, otherwise `never`. When that CLI also advertises `--approve-for-me`,
  Zirv enables Codex's native security reviewer for boundary requests; older
  versions retain plain `on-request`. No zirv `[safety]` rule is projected per
  command.
- **Codex headless:** `--sandbox workspace-write --ask-for-approval never`.

`adapters::SHIPPED_POSTURE_ALLOW`/`_ASK`/`_DENY` are the shared source for the
built-in classifier and Claude projection. Plain `curl`/`wget`, dependency
installation, builds, commits, in-repo writes, read utilities, and commands
zirv has never seen are not prompt-worthy merely because they mutate or are
unknown. Force-push, hard reset, local ref/stash/reflog/worktree loss, recursive deletion, process termination,
registry mutation, remote HTTP mutations, infrastructure destruction and
device/partition tools ask interactively. Generated-directory cleanup,
downloads, loopback requests and dry runs stay silent. Irreversible package or
release publication/deletion, credential-file access or upload, privilege
escalation, download-to-shell pipelines, and attacks on zirv itself are denied.
The structural/semantic result is identical for Unix, `cmd.exe`, and
PowerShell spellings (including `.exe`/`.cmd` wrappers). This classifier is a
tripwire layered with the harness sandbox, not a claim that finite command
analysis can contain an arbitrary-code interpreter.

An operator's own explicit `--sandbox`/`--ask-for-approval`/`--permission-mode`/
`--disallowedTools` (passed after `--`, or via `worker.claude`/`worker.codex`'s
own trailing flags) always wins outright — zirv prepends nothing when the
launch already pins one of these.

To restore the pre-2026-08-22 behaviour (no zirv-applied flags at all; a
launch's approval/sandbox posture comes entirely from the harness's own native
config), set:

```toml
[sandbox]
enabled = false
```

or `ZIRV_CTX_SANDBOX=false`. `sandbox.enabled` is `REPO_FORBIDDEN` (see
[Trust boundary](#trust-boundary) above): a checkout cannot turn its own
sandboxing off, only the operator can.

### Command safety policy (issue #83)

`[safety]` is zirv's harness-neutral shell-command classifier. Claude projects
it through its native rule lists and `Bash|PowerShell` hook; codex currently has no verified
per-command channel and relies on its sandbox/approval boundary instead. `gh`
and `glab` (GitHub's and GitLab's CLIs) are both subcommand-based CLIs with the
same `<tool> <resource> <verb>` shape; this classifier's own read-only carve-out
recognizes a mutating `gh api` call specifically (a non-`GET` `--method`/`-X`,
or a body flag such as `-f`/`-F`/`--input`) and classifies it the same way a
destructive git or publish command is. `zirv ctx permissions`' separate
classifier checks the equivalent mutating-vs-read shape for both `gh api` and
`glab api`, using its own flag list (`-f`/`--field`/`--input`) — see
[Permission auditing and safe-list
proposals](#permission-auditing-and-safe-list-proposals-issue-178) below for turning
repeated prompts on these two CLIs into either a standing operator allow or a
proposed policy change:

```toml
[safety]
deny  = ["terraform destroy*"]   # additional deny patterns
ask   = ["kubectl delete*"]      # additional ask patterns
allow = ["just test*"]           # operator-only, REPO-FORBIDDEN
default = "ask"                  # operator-only, REPO-FORBIDDEN
interactive_default = "allow"    # operator-only, REPO-FORBIDDEN
sql = "on"                       # operator-only, REPO-FORBIDDEN
```

Rules are glob patterns (`*` matches any run of characters); a command is
matched deny-first, then ask, then allow, first match wins within a
category. Unmatched commands use `interactive_default` for an interactive
launch and `default` for a headless one. With SQL classification on, one
provably read-only `SELECT`/`EXPLAIN`/`SHOW` through a recognized client runs
silently; write-shaped, multi-statement, stdin/script-fed, malformed, or CTE
input asks conservatively.

The analyzer evaluates the most restrictive result across quote-aware compound
segments (`;`, `&`, `&&`, `||`, pipes and newlines), nested
`sh`/`bash`/`zsh`/`cmd`/PowerShell inline wrappers, `$()` and backtick command
substitutions, and every semantic candidate it finds. The semantic layer
recognizes SQL writes, remote network mutations, credential-file access,
recursive deletion, infrastructure/service destruction, and irreversible
package/release operations using case-folded executable basenames, so native
Windows and Unix wrapper spellings reach the same verdict. Quoted
command-looking text remains data. This tripwire is bounded against hostile
input and deliberately does not decode obfuscation, expand variables, or read
dynamically sourced scripts; the harness sandbox is the containment boundary
beneath it.

Each supervised Claude decision appends a privacy-preserving audit record under
the platform state directory's `logs/safety-decisions/` UTC-day bucket. Records
contain the verdict, matched rule/origin, launch/current policy fingerprints,
attestation status and SHA-256 of the command—never the raw command, source,
paths, tokens, or shell secrets.

`deny`/`ask` may be extended by a repo checkout (narrowing is always safe—both
are checked before `allow`); `allow`, both defaults, and `sql` may not. A
checkout cannot grant itself approval, loosen either launch posture, or turn
off the conservative SQL narrowing.

Three verbs work with the resolved policy directly:

```sh
zirv ctx safety check --mode interactive -- rm -rf /  # exits 0/1/2 for allow/ask/deny
zirv ctx safety list                     # the effective merged policy, with each rule's origin
zirv ctx safety explain --mode headless -- git push --force  # rule plus launch consequence
```

`zirv ctx safety check` (with no trailing command) is also what `zirv setup
apply` wires into claude's `PreToolUse` hook for `Bash` and `PowerShell` calls,
so the same
evaluator zirv's own CLI uses is what claude consults before running a
command — see [Context Management](#context-management-zirv-ctx).

### Permission auditing and safe-list proposals (issue #178)

`zirv ctx permissions` turns recent transcripts into a report on which
commands kept needing a human, so a policy decision can be made about the
*family* of command instead of clicking through the same prompt every session:

```bash
zirv ctx permissions audit --agent codex --sessions 5     # read-only report
zirv ctx permissions audit --agent claude --json
zirv ctx permissions compile --agent codex --dry-run       # preview eligible allows; writes nothing
zirv ctx permissions propose --agent claude --dry-run       # preview proposed issues; files/comments nothing
```

- **`audit`** is strictly read-only. It extracts every escalated/denied
  permission request from the sampled transcripts (codex: `require_escalated`
  exec requests; claude: headless `dontAsk` denials and interactive
  sandbox-escape asks), groups them by a normalized command family (`gh pr`,
  `cargo publish`, ...), and reports each group's sample command, cause, and a
  reusability verdict: whether a saved approval for this family would
  plausibly match the *next* equivalent invocation, or whether it collapses to
  a one-off (a long literal payload, or a pipe into `jq`/`grep`/`awk`/`sed`
  whose own argument is what varies).
- **`compile`** runs the same audit, then *writes* eligible families as
  standing `[safety] allow` entries in the operator's own `~/.zirv/ctx.toml`.
  A family is eligible only when it is reusable, has at least two normalized
  tokens (a bare program name is too coarse — it would authorize whatever a
  future invocation is told to run), and is not a **protected** family:
  destructive git (`git push --force`, `git reset --hard`, `git rebase`, ...),
  a global binary/config install, a credential/secret command, a
  publish/release action, an interpreter/shell/remote-exec program, or a
  mutating `gh api`/`glab api` call always stay prompting regardless of
  reusability. `--agent codex` compiles are still written as real `[safety]`
  entries — claude reads them too — but a printed caveat makes clear this
  changes nothing about codex's own launch posture, since codex has no
  per-command approval hook yet for zirv to pin against.
- **`propose`** is the mirror image: instead of escalated/denied requests, it
  looks at operator-**approved** prompts and classifies which are so clearly
  safe they should never have prompted at all — today, only a documented
  `gh`/`glab` collaboration verb (`SAFE_COLLABORATION_VERBS`: creating or
  updating a PR/issue, or commenting — never merge, close/reopen, delete,
  release, auth, or an arbitrary API call), matched at the exact
  `(program, resource, verb)` triple, unchained and unpiped. Evidence is
  grouped by family; a family with no open proposal issue yet files one, and a
  family whose evidence has changed since the last run gets one updated
  comment on its existing issue — a family already reported with unchanged
  evidence is skipped, so a re-run over an overlapping transcript window never
  re-comments the same evidence.

`propose` is **disabled by default** — it auto-files issues on a public GitHub
repository. Enable it explicitly, operator-side only:

```toml
# ~/.zirv/.settings.toml
[permissions]
propose_enabled = true
```

Above a threshold of 5 total requests in one audit, `zirv ctx optimize`'s own
friction pass surfaces the same summary as a finding, well before a session
reaches the volume that originally motivated this feature.

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
