# Specification

## Context

Five verified defects in live code paths of supervised sessions (see intent.md). Each issue body already carries a file-level design with `file:line` evidence; this spec fixes the cross-issue decisions and the integration order. Issue bodies: #280, #281, #287, #301, #285 on GitHub.

Evidence checked on `main` at 2f1cd0c:

- `handoff.rs:508 distill(...)` has no previous-handoff parameter; `Handoff.files_touched` (`:34`) is one flat list; `DISTILL_PROMPT_VERSION == "v2"` (`:276`).
- `resume.rs:67 resume_prompt` wraps `labeled_for_injection` only; `sessions::Record` (`sessions.rs:302`) has no `in_flight`; `record_is_alive` exists (`:956`/`:967`).
- `verification.rs:493 change_fingerprint`, `GateOutcome` (`:658`), `latest_is_fresh_and_passing` (`:1968`); no `Unchanged` outcome and no failure-fingerprint comparison.
- `agent.rs:617 resolve_worker_budget` uses `budget - group.spent_tokens`; `WorkGroup` (`group.rs:31`) has `spent_tokens` only; `add_spent_tokens` (`:244`) is the only mutation.
- `exec.rs` `--budget-tokens` exits with `EXIT_BUDGET_EXHAUSTED`; `run_loop.rs` recomposes the prompt per cycle; no objective type exists.

## Goals

- #280: iterative distillation (previous handoff fed into `distill_prompt`), sections Task / Constraints / Done / Remaining / Blocked / Key decisions / Verification / Next step / Files read / Files modified / Gotchas learned, tool-name read-vs-modified classification in both adapters, `COMPACT_FOCUS` extended, v2 markdown back-compat.
- #281: `handoff::working_set` (I/O) + pure `render_working_set`, appended after the distilled handoff inside the same untrusted envelope with caps (40 total / 20 per section, `+N more`, 120-char paths) and the always-present "what did not survive" line; `Record.in_flight` stamped/cleared by `wrap` and `exec`, consumed once into a constant `<zirv_interrupted>` block when the pid is dead.
- #287: `GateOutcome`/verification gains an `Unchanged { fingerprint, since_attempt }` path: a verify whose fresh `change_fingerprint` equals the last failing report's skips execution, still increments `state.attempts`, renders the fixed message, and is emitted distinctly in telemetry; `change_fingerprint` records symlink targets.
- #301: `WorkGroup.reserved_tokens` (`#[serde(default)]`); admission reserves the child's ceiling inside the existing group mutation lock; completion replaces the reservation with actual spend; failed spawn / rollback releases; ceilings derive from `budget - spent - reserved`.
- #285: `<state>/objective/<short>.json` record, `PromptSource::Objective` layer folded in after `Context`, re-injected each `loop` cycle and reloaded across `exec` restarts, status flip to `budget_limited`/`deadline_limited` swaps in the wrap-up text, `zirv ctx objective set|show|close`, `--objective` on `exec`/`loop`, `[pace] run_budget_tokens` operator-only and `REPO_FORBIDDEN`.

## Non-goals

- #303 codex headless compaction (verified impossible with `codex exec`).
- Split-turn summarisation, session trees, new handoff file naming (#280 non-goals).
- Replaying interrupted operations or a second liveness mechanism (#281).
- Cross-step fingerprint sharing or timeout changes (#287).
- Dollar budgets, model downgrade, kill paths (#285/#301).

## Design

Cross-issue decisions:

1. **Injection order (#280 + #281 + #285).** Resume/restart prompt = distilled handoff (v3) -> working-set manifest, both inside one `labeled_for_injection` envelope; the objective layer is a separate late prompt layer emitted by `compile::compile` after `Context`, never inside the handoff envelope. Only #281 changes `resume_prompt`'s body; #280 changes what the `Handoff` contains; #285 does not touch `resume.rs`.
2. **`Record` change (#281) is additive**: `in_flight: Option<InFlight>` with `#[serde(default)]`; stamp at turn start and clear at the turn boundary in `wrap.rs` (turn-signal arm) and `exec.rs` (tick loop). The witness is consumed by clearing the marker after emission.
3. **Outcome shape (#287)**: extend the existing #268 three-valued gate outcome rather than adding a sibling enum; `engine.rs` accounts `Unchanged` exactly as `Failure` for attempts and termination.
4. **Reservation accounting (#301)**: `reserve_tokens(state, id, n)` and `settle_reservation(state, id, reserved, actual)` under the same lock as `add_spent_tokens`; `resolve_worker_budget` (agent) and the dash spawn path both reserve; settlement happens where `add_spent_tokens` is called today (`agent.rs:1950`, `dash/mod.rs:1283`).
5. **Objective purity (#285)**: status transition and layer text are pure functions with `now` and `spent` as parameters; I/O lives in `objective.rs` load/store; spend figure is the same `token_spend` used by `agent::budget_state`.

Tracks: A (#280), B (#281), C (#287 + #301), D (#285), each in its own git worktree from the release branch, merged by the orchestrator in the order A, B, C, D with conflict review on `handoff.rs`, `exec.rs`, `run_loop.rs`, `wrap.rs`.

## Testing strategy

- Per track: `cargo build`, `cargo nextest run <touched modules>`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt -- --check`, run in the foreground.
- After merge: the full five gates; sorted failure-name list diffed against `main`'s on this box.
- Linux Docker (`rust:1-bookworm`, non-root, `git -c core.autocrlf=false archive`) `cargo test --bin zirv wrap:: -- --test-threads=1` plus clippy, because #280 (`COMPACT_FOCUS`) and #281 (turn-start stamp) touch `#[cfg(unix)]` paths; the #287 symlink test also runs there.
- Review: `zirv workflow review run` (claude sonnet + codex), fix confirmed findings, re-review touched areas, max 2 fix rounds.

## Risks

- Merge conflicts across tracks in `handoff.rs`/`exec.rs`: mitigated by disjoint function ownership per track and orchestrator-side conflict review.
- Windows box cannot run the `cfg(unix)` tests: mitigated by the Docker run before push.
- `wrap.rs` hot-path change for #281: the stamp is best-effort (`let _ =`), never unwraps, and a failed stamp leaves supervision untouched.
- Machine instability under all-core builds: workers cap `CARGO_BUILD_JOBS` and test threads.
