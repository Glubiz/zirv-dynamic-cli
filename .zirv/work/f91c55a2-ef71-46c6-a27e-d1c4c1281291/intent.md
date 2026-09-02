# Intent

## Problem

The open issue tracker holds 17 enhancement issues from the Ruflo (#278) and Prime Agent (#279) analyses, all aimed at the cost, speed, or intelligence of the harness sessions zirv supervises. None is implemented. The operator asked for the five highest-impact ones, implemented and shipped as one release PR that auto-closes them.

Selected after verifying each premise against the current code:

- #280 handoff v3: `handoff::distill` never sees the previous handoff, so every restart chain loses earlier decisions; no Constraints / Key decisions / Blocked sections; `files_touched` cannot distinguish read from modified. (intelligence)
- #281 resume injection: nothing a resumed session is told is host-verified; `.zirv/work` artifacts and branch changed paths are never listed; a crash restart is indistinguishable from a planned compaction (`sessions::Record` has no in-flight marker). (intelligence)
- #287 verification no-progress guard: `change_fingerprint` is persisted but a failing report's fingerprint is never compared, so an unchanged worktree re-runs the full check set up to three times. (speed, cost)
- #301 work-group reservation: `resolve_worker_budget` derives each child's ceiling from `remaining` with nothing reserved, so concurrent children can each be authorised the full remainder. (cost)
- #285 durable objective: `--budget-tokens` exhaustion stops the run with `EXIT_BUDGET_EXHAUSTED` and never tells the session to land its work first; `loop` carries no objective across cycles. (cost)

Rejected: #303 (codex headless Compact). Verified 2026-09-02 with codex-cli 0.147.0: `codex exec resume <id> "/compact"` delivers the text as a plain user message (the model answered "I can't invoke /compact from here") and the rollout records no compaction item, so the shared verification path would fail closed on every attempt. Measurement-only issues (#293, #294, #264, #299, #275, #276) and security-posture issues (#262, #272, #267) were judged lower impact than behaviour changes on the supervised session.

## Desired outcome

One release PR (`release/3.11.0-harness-batch`, version 3.11.0) whose description closes #280, #281, #287, #301, #285, with all five CI gates green, the Linux Docker `wrap::` suite passing for the `wrap.rs` changes, vault pages updated per the doc contract, and one review round (claude sonnet + codex) with confirmed findings fixed.

## Constraints

- `rot.rs` stays pure; `wrap` hot path gains no `unwrap`/`expect`; repo-owned surfaces may only narrow (`REPO_FORBIDDEN` for every new operator-only key: `run_budget_tokens`, any `[handoff] working_set_max_lines`).
- Back-compat: v2 handoff markdown, `Record` JSON without `in_flight`, `WorkGroup` JSON without `reserved_tokens`, and older objective records must all still deserialize.
- #287 builds on #268's `GateOutcome` (already merged); no parallel enum.
- No model-asserted completion and no `missing_terminal_evidence` label in #285; budget crossing converts to a handoff, never a kill.
- Windows dev box: diff failure-name lists against `main`; do not chase the pre-existing `wrap::` failures.

## Open questions

None. The operator delegated issue selection with the instruction to verify impact first; selections and the #303 rejection are recorded above.

## Brainstorm

- Q: Which five? A (self-resolved, evidence above): the five whose premise is a verified defect in a live code path of the supervised session, not a measurement or posture gap.
- Q: Keep #303 in place of #285? A: No; the compaction trigger does not exist in `codex exec`, so the feature would be a no-op that always falls back to restart.
- Q: One track or several? A: Three parallel worktree tracks (A: #280; B: #281; C: #287 + #301) then D: #285, merged by the orchestrator; A/B/D overlap in `handoff.rs`, `exec.rs`, `resume.rs` and are merged with conflict review.

## Acceptance criteria

- [ ] `distill` accepts the previous handoff; v3 sections round-trip; v2 files still parse; read vs modified classified by tool name; `COMPACT_FOCUS` asks for constraints and decision rationale.
- [ ] Resume injection appends an existence-checked working-set manifest with caps and the "what did not survive" line; a dead-pid record with `in_flight` set yields exactly one interrupted block, consumed once.
- [ ] A second verify of a step with an unchanged worktree executes no check, still increments attempts, returns a distinct `Unchanged` outcome, and three such attempts still terminate the step.
- [ ] Two concurrent admissions under one group budget cannot both receive the full remainder; a completed child's reservation settles exactly once; a failed spawn releases it.
- [ ] `zirv ctx objective set|show|close` work; the objective layer is emitted once after Context; crossing the budget flips status and swaps in wrap-up text without killing the run; `run_budget_tokens` from a repo `ctx.toml` hard-errors.
- [ ] All five gates pass; Linux `wrap::` suite passes; PR body closes the five issues.
