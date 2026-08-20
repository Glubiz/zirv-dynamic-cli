---
last-verified: 2026-08-20
---

# Built-in Commands

## Quick Reference

- **Files:** `src/main.rs`, `src/input.rs`, `src/commands/mod.rs`, `src/commands/create.rs`, `src/commands/init.rs`, `src/commands/help.rs`, `src/commands/version.rs`
- **Used by:** entry point — nothing calls into these except the OS invoking `zirv`
- **Depends on:** [[Script Runner]] (`execute`), [[Utilities]] (`file_to_script`, `home_dir`, `Shortcuts`, `is_reserved_command`, `candidate_names_in_dir`, `suggest_matches`), [[Ctx Subsystem]] (`ctx::dispatch`, `ctx::memory_cli::dispatch`, both intercepted before clap runs at all)
- **Tests:** inline `#[cfg(test)] mod tests` in each file (`main::tests`, `input::tests`, `create::tests`, `init::tests`, `help::tests`; `version.rs` has no dedicated module dependency beyond its own version string)
- **If changed:** [[Script Resolution]], [[Shortcuts]], [[Getting Started]], [[Ctx Supervisors]] (the dashboard `zirv chat` opens on a capable terminal)
- **Gotchas:** `zirv ctx …`, `zirv memory …`, `zirv chat`/`zirv agent`, a bare `zirv`, and top-level `zirv --help`/`-h` are all matched against raw `argv` *before* clap parses anything — a `.zirv/ctx.yaml` (or `memory.yaml`/`chat.yaml`/`agent.yaml`) script is permanently shadowed, and clap's own auto-generated help for the `Input` struct never fires. A bare `zirv` used to be a clap usage error (exit 2, missing the required `command` argument) — it is now an alias, a deliberate behavior change (see below).

## Purpose

`main.rs` is the CLI's dispatch table: a fixed set of built-ins (`help`, `version`, `init`, `create`, `ctx`, `memory`, `chat`, `agent`) checked before anything falls through to "treat this word as a script name in `.zirv/`".

## How It Works

### Dispatch order (`main.rs`)

1. **Raw-argv `ctx` check** — `argv[1] == "ctx"` routes straight to `commands::ctx::dispatch`, bypassing `Input`/clap entirely. This is why `.zirv/ctx.yaml` can never be reached as a script (see `help.rs`'s "shadowed" marker below) and why a malformed `.zirv/ctx.toml` never breaks script lookup — `main.rs` never gets that far for `ctx`.
2. **Raw-argv `memory` check** (`is_top_level_memory`) — `argv[1] == "memory"` routes straight to `commands::ctx::memory_cli::dispatch`, the same bypass-clap-entirely treatment as `ctx` above. Unlike the `chat`/`agent` aliases below, `memory` is not a 1:1 rewrite into a single `ctx` verb — it carries its own verb tree (`status`/`list`/`recall`/`remember`/`forget`/`verify`) and its own `clap::Parser` (`MemoryCli`), dispatched independently of `CtxCli`. See [[Ctx Subsystem]] for the verb-level detail.
3. **Raw-argv `chat`/`agent` alias check** (`top_level_ctx_alias`) — `argv[1] == "chat"` or `"agent"` rewrites argv (`rewrite_ctx_alias_args`) into `["ctx", <verb>, ...rest]` and routes through the same `ctx::dispatch` the raw `ctx` check above uses, so the alias gets the full ctx verb tree for free: subcommand parsing, `--help` exiting 0, and the same parse-failure classification `zirv ctx chat --help` would get.
4. **Bare invocation** (`argv.len() == 1`, i.e. no arguments at all) — `bare_invocation_target(zirv_dir_exists, stdin_is_tty)` is a pure function of two caller-supplied facts (`zirv_dir_present` checks `./.zirv` and `~/.zirv`; `std::io::IsTerminal` checks stdin) deciding `BareTarget::Chat` (routes to `ctx::dispatch(["ctx", "chat"])`) or `BareTarget::Help` (prints `show_help`, **exit 0**). This is the deliberate behavior change: before this, `Input::command` being a required positional meant a bare `zirv` was a clap usage error exiting 2. As of the dashboard sweep, the `chat` this routes to is no longer always a plain `wrap` passthrough — see the note below.
5. **Raw-argv top-level help check** (`is_top_level_help`) — `argv[1]` is exactly `--help` or `-h` (not a script's own `--help` parameter, which lands later). Bypasses clap so `zirv help`'s rich script listing runs instead of clap's generated help for `Input`. Argv length 2 here means this never overlaps with the bare-invocation check above (length 1) or the `ctx`/`memory`/alias checks (which match on `argv[1]`'s value, not `--help`).
6. **`Input::parse()`** — clap parses the rest.
7. **`misplaced_create_flag` guard** — `--name`/`--shortcut`/`--global` live on the shared `Input` struct so clap accepts them for *every* command; used outside `create`/`c` they are refused with a named error instead of being silently swallowed as (and eating) the next positional argument.
8. **Built-in match** on `input.command`: `help|h`, `version|v`, `init|i`, `create|c` each dispatch to their module and return.
9. **Fallback**: resolve `input.command` as a script via `Input::get_file_path`, parse it with `utils::file_to_script`, and run it through `script_runner::execute`.

An explicit `zirv chat`/`zirv agent` (step 3) is not subject to the bare-invocation's stdin-is-a-terminal rule (step 4) at all — they are different checks on different argv shapes, so a piped `zirv chat` still launches chat rather than falling back to help. `zirv memory` (step 2) is unaffected by the bare-invocation rule for the same reason.

### CLI shape (`input.rs`)

`Input` (derives `clap::Parser`): `command: String` (positional), `params: Vec<String>` (`num_args 0..`), `dry_run: bool`, and create-only `name`/`shortcut`/`global: Option<T>` — all `None` by default, which is also how `create` distinguishes "flag not given, prompt interactively" from "flag given". `misplaced_create_flag` returns the first of those three that's `Some` when the command isn't `create`.

`Input::get_file_path` resolution order: (1) the literal string as an existing path, (2) `./.zirv/<command>.<ext>` for each of `SUPPORTED_EXTENSIONS`, (3) `./.zirv/.shortcuts.yaml` mapping `<command>` to a target file, (4) the same two steps under `~/.zirv`. A `.shortcuts.yaml` that fails to read or parse is a warning, not a fatal error — lookup falls through to whatever else might match rather than taking down the whole command. A miss produces `not_found_error`, which offers up to three "did you mean" suggestions (via `utils::suggest_matches`) drawn from both local and global script/shortcut names, plus a pointer to `zirv help`.

### `create` (`commands/create.rs`)

`CreateOptions { name, shortcut, global: Option<T> }` — any field left `None` prompts interactively via `dialoguer`; all three `Some` makes the whole command non-interactive (used by `--name`/`--shortcut`/`--global` on the CLI, and by tests). `validate_name` rejects path separators and a bare `".."` — the name becomes a path segment under `.zirv/`, so `--name ../../escaped` must not be allowed to write outside it. A name or shortcut colliding with a reserved built-in command name (`utils::is_reserved_command`) warns and asks for confirmation interactively, or hard-errors non-interactively (nothing to confirm with). An existing `.shortcuts.yaml` that fails to parse is never silently rewritten — that would drop every shortcut it holds — so it's an error non-interactively and a confirm-to-replace prompt interactively. Writes `<name>.yaml` from a commented `DEFAULT_TEMPLATE` and, if a shortcut was given, upserts it into `.shortcuts.yaml`.

### `init` (`commands/init.rs`)

`init_zirv_with(confirm_fn)` always ensures `~/.zirv` and its default `.shortcuts.yaml` exist, then asks (via the injected `confirm_fn`, real code uses `dialoguer::Confirm`) whether to also create `./.zirv`. `init_zirv` is the production wrapper.

### `help` (`commands/help.rs`)

`show_help` writes: a builtins/usage block (hardcoded, not generated from clap — intercepting `--help` took away clap's auto-generated help, so this exists specifically to keep every flag discoverable), then local `.zirv/` scripts and shortcuts, then global `~/.zirv` scripts and shortcuts. The builtins block names `chat`/`agent` alongside the older built-ins and states the bare-invocation rule in one line, so both are discoverable from `zirv help` and not just from this page. Script listing skips every name in `utils::RESERVED_ZIRV_FILES` (`.shortcuts.yaml`, `ctx.toml`, `.settings.toml`), compared case-insensitively (`utils::is_reserved_zirv_file`, since NTFS/APFS resolve file names case-insensitively too) — parsing one of these as a `Script` used to fail the whole listing. Any script file or shortcut whose name collides with a reserved built-in (`utils::RESERVED_COMMANDS`: `help`/`h`, `version`/`v`, `init`/`i`, `create`/`c`, `ctx`, `memory`, `chat`, `agent`) is annotated `(shadowed by a built-in command, unreachable)`, since `main.rs`'s built-in match runs before `.zirv/` is ever consulted.

### `version` (`commands/version.rs`)

One line: `Version: {CARGO_PKG_VERSION}`.

### `zirv memory` (`commands/ctx/memory_cli.rs`)

A management surface for the repo-scoped memory bank (see [[Ctx Subsystem]]'s "Sessions and Memory" section for the store itself) that works without starting an AI session at all (issue #33): `status`, `list`, `recall <query>`, `remember <key> <text>`, `forget <key>`, `verify <key>`. Every verb defaults to the private (machine-local) scope; `--shared` switches to the repository-owned bank under `<repo>/.zirv/memory/`. `status`/`list`/`recall` are reads and respect each scope's own gate (`memory.enabled`/`memory.shared_enabled` — a disabled scope reports as disabled or lists empty rather than showing what it holds); `forget`/`verify` are maintenance verbs and stay ungated, so disabling a scope can never trap data behind it. `status` never prints a key or a body, only counts, stored bytes, scope availability, and the two injection budgets issue #34/#35 introduced: `memory.core_max_bytes` (the merged private+shared core layer every session gets) and `memory.retrieval_max_bytes`/`retrieval_max_entries` (the context-ranked retrieval layer, on top of core; `memory.max_injected_bytes` is superseded by `core_max_bytes` and no longer read by any injection call site). `recall <query>` (issue #35) ranks this scope's entries by relevance through `retrieval::rank`/`select` — key/keyword/tag matches, importance/confidence, verification staleness — budgeted by `retrieval_max_bytes`/`retrieval_max_entries`; an exact key match ranks first but no longer suppresses every other hit, and both human-readable and JSON output carry a `reasons` trail explaining why each entry was selected. An empty or weak query (nothing clears the relevance floor) returns nothing rather than the whole bank. `remember`'s private arm is a thin wrapper over the exact function `zirv ctx remember` itself calls, so the two surfaces cannot drift for that scope; `zirv ctx remember`/`recall`/`forget` are unchanged by this and keep working as before, independent of this family. Human-readable output for a shared-scope entry always carries an explicit "not operator-verified" note, and JSON output carries a `scope` field derived from which directory was actually read — never from the entry's own header — since a shared entry's `Source`/`Written-By` fields are attacker-supplied repository content (see [[Ctx Subsystem]]).

### `zirv chat`/bare `zirv` default to the dashboard on a capable terminal

`zirv chat` (and bare `zirv`, once step 4 above routes to it) no longer always lands on a plain `wrap` passthrough session: on a real terminal at least 80×20 with VT processing available and `cfg.dash.enabled` (the default), it opens the dashboard session multiplexer instead — see [[Ctx Supervisors]]'s "The dashboard (`dash`)" section for the pane model, prefix keys, and quit/restore roster. `--simple` (or a terminal too small, or `[dash] enabled = false`) falls back to today's plain `wrap` chrome unchanged; a too-small terminal that would otherwise qualify prints a one-line notice naming the floor before falling back. `[chat] model`/`ZIRV_CTX_CHAT_MODEL`, when set, selects the model for the orchestrator session either way (dashboard or plain `wrap`) and is shown in the launch banner/dashboard header — see [[Ctx Adapters]] and [[Untrusted Configuration]] for why this one model key is deliberately not repo-forbidden, unlike every other model key in `ctx.toml`.
