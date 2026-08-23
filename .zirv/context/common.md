# zirv -- working instructions (all harnesses)

zirv is one Rust binary with two halves: a script runner that executes
developer-defined YAML/JSON/TOML scripts in `.zirv/` (substituting `${var}`
params and secrets), and `zirv ctx`, which supervises Claude Code / Codex
sessions, scores transcripts with a deterministic rot engine, and acts
(advise / compact / restart-with-handoff) before context rot ruins it.

## Build and verify (all four, before claiming done)

    cargo build
    cargo test --verbose -- --test-threads=1   # serial required
    cargo fmt -- --check
    cargo clippy --all-targets -- -D warnings

## Module map

- `src/main.rs`, `src/input.rs` -- raw-argv built-in interception, then clap, then script lookup; `src/commands/` -- create, init, help, version, setup.
- `src/commands/workflow/` -- skill/capability, engine/classify, verification/review, artifact/telemetry.
- `src/script_runner/` -- script.rs (model + run loop), command.rs (`${var}`), command_types.rs, options.rs, mod.rs.
- `src/utils.rs` (parsing, shortcuts, reserved names); `src/settings.rs` (per-agent `.settings.toml` gate).
- `src/commands/ctx/` -- mod/config/state/log; event+rot; adapters/{claude,codex}; score handoff resume hook status handover; run_loop exec wrap (supervisors); signal supervise term; pace usage window poll; optimize prompt compile context memory memory_cli mail sessions policy safety chat agent announce chrome; dash/ (mod pane ui spawnreq roster).

## Conventions

- Rust edition 2024. Command options use `#[serde(default)]` or `Option<T>`. Optional script params take a `?` suffix (`"branch?"`). Scripts live in `.zirv/` (local) or `~/.zirv/` (global).
- Reserved built-ins a script can never shadow (case-insensitive): ctx, chat, agent, skill, workflow, test, verify, artifact, memory, context, setup, help, version, init, create, frontend. `.zirv/ctx.toml`, `.zirv/.settings.toml`, `.zirv/verify.toml`, `.zirv/.shortcuts.yaml` are config, not scripts.
- `rot.rs` is pure: no fs, clock, env, or net inside it, so identical events always produce an identical verdict. All I/O lives one layer up in `score.rs`.
- `wrap` must never make a session worse: no `unwrap`/`expect` on its hot path, raw-mode restore in explicit arms (release profile is `panic = "abort"`), and any supervision failure degrades to pure passthrough.
- Repo-owned surfaces are UNTRUSTED and may only NARROW, never widen: `<repo>/.zirv/ctx.toml`, `system-prompt.md`, `context/*.md`, `memory/`, repo skills and checks. `REPO_FORBIDDEN` keys in `config.rs` hard-error from a repo layer; only `~/.zirv/ctx.toml`, `ZIRV_CTX_*`, or a flag may set them.
- Tests stay inline in `#[cfg(test)] mod tests`; `tests/fixtures/` is data only.

## After completing work -- mandatory

Read `docs/obsidian/_system-context.md` first each session, then Active Work,
the last 2-3 Work Journal entries, Known Issues, and the Decision Log. Update
the vault when behavior, contract, or architecture changes (not for pure
refactors, bug fixes, new tests, or CI-only diffs), and bump the page's
`last-verified`:

- CLI arg / built-in -> `Modules/Built-in Commands.md`
- script format, option, param, shortcut resolution -> `Concepts/Script Files.md`, `Modules/Script Runner.md`, `Concepts/Shortcuts.md`
- ctx verb / adapter / safety policy -> `Modules/Ctx Subsystem.md`, `Modules/Ctx Adapters.md`, `Modules/Command Safety.md`, `Concepts/Untrusted Configuration.md`
- rot / supervisor / pacing -> `Modules/Rot Engine.md`, `Modules/Ctx Supervisors.md`, `Modules/Usage and Pacing.md`
- dependency or release profile -> `Architecture/Technology Stack.md`
- decision / session work / gotcha -> `Development/{Decision Log,Work Journal,Known Issues}.md`

Move finished work into Active Work's "Recently Completed" with next-session
context; extend an existing page instead of adding a parallel copy.

## Git

Never commit or push to `main`/`master` -- branch first and open a PR. Every PR
must raise `Cargo.toml`'s version above its base or CD fails on a duplicate
release. No "Co-Authored-By" or "Generated with Claude Code" lines.
