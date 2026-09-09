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

## Docs and memory

`README.md` is the reference: update its section and trust-boundary table when a CLI arg, config key, or contract changes. Before substantive work in an area, run `zirv ctx recall` for that area's durable facts. A fact a future session cannot derive from code or git (verified vendor/tool behaviour, a standing decision with its rationale, an unresolved gotcha with no issue) goes to the repo bank via `zirv ctx remember --repo --key <k>`; specs and plans live in `docs/superpowers/`.

## Git

Never commit/push `main`/`master`: branch and open a PR. Bump `Cargo.toml` above base only when `src/`, `Cargo.toml`, `Cargo.lock`, or `build.rs` changed (CD is idempotent, so doc/CI-only PRs need none). No `Co-Authored-By` or `Generated with Claude Code` lines.
