---
last-verified: 2026-08-31
---

# Architecture Overview

## Quick Reference

- Single binary crate `zirv`. Entry point `src/main.rs` intercepts `ctx`, workflow commands, aliases, and top-level help before legacy clap/script resolution.
- Module map: `main.rs` (dispatch) -> top-level built-ins, `commands/workflow/`, `commands/ctx/`, or `input.rs`/`script_runner/` for legacy scripts. `commands/workflow/` owns provider-neutral development methodology and durable lifecycle state; `commands/ctx/` owns agent supervision/context.
- **If changed:** update [[Script Resolution]] (dispatch order lives in `main.rs`), [[Technology Stack]] (if a module gains/drops a dependency), [[Script Runner]] and [[Ctx Subsystem]] (module-level detail for their halves of the tree).
- **Gotchas:** `zirv ctx` is matched on raw `argv[1]`, ahead of clap parsing — a `.zirv/commands/ctx.yaml` script named `ctx` can never be reached (see [[Script Resolution]]). `zirv --help`/`-h` is also intercepted on raw argv so clap's auto-generated help never fires. Scripts resolve from `.zirv/commands/` (and `~/.zirv/commands/`), not the `.zirv` root, as of zirv 3.0 (issue #212) — the root holds only config (`ctx.toml`, `.settings.toml`, `verify.toml`, `.shortcuts.yaml`, `system-prompt.md`, `context/`, `memory/`).

## Module map

```
src/
├── main.rs              # entry point: ctx interception, help interception, built-in dispatch
├── input.rs              # clap Input struct; script path resolution (get_file_path)
├── utils.rs               # SUPPORTED_EXTENSIONS/SCRIPT_DIR_NAME/COMMANDS_DIR_NAME, file parsing, Shortcuts, suggestions
├── output.rs              # console output helpers (step/error/warn/dry_run)
├── commands/
│   ├── create.rs          # `zirv create` / `c` — interactive script scaffolding, writes into .zirv/commands/
│   ├── help.rs             # `zirv help` / `h` — usage + script/shortcut listing
│   ├── init.rs             # `zirv init` / `i` — creates a .zirv directory and its commands/ subdirectory
│   ├── version.rs          # `zirv version` / `v`
│   ├── workflow/            # skills, workflows, risk, tests, reviews, artifacts, telemetry
│   └── ctx/                # `zirv ctx <verb>` — AI-agent context management subsystem
└── script_runner/
    ├── script.rs           # Script model + run loop
    ├── command.rs          # single shell Command: substitution, invoke, fallback
    ├── command_types.rs    # CommandTypes enum: Command / Commands / Agent
    ├── agent_command.rs    # AgentCommand step (supervised AI-agent step)
    ├── options.rs           # per-command Options (interactive, os filter, delay, fallback)
    ├── fallback_command.rs  # fallback step executed on failure
    ├── operating_system.rs  # OperatingSystem enum + is_current()
    ├── secret.rs             # Secret { name, env_var }
    └── mod.rs                # execute(): build_context + Script::run
```

See [[Script Runner]] for the execution-loop internals, [[Workflows]] for the development lifecycle, [[Ctx Subsystem]] for the `commands/ctx/` module breakdown, and [[Utilities]] for what lives in `utils.rs`.

## Execution flow

```mermaid
graph TB
    A[argv collected in main] --> B{argv[1] == 'ctx'?}
    B -- yes --> C[ctx::dispatch - exits process]
    B -- no --> W{workflow command?}
    W -- yes --> WC[workflow::dispatch]
    W -- no --> D{is_top_level_help:<br/>argv[1] in --help/-h?}
    D -- yes --> E[show_help, return]
    D -- no --> F[Input::parse via clap]
    F --> G{misplaced create-only flag<br/>on a non-create command?}
    G -- yes --> H[error, exit 1]
    G -- no --> I{input.command}
    I -- help/h --> E
    I -- version/v --> J[get_version]
    I -- init/i --> K[init_zirv]
    I -- create/c --> L[create_script]
    I -- other --> M[input.get_file_path]
    M --> N[Script Resolution:<br/>literal path, local .zirv,<br/>global ~/.zirv, shortcuts]
    N --> O[file_to_script:<br/>parse YAML/JSON/TOML]
    O --> P[script_runner::execute]
    P --> Q[build_context:<br/>params + secrets]
    Q --> R[Script::run loop<br/>over commands]
```

`ctx` interception happens on raw `argv`, ahead of everything else, including clap parsing — see [[Script Resolution]] for why that makes a same-named `.zirv/ctx.*` script permanently unreachable. Steps M–O are detailed in [[Script Resolution]]; step R is detailed in [[Script Runner]] and the file format it consumes is [[Script Files]].

## Where `zirv ctx` fits

`commands/ctx/mod.rs` defines its own `clap::Parser` (`CtxCli`) and verb enum (`CtxVerb`), parsed independently from the top-level `Input` struct once `main.rs` has handed off control. Its own dispatch (`dispatch(&argv[1..])`) never returns to `main.rs` — it exits the process directly with a verb-specific or clap-derived exit code. The subsystem is large enough to warrant its own architecture notes; see [[Ctx Subsystem]], [[Ctx Supervisors]], [[Ctx Adapters]], and [[Rot Engine]].
