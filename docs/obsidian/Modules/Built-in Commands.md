---
last-verified: 2026-08-20
---

# Built-in Commands

## Quick Reference

- **Files:** `src/main.rs`, `src/input.rs`, `src/commands/mod.rs`, `src/commands/create.rs`, `src/commands/init.rs`, `src/commands/help.rs`, `src/commands/version.rs`, `src/commands/workflow/`
- **Used by:** entry point — nothing calls into these except the OS invoking `zirv`
- **Depends on:** [[Script Runner]] (`execute`), [[Utilities]] (`file_to_script`, `home_dir`, `Shortcuts`, `is_reserved_command`, `candidate_names_in_dir`, `suggest_matches`), [[Workflows]] (`workflow::dispatch`), [[Ctx Subsystem]] (`ctx::dispatch`)
- **Tests:** inline `#[cfg(test)] mod tests` in each file (`main::tests`, `input::tests`, `create::tests`, `init::tests`, `help::tests`; `version.rs` has no dedicated module dependency beyond its own version string)
- **If changed:** [[Script Resolution]], [[Shortcuts]], [[Getting Started]], [[Ctx Supervisors]] (the dashboard `zirv chat` opens on a capable terminal)
- **Gotchas:** `zirv ctx …`, `zirv chat`/`zirv agent`, `zirv skill`/`workflow`/`test`/`verify`/`artifact`, a bare `zirv`, and top-level `zirv --help`/`-h` are all matched against raw `argv` *before* legacy clap/script resolution — a same-named `.zirv/*.yaml` script is permanently shadowed. A bare `zirv` used to be a clap usage error (exit 2, missing the required `command` argument) — it is now an alias, a deliberate behavior change (see below).

## Purpose

`main.rs` is the CLI's dispatch table: fixed built-ins (`help`, `version`, `init`, `create`, `ctx`, `chat`, `agent`, `skill`, `workflow`, `test`, `verify`, `artifact`) are checked before anything falls through to script lookup.

## How It Works

### Dispatch order (`main.rs`)

1. **Raw-argv `ctx` check** — `argv[1] == "ctx"` routes straight to `commands::ctx::dispatch`, bypassing `Input`/clap entirely. This is why `.zirv/ctx.yaml` can never be reached as a script (see `help.rs`'s "shadowed" marker below) and why a malformed `.zirv/ctx.toml` never breaks script lookup — `main.rs` never gets that far for `ctx`.
2. **Raw-argv workflow command check** — `skill`, `workflow`, `test`, `verify`, and `artifact` route to `commands::workflow::dispatch`, whose clap tree is independent from both `ctx` and legacy scripts. These names are reserved and `.zirv/verify.toml` is excluded from script resolution/listing.
3. **Raw-argv `chat`/`agent` alias check** (`top_level_ctx_alias`) — rewrites argv into the existing ctx verb tree.
4. **Bare invocation** (`argv.len() == 1`, i.e. no arguments at all) — `bare_invocation_target(zirv_dir_exists, stdin_is_tty)` chooses chat or help.
5. **Raw-argv top-level help check** (`is_top_level_help`) — `argv[1]` is exactly `--help` or `-h`.
6. **`Input::parse()`** — clap parses the legacy built-in/script surface.
7. **`misplaced_create_flag` guard** — refuses create-only flags on another command.
8. **Built-in match** on `input.command`: `help|h`, `version|v`, `init|i`, `create|c`.
9. **Fallback**: resolve `input.command` as a script and run it through `script_runner::execute`.

An explicit `zirv chat`/`zirv agent` (step 3) is not subject to the bare-invocation's terminal rule (step 4).

### CLI shape (`input.rs`)

`Input` (derives `clap::Parser`): `command: String` (positional), `params: Vec<String>` (`num_args 0..`), `dry_run: bool`, and create-only `name`/`shortcut`/`global: Option<T>` — all `None` by default, which is also how `create` distinguishes "flag not given, prompt interactively" from "flag given". `misplaced_create_flag` returns the first of those three that's `Some` when the command isn't `create`.

`Input::get_file_path` resolution order: (1) the literal string as an existing path, (2) `./.zirv/<command>.<ext>` for each of `SUPPORTED_EXTENSIONS`, (3) `./.zirv/.shortcuts.yaml` mapping `<command>` to a target file, (4) the same two steps under `~/.zirv`. A `.shortcuts.yaml` that fails to read or parse is a warning, not a fatal error — lookup falls through to whatever else might match rather than taking down the whole command. A miss produces `not_found_error`, which offers up to three "did you mean" suggestions (via `utils::suggest_matches`) drawn from both local and global script/shortcut names, plus a pointer to `zirv help`.

### `create` (`commands/create.rs`)

`CreateOptions { name, shortcut, global: Option<T> }` — any field left `None` prompts interactively via `dialoguer`; all three `Some` makes the whole command non-interactive (used by `--name`/`--shortcut`/`--global` on the CLI, and by tests). `validate_name` rejects path separators and a bare `".."` — the name becomes a path segment under `.zirv/`, so `--name ../../escaped` must not be allowed to write outside it. A name or shortcut colliding with a reserved built-in command name (`utils::is_reserved_command`) warns and asks for confirmation interactively, or hard-errors non-interactively (nothing to confirm with). An existing `.shortcuts.yaml` that fails to parse is never silently rewritten — that would drop every shortcut it holds — so it's an error non-interactively and a confirm-to-replace prompt interactively. Writes `<name>.yaml` from a commented `DEFAULT_TEMPLATE` and, if a shortcut was given, upserts it into `.shortcuts.yaml`.

### `init` (`commands/init.rs`)

`init_zirv_with(confirm_fn)` always ensures `~/.zirv` and its default `.shortcuts.yaml` exist, then asks (via the injected `confirm_fn`, real code uses `dialoguer::Confirm`) whether to also create `./.zirv`. `init_zirv` is the production wrapper.

### `help` (`commands/help.rs`)

`show_help` writes a builtins/usage block, then local and global scripts/shortcuts. Script listing skips `utils::RESERVED_ZIRV_FILES`, now including `verify.toml`. Reserved command collisions include the five workflow commands and are annotated `(shadowed by a built-in command, unreachable)`.

### `version` (`commands/version.rs`)

One line: `Version: {CARGO_PKG_VERSION}`.

### `zirv chat`/bare `zirv` default to the dashboard on a capable terminal

`zirv chat` (and bare `zirv`, once step 3 above routes to it) no longer always lands on a plain `wrap` passthrough session: on a real terminal at least 80×20 with VT processing available and `cfg.dash.enabled` (the default), it opens the dashboard session multiplexer instead — see [[Ctx Supervisors]]'s "The dashboard (`dash`)" section for the pane model, prefix keys, and quit/restore roster. `--simple` (or a terminal too small, or `[dash] enabled = false`) falls back to today's plain `wrap` chrome unchanged; a too-small terminal that would otherwise qualify prints a one-line notice naming the floor before falling back. `[chat] model`/`ZIRV_CTX_CHAT_MODEL`, when set, selects the model for the orchestrator session either way (dashboard or plain `wrap`) and is shown in the launch banner/dashboard header — see [[Ctx Adapters]] and [[Untrusted Configuration]] for why this one model key is deliberately not repo-forbidden, unlike every other model key in `ctx.toml`.
