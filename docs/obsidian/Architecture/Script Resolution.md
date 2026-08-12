---
last-verified: 2026-08-12
---

# Script Resolution

## Quick Reference

- Resolution order: **1)** `ctx` interception on raw argv, **2)** top-level `--help`/`-h` interception on raw argv, **3)** clap-parsed built-ins (`help`, `version`, `init`, `create` and their short aliases), **4)** literal file path, **5)** local `.zirv/` (direct extension match, then `.shortcuts.yaml`), **6)** global `~/.zirv/` (same two steps).
- Supported script extensions, in match order: `yaml`, `yml`, `json`, `toml` (`utils::SUPPORTED_EXTENSIONS`). Script directory name is always `.zirv` (`utils::SCRIPT_DIR_NAME`).
- **If changed:** update [[Architecture Overview]]'s flow diagram and [[Shortcuts]] if the shortcut fallback logic changes; update [[Utilities]] if `candidate_names_in_dir`/`suggest_matches` change.
- **Gotchas:** the reserved built-in names (`help`, `h`, `version`, `v`, `init`, `i`, `create`, `c`, `ctx` — `utils::RESERVED_COMMANDS`) are matched *before* any script lookup, in `main.rs`. A script or shortcut sharing one of these names is permanently unreachable; `zirv help` flags this in its listing as "shadowed by a built-in command, unreachable", but nothing prevents creating the file in the first place.

## Order of resolution

### 1. `ctx` interception (before clap)

`main.rs` checks `argv.get(1) == Some("ctx")` against raw `argv`, before `Input::parse()` ever runs. If it matches, control passes straight to `commands::ctx::dispatch(&argv[1..])` and the process exits with its return code — script lookup never happens. This is why a `.zirv/ctx.yaml` (or `.json`/`.toml`) script is shadowed: see [[Ctx Subsystem]].

### 2. Top-level help interception (before clap)

`is_top_level_help` checks whether `argv[1]` is exactly `--help` or `-h` (not a script's own `--help` parameter — `zirv build --help` does *not* match, since the flag isn't in the command slot). When it matches, `show_help` runs and returns immediately, so clap's auto-generated help for the `Input` struct is never reached.

### 3. Clap-parsed built-ins

`Input::parse()` runs, then `main.rs` matches on `input.command`:

| Command | Aliases | Handler |
|---|---|---|
| `help` | `h` | `commands::help::show_help` |
| `version` | `v` | `commands::version::get_version` |
| `init` | `i` | `commands::init::init_zirv` |
| `create` | `c` | `commands::create::create_script` |

Each returns/exits before script resolution begins. Before this match, `main.rs` also rejects `--name`/`--shortcut`/`--global` when the command isn't `create`/`c` — those flags live on the shared `Input` struct (so clap accepts them for every command) but only make sense for `create`; letting them through silently would eat the next positional argument as their value.

### 4–6. `Input::get_file_path`

Anything that isn't a built-in falls through to `input.get_file_path()`:

1. **Literal path**: if `input.command` is itself an existing file path (e.g. `zirv path/to/script.yaml`), it is canonicalized and used directly — no `.zirv/` involved.
2. **Local `.zirv/`**: `find_script_in_dir(".zirv", name)` tries `<name>.yaml`, `<name>.yml`, `<name>.json`, `<name>.toml` in that order. First existing file wins. Before checking existence, each candidate is skipped if it matches `utils::RESERVED_ZIRV_FILES` (`.shortcuts.yaml`, `ctx.toml`, `.settings.toml`), compared case-insensitively via `utils::is_reserved_zirv_file` — so `zirv .settings` cannot resolve `.zirv/.settings.toml` (zirv's own agent-gate config, see [[Ctx Adapters]]) as if it were a script, the same way `ctx.toml` was already excluded from script *listing* (`help.rs`).
3. **Local shortcuts**: if no direct extension match, and `.zirv/.shortcuts.yaml` exists, it's parsed and looked up by `name`. A malformed shortcuts file is *not* fatal here — it's warned about and ignored for this lookup, so a direct match elsewhere in the same directory can still succeed. The mapped value is tried first as a literal path relative to the directory, then with each supported extension appended.
4. **Global `~/.zirv/`**: steps 2–3 repeat against `home_dir().join(".zirv")` (`home_dir` reads `$HOME`, falling back to `%USERPROFILE%`).
5. **Not found**: `not_found_error` builds a message enriched with up to 3 "did you mean" suggestions — Levenshtein distance over the union of local and global script stems plus shortcut keys — plus a pointer to `zirv help`.

Local scripts effectively shadow global scripts of the same name, since the local directory is always checked first.

### 7. Parsing the resolved file

Once a path is resolved, `file_to_script` reads it and dispatches on its extension to `serde_yaml_ng`, `serde_json`, or `toml`, producing a `Script` (see [[Script Files]]).

## Diagram

```mermaid
graph TB
    A[zirv command params...] --> B{argv[1] == 'ctx'?}
    B -- yes --> C[commands::ctx::dispatch]
    B -- no --> D{argv[1] in --help/-h?}
    D -- yes --> E[show_help]
    D -- no --> F[clap parses Input]
    F --> G{command in<br/>help/version/init/create?}
    G -- yes --> H[run that built-in]
    G -- no --> I{literal path exists?}
    I -- yes --> J[use it directly]
    I -- no --> K[.zirv/name.ext ?]
    K -- found --> L[resolved]
    K -- not found --> M[.zirv/.shortcuts.yaml lookup]
    M -- found --> L
    M -- not found --> N[~/.zirv/name.ext ?]
    N -- found --> L
    N -- not found --> O[~/.zirv/.shortcuts.yaml lookup]
    O -- found --> L
    O -- not found --> P[not-found error<br/>+ did-you-mean suggestions]
```

See also [[Shortcuts]] for `.shortcuts.yaml` mechanics in detail, and [[Utilities]] for `suggest_matches`/`candidate_names_in_dir`.
