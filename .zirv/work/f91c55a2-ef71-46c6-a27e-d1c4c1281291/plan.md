# Implementation plan

## Ordered tasks

- [ ] T1: Track A -- #280 handoff v3 (iterative distillation, new sections, read-vs-modified, COMPACT_FOCUS)
  - Files: `src/commands/ctx/handoff.rs`, `event.rs`, `adapters/{claude,codex}.rs`, `resume.rs`, `wrap.rs` (COMPACT_FOCUS + restart path), `exec.rs`, `handover.rs`, `dash/mod.rs`
  - Verify: `cargo nextest run handoff:: adapters:: resume:: --no-fail-fast`, clippy, fmt (worktree)
- [ ] T2: Track B -- #281 working-set manifest + crash-interruption witness
  - Files: `src/commands/ctx/handoff.rs` (working_set/render_working_set), `resume.rs`, `hook.rs`, `sessions.rs` (Record.in_flight), `wrap.rs`, `exec.rs`
  - Verify: `cargo nextest run handoff:: resume:: hook:: sessions:: --no-fail-fast`, clippy, fmt (worktree)
- [ ] T3: Track C -- #287 no-progress guard + #301 per-child reservation
  - Files: `src/commands/workflow/{verification,engine,telemetry}.rs`; `src/commands/ctx/{group,agent}.rs`, `dash/mod.rs`
  - Verify: `cargo nextest run verification:: engine:: group:: agent:: --no-fail-fast`, clippy, fmt (worktree)
- [ ] T4: Track D -- #285 durable objective layer
  - Files: new `src/commands/ctx/objective.rs`, `mod.rs`, `state.rs`, `prompt.rs`, `compile.rs`, `exec.rs`, `run_loop.rs`, `config.rs`, `handoff.rs`
  - Verify: `cargo nextest run objective:: prompt:: compile:: config:: exec:: run_loop:: --no-fail-fast`, clippy, fmt (worktree)
- [ ] T5: Merge A, B, C, D into `release/3.11.0-harness-batch`; resolve conflicts; full five gates; failure-name diff vs `main`
  - Files: release branch
  - Verify: `cargo build`; `cargo nextest run --no-fail-fast`; `cargo test --verbose -- --test-threads=1`; `cargo fmt -- --check`; `cargo clippy --all-targets -- -D warnings`
- [ ] T6: Linux Docker `wrap::` suite + clippy on `rust:1-bookworm` (non-root, autocrlf=false archive)
  - Files: none
  - Verify: `cargo test --bin zirv wrap:: -- --test-threads=1`; `cargo clippy --all-targets -- -D warnings`
- [ ] T7: Vault docs (vault-keeper), review round (`zirv workflow review run`), fixes, PR closing #280 #281 #287 #301 #285
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
