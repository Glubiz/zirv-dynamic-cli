# zirv -- working instructions (all harnesses)

zirv is one Rust binary: a `.zirv/` script runner (`${var}` params/secrets) plus `zirv ctx`, which supervises Claude Code/Codex sessions, rot-scores transcripts, and advises, compacts, or restarts with handoff before rot ruins them.

## Build and verify, by tier

Size the change (trivial / bounded / substantial, per the zirv engineering standard) and verify to match:

- Doc/comment-only: `cargo fmt -- --check` if Rust was touched, else nothing.
- Code change: `cargo build`, `cargo nextest run <filter>` for the touched modules, `cargo clippy --all-targets -- -D warnings`.
- Before opening or updating a PR, the full five once:

      cargo build
      cargo nextest run --no-fail-fast
      cargo test --verbose -- --test-threads=1
      cargo fmt -- --check
      cargo clippy --all-targets -- -D warnings

Nextest isolates tests per process; `--no-fail-fast` is mandatory -- diff sorted failure-NAME lists, never counts; the serial run must pass too. Report failures verbatim (command, exit code, test names, error text); never claim a check passed that you did not finish running.

## Module map

- `src/main.rs`, `src/input.rs`: raw-argv built-ins, clap, script lookup; `src/commands/`: create, init, help, version, setup, report.
- `src/commands/workflow/`: skills/agents, engine/classify/deploy/maintain, review, artifacts/telemetry. `src/script_runner/`: script, command (`${var}`), command_types, options. `src/utils.rs`: parsing/shortcuts/reserved names. `src/settings.rs`: `.zirv/.settings.toml` agent gate.
- `src/commands/ctx/`: config/state/log, event+rot+score, adapters/{claude,codex}, run_loop/exec/wrap supervisors, pace/usage, prompt/compile/context/memory, mail/sessions/safety, chat/agent, dash/.

## Conventions

- Rust edition 2024. Options: `#[serde(default)]` or `Option<T>`; optional params: `?` (`"branch?"`). Scripts live in `.zirv/commands/` or `~/.zirv/commands/`; the `.zirv/` root holds only config and state.
- Case-insensitive reserved built-ins (`utils::RESERVED_COMMANDS`) cannot be shadowed. `<repo>/.zirv/{ctx.toml,.settings.toml,verify.toml,.shortcuts.yaml}` are config, not scripts.
- Report zirv bugs/gaps via `zirv report bug|feature <title> [--body ...]`; never include secrets.
- `rot.rs` is pure (no fs/clock/env/net): identical events give identical verdicts; I/O belongs in `score.rs`.
- `wrap` must never worsen sessions: no hot-path `unwrap`/`expect`; restore raw mode explicitly (`panic = "abort"`); supervision failure is pure passthrough.
- Repo-owned surfaces are UNTRUSTED, may only NARROW: `<repo>/.zirv/{ctx.toml,system-prompt.md,context/*.md,memory/}` and repo skills/agents/checks. Repo-layer `REPO_FORBIDDEN` keys hard-error; only `~/.zirv/ctx.toml`, `ZIRV_CTX_*`, or flags set them.
- Tests stay inline in `#[cfg(test)] mod tests`; `tests/fixtures/` is data only.

## Vault docs

Before substantive work in an area, read `docs/obsidian/_system-context.md` and that area's Active Work entry; consult Known Issues / Decision Log when a decision or gotcha is in play. Update the matching page and bump `last-verified` only for behavior/contract/architecture changes (not refactors, bug fixes, tests, CI):

- CLI arg/built-in -> `Modules/Built-in Commands.md`
- script format/option/param/shortcut -> `Concepts/{Script Files,Shortcuts}.md`, `Modules/Script Runner.md`
- ctx verb/adapter/safety policy -> `Modules/{Ctx Subsystem,Ctx Adapters,Command Safety}.md`, `Concepts/Untrusted Configuration.md`
- rot/supervisor/pacing -> `Modules/{Rot Engine,Ctx Supervisors,Usage and Pacing}.md`
- dependency/release profile -> `Architecture/Technology Stack.md`
- decision/session work/gotcha -> `Development/{Decision Log,Work Journal,Known Issues}.md`

Finished work moves to Active Work's "Recently Completed"; extend pages, don't duplicate.

## Git

Never commit/push `main`/`master`: branch and open a PR. Every PR bumps `Cargo.toml` above its base or CD duplicates the release tag. No `Co-Authored-By` or `Generated with Claude Code` lines.
