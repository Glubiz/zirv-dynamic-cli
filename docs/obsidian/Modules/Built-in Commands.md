---
last-verified: 2026-08-12
---

# Built-in Commands

## Quick Reference

- **Files:** `src/main.rs`, `src/input.rs`, `src/commands/mod.rs`, `src/commands/create.rs`, `src/commands/init.rs`, `src/commands/help.rs`, `src/commands/version.rs`
- **Used by:** entry point — nothing calls into these except the OS invoking `zirv`
- **Depends on:** [[Script Runner]] (`execute`), [[Utilities]] (`file_to_script`, `home_dir`, `Shortcuts`, `is_reserved_command`, `candidate_names_in_dir`, `suggest_matches`), [[Ctx Subsystem]] (`ctx::dispatch`, intercepted before clap runs at all)
- **Tests:** inline `#[cfg(test)] mod tests` in each file (`main::tests`, `input::tests`, `create::tests`, `init::tests`, `help::tests`; `version.rs` has no dedicated module dependency beyond its own version string)
- **If changed:** [[Script Resolution]], [[Shortcuts]], [[Getting Started]]
- **Gotchas:** `zirv ctx …`, `zirv chat`/`zirv agent`, a bare `zirv`, and top-level `zirv --help`/`-h` are all matched against raw `argv` *before* clap parses anything — a `.zirv/ctx.yaml` (or `chat.yaml`/`agent.yaml`) script is permanently shadowed, and clap's own auto-generated help for the `Input` struct never fires. A bare `zirv` used to be a clap usage error (exit 2, missing the required `command` argument) — it is now an alias, a deliberate behavior change (see below).

## Purpose

`main.rs` is the CLI's dispatch table: a fixed set of built-ins (`help`, `version`, `init`, `create`, `ctx`, `chat`, `agent`) checked before anything falls through to "treat this word as a script name in `.zirv/`".

## How It Works

### Dispatch order (`main.rs`)

1. **Raw-argv `ctx` check** — `argv[1] == "ctx"` routes straight to `commands::ctx::dispatch`, bypassing `Input`/clap entirely. This is why `.zirv/ctx.yaml` can never be reached as a script (see `help.rs`'s "shadowed" marker below) and why a malformed `.zirv/ctx.toml` never breaks script lookup — `main.rs` never gets that far for `ctx`.
2. **Raw-argv `chat`/`agent` alias check** (`top_level_ctx_alias`) — `argv[1] == "chat"` or `"agent"` rewrites argv (`rewrite_ctx_alias_args`) into `["ctx", <verb>, ...rest]` and routes through the same `ctx::dispatch` the raw `ctx` check above uses, so the alias gets the full ctx verb tree for free: subcommand parsing, `--help` exiting 0, and the same parse-failure classification `zirv ctx chat --help` would get.
3. **Bare invocation** (`argv.len() == 1`, i.e. no arguments at all) — `bare_invocation_target(zirv_dir_exists, stdin_is_tty)` is a pure function of two caller-supplied facts (`zirv_dir_present` checks `./.zirv` and `~/.zirv`; `std::io::IsTerminal` checks stdin) deciding `BareTarget::Chat` (routes to `ctx::dispatch(["ctx", "chat"])`) or `BareTarget::Help` (prints `show_help`, **exit 0**). This is the deliberate behavior change: before this, `Input::command` being a required positional meant a bare `zirv` was a clap usage error exiting 2.
4. **Raw-argv top-level help check** (`is_top_level_help`) — `argv[1]` is exactly `--help` or `-h` (not a script's own `--help` parameter, which lands later). Bypasses clap so `zirv help`'s rich script listing runs instead of clap's generated help for `Input`. Argv length 2 here means this never overlaps with the bare-invocation check above (length 1) or the `ctx`/alias checks (which match on `argv[1]`'s value, not `--help`).
5. **`Input::parse()`** — clap parses the rest.
6. **`misplaced_create_flag` guard** — `--name`/`--shortcut`/`--global` live on the shared `Input` struct so clap accepts them for *every* command; used outside `create`/`c` they are refused with a named error instead of being silently swallowed as (and eating) the next positional argument.
7. **Built-in match** on `input.command`: `help|h`, `version|v`, `init|i`, `create|c` each dispatch to their module and return.
8. **Fallback**: resolve `input.command` as a script via `Input::get_file_path`, parse it with `utils::file_to_script`, and run it through `script_runner::execute`.

An explicit `zirv chat`/`zirv agent` (step 2) is not subject to the bare-invocation's stdin-is-a-terminal rule (step 3) at all — they are different checks on different argv shapes, so a piped `zirv chat` still launches chat rather than falling back to help.

### CLI shape (`input.rs`)

`Input` (derives `clap::Parser`): `command: String` (positional), `params: Vec<String>` (`num_args 0..`), `dry_run: bool`, and create-only `name`/`shortcut`/`global: Option<T>` — all `None` by default, which is also how `create` distinguishes "flag not given, prompt interactively" from "flag given". `misplaced_create_flag` returns the first of those three that's `Some` when the command isn't `create`.

`Input::get_file_path` resolution order: (1) the literal string as an existing path, (2) `./.zirv/<command>.<ext>` for each of `SUPPORTED_EXTENSIONS`, (3) `./.zirv/.shortcuts.yaml` mapping `<command>` to a target file, (4) the same two steps under `~/.zirv`. A `.shortcuts.yaml` that fails to read or parse is a warning, not a fatal error — lookup falls through to whatever else might match rather than taking down the whole command. A miss produces `not_found_error`, which offers up to three "did you mean" suggestions (via `utils::suggest_matches`) drawn from both local and global script/shortcut names, plus a pointer to `zirv help`.

### `create` (`commands/create.rs`)

`CreateOptions { name, shortcut, global: Option<T> }` — any field left `None` prompts interactively via `dialoguer`; all three `Some` makes the whole command non-interactive (used by `--name`/`--shortcut`/`--global` on the CLI, and by tests). `validate_name` rejects path separators and a bare `".."` — the name becomes a path segment under `.zirv/`, so `--name ../../escaped` must not be allowed to write outside it. A name or shortcut colliding with a reserved built-in command name (`utils::is_reserved_command`) warns and asks for confirmation interactively, or hard-errors non-interactively (nothing to confirm with). An existing `.shortcuts.yaml` that fails to parse is never silently rewritten — that would drop every shortcut it holds — so it's an error non-interactively and a confirm-to-replace prompt interactively. Writes `<name>.yaml` from a commented `DEFAULT_TEMPLATE` and, if a shortcut was given, upserts it into `.shortcuts.yaml`.

### `init` (`commands/init.rs`)

`init_zirv_with(confirm_fn)` always ensures `~/.zirv` and its default `.shortcuts.yaml` exist, then asks (via the injected `confirm_fn`, real code uses `dialoguer::Confirm`) whether to also create `./.zirv`. `init_zirv` is the production wrapper.

### `help` (`commands/help.rs`)

`show_help` writes: a builtins/usage block (hardcoded, not generated from clap — intercepting `--help` took away clap's auto-generated help, so this exists specifically to keep every flag discoverable), then local `.zirv/` scripts and shortcuts, then global `~/.zirv` scripts and shortcuts. The builtins block names `chat`/`agent` alongside the older built-ins and states the bare-invocation rule in one line, so both are discoverable from `zirv help` and not just from this page. Script listing skips every name in `utils::RESERVED_ZIRV_FILES` (`.shortcuts.yaml`, `ctx.toml`, `.settings.toml`), compared case-insensitively (`utils::is_reserved_zirv_file`, since NTFS/APFS resolve file names case-insensitively too) — parsing one of these as a `Script` used to fail the whole listing. Any script file or shortcut whose name collides with a reserved built-in (`utils::RESERVED_COMMANDS`: `help`/`h`, `version`/`v`, `init`/`i`, `create`/`c`, `ctx`, `chat`, `agent`) is annotated `(shadowed by a built-in command, unreachable)`, since `main.rs`'s built-in match runs before `.zirv/` is ever consulted.

### `version` (`commands/version.rs`)

One line: `Version: {CARGO_PKG_VERSION}`.
