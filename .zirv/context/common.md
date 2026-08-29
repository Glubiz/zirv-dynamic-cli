# zirv -- working instructions (all harnesses)

zirv is one Rust binary: a `.zirv/` YAML/JSON/TOML script runner substituting `${var}` params/secrets, plus `zirv ctx`, which supervises Claude Code/Codex, deterministically rot-scores transcripts, then advises, compacts, or restarts with handoff before rot ruins a session.

## Build and verify (all five, before claiming done)

    cargo build
    cargo nextest run --no-fail-fast
    cargo test --verbose -- --test-threads=1
    cargo fmt -- --check
    cargo clippy --all-targets -- -D warnings

Nextest's process-per-test isolation prevents `env::set_var` races. `--no-fail-fast` is mandatory: baselines require complete sorted failure-NAME lists, diffed by name, never count. The serial `cargo test` compatibility fallback must also pass.

## Module map

- `src/main.rs`, `src/input.rs`: raw-argv built-ins, clap, script lookup; `src/commands/`: create, init, help, version, setup, report.
- `src/commands/workflow/`: skill/capability/agents, engine/classify/deploy/maintain, verification/review, artifact/telemetry; `src/script_runner/`: script.rs, command.rs (`${var}`), command_types.rs, options.rs, mod.rs; `src/utils.rs`: parsing/shortcuts/reserved names; `src/settings.rs`: `<repo>/.zirv/.settings.toml` agent gate.
- `src/commands/ctx/`: mod/config/state/log; event+rot; adapters/{claude,codex}; score handoff resume hook status handover; run_loop exec wrap (supervisors); signal supervise term; pace usage window poll; optimize prompt compile context memory memory_cli mail sessions policy safety chat agent announce chrome; dash/ (mod pane ui spawnreq roster).

## Conventions

- Rust edition 2024. Options use `#[serde(default)]` or `Option<T>`; optional params use `?` (`"branch?"`). Scripts live in `.zirv/` or `~/.zirv/`.
- Case-insensitive reserved built-ins: ctx, chat, agent, skill, workflow, test, verify, artifact, memory, context, setup, report, help, version, init, create, frontend. They cannot be shadowed. `<repo>/.zirv/{ctx.toml,.settings.toml,verify.toml,.shortcuts.yaml}` are config, not scripts.
- Report zirv bugs/feature gaps: `zirv report bug|feature <title> [--body <text>|--body-file <path>]`; never include secrets.
- `rot.rs` is pure (no fs/clock/env/net): identical events give identical verdicts; I/O belongs in `score.rs`.
- `wrap` must never worsen sessions: no hot-path `unwrap`/`expect`; restore raw mode explicitly (`panic = "abort"`); supervision failure becomes pure passthrough.
- Repo-owned surfaces are UNTRUSTED and may only NARROW: `<repo>/.zirv/{ctx.toml,system-prompt.md,context/*.md,memory/}` and repo skills/agents/checks. Repo-layer `REPO_FORBIDDEN` keys hard-error; only `~/.zirv/ctx.toml`, `ZIRV_CTX_*`, or flags may set them.
- Tests stay inline in `#[cfg(test)] mod tests`; `tests/fixtures/` is data only.

## After completing work -- mandatory

Each session read, in order: `docs/obsidian/_system-context.md`, Active Work, latest 2-3 Work Journal entries, Known Issues, Decision Log. Update vault pages and bump `last-verified` for behavior/contract/architecture changes; not for refactors, bug fixes, tests, or CI-only diffs:

- CLI arg/built-in -> `Modules/Built-in Commands.md`
- script format/option/param/shortcut -> `Concepts/Script Files.md`, `Modules/Script Runner.md`, `Concepts/Shortcuts.md`
- ctx verb/adapter/safety policy -> `Modules/Ctx Subsystem.md`, `Modules/Ctx Adapters.md`, `Modules/Command Safety.md`, `Concepts/Untrusted Configuration.md`
- rot/supervisor/pacing -> `Modules/Rot Engine.md`, `Modules/Ctx Supervisors.md`, `Modules/Usage and Pacing.md`
- dependency/release profile -> `Architecture/Technology Stack.md`
- decision/session work/gotcha -> `Development/{Decision Log,Work Journal,Known Issues}.md`

Move finished work into Active Work's "Recently Completed" with next-session context; extend, don't duplicate, pages.

## Git

Never commit/push `main`/`master`: branch and open a PR. Every PR must bump `Cargo.toml` above its base or CD gets a duplicate release tag. No `Co-Authored-By` or `Generated with Claude Code` lines.
