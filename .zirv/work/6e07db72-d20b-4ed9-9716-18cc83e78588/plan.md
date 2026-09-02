# Implementation plan

## Ordered tasks

- [ ] T1: Track A -- #267 writer vs reader permits, one writer per tree, `--mode`, `--worktree`
  - Files: `src/commands/ctx/permit.rs`, `agent.rs`, `dash/spawnreq.rs`, `log.rs` (`Delegation.mode`), `config.rs` (`supervise.max_writers`, `REPO_FORBIDDEN`), `src/commands/workflow/engine.rs` (`auto_spawn_decision`), `adapters/{claude,codex}.rs` (one sentence), `main.rs`/`input.rs` if the `zirv agent` alias needs the flags
  - Verify: `cargo nextest run permit:: agent:: spawnreq:: engine:: config:: --no-fail-fast`, clippy, fmt (worktree, foreground)
- [ ] T2: Track C -- #293 timestamps on events, latency/TTFT derivation, speed telemetry
  - Files: `src/commands/ctx/event.rs`, `window.rs`, `adapters/{mod,claude,codex}.rs`, `score.rs`, `rot.rs` (only if a match must gain `..`), `src/commands/workflow/telemetry.rs`
  - Verify: `cargo nextest run event:: window:: adapters:: score:: rot:: telemetry:: --no-fail-fast`, clippy, fmt (worktree, foreground)
- [ ] T3: Track D -- #275 `zirv context lint`
  - Files: new `src/commands/ctx/context_lint.rs`, `context.rs`, `compile.rs` (layer accessor with provenance and budget), `src/commands/workflow/verification.rs` (`CheckKind::ContextLint`), `config.rs` (`context.lint_max_pairs`), fixtures under `tests/fixtures/context-lint/`
  - Verify: `cargo nextest run context_lint:: context:: compile:: verification:: config:: --no-fail-fast`, clippy, fmt (worktree, foreground)
- [ ] T4: Track B -- #264 cost ledger (after T1 and T2 merge)
  - Files: new `src/commands/ctx/price.rs`, `spend.rs`; `mod.rs` (verb), `status.rs`, `log.rs` (`task_class`), `agent.rs`, `dash/spawnreq.rs`, `dash/` aggregate row, `src/commands/workflow/telemetry.rs` (cost fields, `parent_session_id`, stats cost block), `config.rs` (`[price]` keys), fixture ledger
  - Verify: `cargo nextest run price:: spend:: status:: log:: agent:: telemetry:: dash:: --no-fail-fast`, clippy, fmt (worktree, foreground)
- [ ] T5: Track E -- #299 prompt-prefix stability harness (after T3 merges)
  - Files: `tests/fixtures/fake-agent.sh`, `src/commands/ctx/compile.rs`, `prompt.rs` (test modules + shared `prefix_diff` helper)
  - Verify: `cargo nextest run compile:: prompt:: --no-fail-fast`, clippy, fmt (worktree, foreground)
- [ ] T6: Merge A, C, D, then B, E into `release/3.12.0-harness-batch-2`; resolve conflicts; full five gates; failure-name diff vs baseline
  - Files: release branch
  - Verify: `cargo build`; `cargo nextest run --no-fail-fast`; `cargo test --verbose -- --test-threads=1`; `cargo fmt -- --check`; `cargo clippy --all-targets -- -D warnings`
- [ ] T7: Linux Docker full serial suite + clippy on `rust:1-bookworm` (non-root, autocrlf=false archive)
  - Files: none
  - Verify: `cargo test -- --test-threads=1`; `cargo clippy --all-targets -- -D warnings`
- [ ] T8: Vault docs (vault-keeper), review round (`zirv workflow review run` claude + codex), fixes, PR closing #267 #275 #264 #293 #299
  - Files: `docs/obsidian/**`, PR
  - Verify: review round yields no new confirmed findings; CI green

## Execution ledger

| Task | Started | Finished | Evidence |
| --- | --- | --- | --- |
| T1 |  |  |  |
| T2 |  |  |  |
| T3 |  |  |  |
| T4 |  |  |  |
| T5 |  |  |  |
| T6 |  |  |  |
| T7 |  |  |  |
| T8 |  |  |  |
