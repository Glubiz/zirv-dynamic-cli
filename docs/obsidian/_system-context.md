---
last-verified: 2026-08-20
---

# _system-context

> This file is Claude's entry point. For human-readable navigation, see [[Home]].

## System Purpose

`zirv` is a single Rust binary that executes developer-defined scripts (YAML/JSON/TOML) living in `.zirv/` directories, substituting `${var}` parameters and secrets and running shell/agent/concurrent steps in order. Its second half, `zirv ctx`, is an AI-agent-context-management subsystem: it supervises long-running Claude Code or Codex sessions, scoring their transcripts with a deterministic rot engine and acting — advise, compact, or restart-with-handoff — before context-window degradation ruins the session. Codex is launch-supported but honestly degraded (no rot score, no turn signal) until its event shapes are wired up — see [[Ctx Adapters]]. The two halves share one binary and one dispatch table but are otherwise decoupled: script steps can *invoke* a supervised agent run, but the ctx CLI surface is intercepted before script resolution ever runs. See [[Architecture Overview]] and [[Context Management]].

## Architecture at a Glance

```
argv[1] == "ctx"?  --yes-->  commands::ctx::dispatch (own clap parser, exits process)
      |no
argv[1] in --help/-h? --yes--> show_help (raw-argv intercept, bypasses clap)
      |no
Input::parse() (clap)  -->  help|version|init|create  -->  handled, return
      |no match
Input::get_file_path()  -->  literal path | .zirv/<name>.ext | .zirv/.shortcuts.yaml
                              | ~/.zirv/<name>.ext | ~/.zirv/.shortcuts.yaml
      |
file_to_script (parse YAML/JSON/TOML)  -->  script_runner::execute
      |
build_context (params + secrets)  -->  Script::run loop over CommandTypes steps
```

Dispatch order matters: `ctx` and top-level `--help` are matched on **raw argv**, before clap ever runs, which is why `.zirv/ctx.yaml` is permanently unreachable (see Gotchas). The same raw-argv-first pattern repeats one layer down inside `ctx` itself: `commands::ctx::dispatch` reparses argv under its own `CtxCli` clap parser and never returns control to `main.rs` — it exits the process directly. Full detail: [[Architecture Overview]], [[Script Resolution]].

## Module Quick Map

| Area | Source paths | Vault page | Purpose |
|---|---|---|---|
| Script runner core | `src/script_runner/{script,command,command_types,options,agent_command,fallback_command,operating_system,secret}.rs`, `mod.rs` | [[Script Runner]] | Parses/executes a `Script`: shell steps, concurrent terminal blocks, supervised agent steps |
| Built-ins & entry point | `src/main.rs`, `src/input.rs`, `src/commands/{mod,create,init,help,version}.rs` | [[Built-in Commands]] | Dispatch table; `help`/`version`/`init`/`create` and their aliases |
| Development workflows | `src/commands/workflow/` | [[Workflows]] | Skills/capabilities, durable lifecycle state, risk classification, verification, review, artifacts, telemetry |
| Utilities | `src/utils.rs` | [[Utilities]] | File parsing, reserved names, shortcuts struct, home dir, Levenshtein "did you mean" |
| Ctx hub / verb tree | `src/commands/ctx/{mod,config,state,log}.rs` | [[Ctx Subsystem]] | `CtxCli`/`CtxVerb`, dispatch + parse-failure classification, layered `CtxConfig`, `StateDir`, decision log |
| Ctx verb modules | `src/commands/ctx/{score,handoff,resume,hook,status,chat,agent,mail,sessions,memory}.rs` | [[Ctx Subsystem]] | One module per verb: read-transcript/decide/maybe-write-state, the meta-harness verbs (chat/agent/mail), the session registry + nudge, and the memory bank |
| Ctx supervisors | `src/commands/ctx/{run_loop,exec,wrap}.rs` + `{signal,supervise,term}.rs` + `dash/{mod,pane,ui,spawnreq,roster}.rs` | [[Ctx Supervisors]] | The three process supervisors, turn-signal sockets, shared process/terminal primitives, and the `dash` session multiplexer `zirv chat` opens on a capable terminal |
| Ctx adapters | `src/commands/ctx/adapters/{mod,claude,codex}.rs` | [[Ctx Adapters]] | `AgentAdapter` trait; claude (full capabilities) and codex (launch-supported, event parsing/rot score/turn signal pending issue #11) |
| Rot engine | `src/commands/ctx/{event,rot}.rs` | [[Rot Engine]] | Pure normalized-event scoring → `Verdict` |
| Usage & pacing | `src/commands/ctx/{window,usage,pace}.rs` | [[Usage and Pacing]] | Rolling rate-limit windows, the pacing gate, `usage` verb + statusline tee |
| Config analysis & prompt | `src/commands/ctx/{optimize,prompt}.rs` | [[Utilities]] | Report-only config analysis; the layered injected session prompt |
| Concepts (cross-cutting) | n/a — describe conventions, not modules | [[Script Files]], [[Shortcuts]], [[Context Management]], [[Untrusted Configuration]] | The file format contract, the shortcut-lookup convention, the ctx philosophy, and the repo-trust boundary that spans several modules |

Every row's "vault page" is also the page whose "If changed" line names its own downstream dependents — follow that chain rather than guessing which sibling pages need re-verifying after an edit.

## Ctx Verb Reference

| Verb | One-liner |
|---|---|
| `score` | Rot-score a transcript once, print JSON. No persisted state. |
| `handoff` | Distill a transcript into a stored handoff document. |
| `resume` | Start a clean interactive session with the latest handoff injected. |
| `hook` | Agent hook entrypoints (`Stop`, `Prompt`, `PreCompact`, `Notify`); must never exit non-zero on `Stop`. |
| `status` | Show the session registry, the memory bank, unread mail, scores, and the decision-log tail. |
| `send` / `inbox` | Leave or read repo-scoped notes between sessions, optionally addressed to one live session (`--to-session`). |
| `nudge` | Wake a live session early with a message, resolved against the session registry. |
| `remember` / `recall` / `forget` | Read and write the repo-scoped memory bank of durable repository facts. |
| `loop` | Stateless loop runner — a fresh headless session and `SessionId` every cycle. |
| `exec` | Supervise one headless run; restart in place (bounded budget) on rot or a usage limit. |
| `wrap` | Supervise an interactive TUI through a PTY; advise/compact/restart at verified turn boundaries. |
| `usage` | Report usage windows, or tee the statusline to record them. |
| `optimize` | Report-only analysis of the config surfaces that steer every session. |

## Key Flows

1. **Script execution** — `main.rs` resolves a script name to a file ([[Script Resolution]]), parses it into a `Script` ([[Script Files]]), then `script_runner::execute` builds a param/secret context and `Script::run` walks each `CommandTypes` step (`Command`/`Commands`/`Agent`) in order, substituting `${var}` and stopping at the first hard error. A `Commands` (list-of-lists) step opens a new terminal window per OS instead of running inline. See [[Script Runner]].
2. **Ctx verb dispatch** — `argv[1] == "ctx"` routes to `commands::ctx::dispatch`, which reparses argv as `CtxCli`, special-cases `--help`/`--version`, and on a genuine parse failure classifies it (`Hook` → exit 0, `usage tee` → fallback statusline line, else exit 2) rather than crashing a downstream reader — because a Stop hook exit 2 means "block the stop" to Claude Code, and a broken statusline command renders as a broken terminal. See [[Ctx Subsystem]].
3. **`wrap` supervision** — opens a PTY around an interactive agent, pumps bytes both ways, and at verified turn boundaries (debounced, only while idle) may advise/compact/restart based on the rot engine's `Verdict`. Any supervision failure sets a one-way `degraded` flag that permanently falls back to pure passthrough for the rest of that session. See [[Ctx Supervisors]].
4. **Handoff / resume** — a restart (from `exec`, `wrap`, or a rotted `loop` cycle) never asks the rotting session to summarize itself: `handoff::distill_or_structural` runs a fresh model call (or a mechanical fallback that never fails) over the transcript tail, stores a handoff doc under the state dir, and `resume`/the relaunch injects it as the new opening prompt. See [[Ctx Subsystem]], [[Context Management]].
5. **Usage pacing** — before/around every supervised cycle, `pace::wait_for_window` reads a server-authoritative collector (fed by the statusline tee) and/or a token-count estimator, and either proceeds or sleeps in ≤30s chunks (jittered, capped by the tripped window's own length) until a rate-limit window resets, so autonomous work doesn't die mid-run on a 5-hour or 7-day limit. See [[Usage and Pacing]].

## Cross-Reference Index

| Working on... | Read first |
|---|---|
| A script's YAML/JSON/TOML shape, params, secrets, options | [[Script Files]], [[Script Runner]] |
| How a script name resolves to a file | [[Script Resolution]], [[Shortcuts]] |
| `main.rs` dispatch, built-in commands (`help`/`version`/`init`/`create`) | [[Built-in Commands]] |
| Adding/changing a `zirv ctx` verb | [[Ctx Subsystem]] |
| Skills, workflow phases, risk, verification, review, artifacts, telemetry | [[Workflows]], [[Untrusted Configuration]] |
| `loop`/`exec`/`wrap` process supervision, raw mode, signals | [[Ctx Supervisors]] |
| Claude/Codex launch commands, transcript parsing, the distiller restriction | [[Ctx Adapters]] |
| Rot scoring thresholds, verdict logic, event model | [[Rot Engine]] |
| Rate-limit windows, pacing gate, statusline tee | [[Usage and Pacing]] |
| `zirv ctx optimize`, the injected session prompt, `utils.rs` helpers | [[Utilities]] |
| Repo config/prompt trust boundary, `REPO_FORBIDDEN` | [[Untrusted Configuration]] |
| Why `zirv ctx` exists, the rot/supervise/handoff philosophy | [[Context Management]] |
| Crate dependencies, release profile, `panic = "abort"` | [[Technology Stack]] |
| Running/writing tests, fixtures | [[Testing Guide]] |
| Recent work, in-progress branches | [[Active Work]], [[Work Journal]] |
| Past architectural decisions | [[Decision Log]] |
| Known gotchas before debugging | [[Known Issues]] |
| Why the crate depends on `portable-pty`/`tokio`/`hashbrown`/etc. | [[Technology Stack]] |
| Adding a new step type or per-command option to the script format | [[Script Files]], [[Script Runner]] |
| Adding/removing a config key in `ctx.toml`, deciding if it must be repo-forbidden | [[Ctx Subsystem]], [[Untrusted Configuration]] |
| Debugging a flaky or state-corrupting test | [[Testing Guide]], [[Known Issues]] |
| Writing or updating a vault page | this file's Documentation Contract section, `CLAUDE.md` § "Obsidian Documentation Updates" |

## Anti-Patterns and Gotchas

- **Rot engine purity**: `rot.rs` must contain no `std::fs`, `std::time`, `std::env`, or `std::net` calls — every scoring function is data-in/data-out so identical events always produce an identical verdict. All I/O lives one layer up in `score.rs`. See [[Rot Engine]].
- **`wrap` never degrades a session**: the release profile sets `panic = "abort"`, so `Drop`-based cleanup cannot be relied on — raw-mode terminal restore happens in explicit arms, `wrap.rs`'s hot path (`run_with`, `pump`) has zero `unwrap`/`expect`, and any supervision failure flips a one-way `degraded` flag that falls back to pure passthrough rather than crashing or corrupting the terminal. See [[Ctx Supervisors]], [[Technology Stack]].
- **`.zirv/ctx.yaml` is shadowed**: `zirv ctx` is intercepted on raw argv in `main.rs` before any script lookup, so a script literally named `ctx` can never run. `.zirv/ctx.toml` is a different file (the config), excluded from script listing. See [[Ctx Subsystem]], [[Known Issues]].
- **Tests require `--test-threads=1`**: `cargo test --verbose -- --test-threads=1` — tests share state (state dir, fixtures) and flake or corrupt each other under the default parallel runner. See [[Testing Guide]].
- **Repo-provided prompt/config text is untrusted**: `<repo>/.zirv/ctx.toml`'s repo layer and `<repo>/.zirv/system-prompt.md` are adversarial input, capped, labeled non-authoritative, and structurally unable to enable or uncap themselves (`REPO_FORBIDDEN` keys are hard config-load errors from a repo file). See [[Untrusted Configuration]].
- **`zirv ctx optimize` is report-only**: writes only to stdout, a timestamped state-dir copy, and an explicit `--out` path — never an analyzed file. Verified by a test that snapshots the analyzed tree before/after and asserts byte-identical. Its judgment/distiller model child also has `Write`/`Edit`/`Bash`/`NotebookEdit` structurally denied (`ClaudeAdapter::distiller_cmd`, one `=`-bound argv token), because its prompt embeds untrusted repo CLAUDE.md text — the guarantee is structural, not just "the model chose not to." See [[Utilities]], [[Untrusted Configuration]], [[Ctx Adapters]].
- **Fixtures are data, not tests**: `tests/fixtures/` holds recorded sessions and fake shell scripts only; all unit tests stay inline in `#[cfg(test)] mod tests` next to the code. Re-record the claude fixture with `scripts/record-claude-fixture.py`; never hand-edit it. See [[Testing Guide]].
- **Untagged-enum footgun avoided on purpose**: `CommandTypes` is deserialized by hand (dispatching on which key a step's mapping has) rather than serde's `untagged`, because untagged silently picks the first variant that fits and reports only "data did not match any variant." See [[Script Runner]].
- **`REPO_FORBIDDEN` is a real security boundary, not style**: a repo-committed `.zirv/ctx.toml` cannot set `agent_bin`, `supervise.on_failure`, `handoff.model`, `optimize.model`, or any `prompt.*` cap/enable key — `CtxConfig::load` hard-errors naming the key and its operator-only escape hatch (an env var or `~/.zirv/ctx.toml`). See [[Ctx Subsystem]], [[Untrusted Configuration]].
- **codex is launch-supported, not feature-parity**: `CodexAdapter::ready()` mirrors claude's (only an unresolvable binary fails it), so `--agent codex` selects and launches, but `capabilities()` stays all-false — no event parsing, rot score, usage source, turn signal, or injected system prompt. Don't assume adapter parity between claude and codex when reading `adapters/codex.rs`; full event support is issue #11. See [[Ctx Adapters]], [[Known Issues]].

## Build & Verify

| Task | Command |
|---|---|
| Build | `cargo build` |
| Test (serial — required, see Gotchas) | `cargo test --verbose -- --test-threads=1` |
| Format check | `cargo fmt -- --check` |
| Lint | `cargo clippy --all-targets -- -D warnings` |

Run all four before claiming a change is done. Full detail: [[Getting Started]], [[Testing Guide]].

## Key Config Files

- `.zirv/*.yaml|.json|.toml` — user-defined scripts (this repo's own dev scripts live under `.zirv/`, e.g. `commit.yaml`, `test.yaml`).
- `.zirv/.shortcuts.yaml` — short-key → script-filename map, local or global (`~/.zirv/`). See [[Shortcuts]].
- `.zirv/ctx.toml` — the layered `zirv ctx` config's repo layer; subject to `REPO_FORBIDDEN` key rejection. See [[Ctx Subsystem]], [[Untrusted Configuration]].
- `.claude/settings.json` — wires the doc-coverage `PreToolUse` hook (`bash scripts/check-doc-coverage.sh` on `Bash` tool calls).
- `scripts/check-doc-coverage.sh`, `scripts/check-doc-staleness.sh` — enforce that vault pages stay updated and flag stale `last-verified` dates.
- `.claude/agents/vault-keeper.md` — the agent that enforces the doc-update contract before push.
- `Cargo.toml` `[profile.release]` — `opt-level = "z"`, `lto = true`, `codegen-units = 1`, `panic = "abort"`; the last of these is load-bearing for [[Ctx Supervisors]]'s wrap-never-degrades contract. See [[Technology Stack]].
- `docs/superpowers/` — specs, plans, and dated verification notes (e.g. the system-prompt-injection facts cited by [[Ctx Adapters]] and [[Untrusted Configuration]]); lives outside this vault, plain filesystem path only, not wikilinked.

## State Directory Layout

`StateDir::resolve` roots at `ZIRV_CTX_STATE_DIR`, else the OS state dir, else the OS local-data dir, then `zirv/ctx`. Subpaths: `handoffs/<repo_slug>/` (stored handoff docs), `s/` (turn-signal sockets, short name for the unix path-length limit), `logs/decisions.jsonl` (append-only decision log), `usage.json` (single machine-wide file, merged across sessions), `scoring/` (per-transcript incremental-scoring checkpoints). Unix directories are `0700` and files `0600`; Windows has no equivalent and is a no-op there. See [[Ctx Subsystem]].

## Documentation Contract

Every vault page carries `last-verified` frontmatter and, where relevant, an "If changed" line naming which sibling pages to update alongside it. The authoritative doc-update trigger table (change type → page to update) lives in this repo's `CLAUDE.md` under "Obsidian Documentation Updates" — read it before deciding whether a change needs a doc update. The `vault-keeper` agent (`.claude/agents/vault-keeper.md`) enforces that contract before push, backed by the `check-doc-coverage.sh`/`check-doc-staleness.sh` scripts.
