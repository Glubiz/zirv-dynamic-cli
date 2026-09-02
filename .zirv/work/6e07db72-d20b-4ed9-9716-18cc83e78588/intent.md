# Intent

## Problem

After release 3.11.0 (PR #305) twelve enhancement issues from the Ruflo (#278) and Prime Agent (#279) analyses remain open, all aimed at the cost, speed, or intelligence of the harness sessions zirv supervises. The operator asked for the five highest-impact remaining ones, implemented and shipped as one release PR that auto-closes them.

Selected after verifying each premise against `main` at 985b5e5:

- #267 writer vs reader permits: `permit.rs` has one pool (`supervise.max_heavy_operations`, default 1); read-only review/research workers queue behind an implementer's `cargo test`, and nothing refuses a second writing worker in the same checkout. (speed, intelligence)
- #275 context lint: no `zirv context lint` exists; `common.md` sits at 4047 of 4096 bytes with only the injection-time truncator guarding it; the four-way duplicate and the codex.md contradiction found in PR #261 were found by hand. Injected text is the dominant cost of every worker. (cost, intelligence)
- #264 cost ledger: no `price`/`spend` module; `log::Delegation` rows are never aggregated by session, group, harness, model or day; no dollar figure exists anywhere; `TelemetryEvent.parent_session_id` is never populated. (cost)
- #293 speed signals: `NormalizedEvent` carries no timestamp; `score.rs` derives no latency; `workflow/telemetry.rs` has per-phase `duration_ms` only. The speed axis is unmeasured. (speed)
- #299 prompt-prefix stability: `tests/fixtures/fake-agent.sh` captures argv/env/cwd but never the assembled prompt; no test asserts the injected prefix is byte-stable across turns, which is the one property that moves cache cost. (cost)

Not selected: #294 (L, measurement verb; overlaps #293 and depends on it), #295 (changes the default target of `remember` to a session tier, a product decision for the operator), #262 (authority narrowing, a safety posture not one of the three axes), #272, #276, #302 (hygiene), #260 (Windows-only test bug), #303 (rejected with evidence in 3.11.0).

## Desired outcome

One release PR (`release/3.12.0-harness-batch-2`, version 3.12.0) whose description closes #267, #275, #264, #293, #299, with all five gates green on Windows (failure names equal to the machine baseline), the Linux Docker suite passing (the fake-agent fixture and adapter changes need it), vault pages updated per the doc contract, and one review round (claude sonnet + codex) with confirmed findings fixed.

## Constraints

- `rot.rs` stays pure: timestamps arrive as event data; no verdict changes from #293.
- `wrap` hot path gains no `unwrap`/`expect`; permits keep the `create_new` contention files and dead-owner sweep.
- Repo-owned surfaces may only narrow: `supervise.max_writers`, `price.*`, `[measure]`/lint caps are `REPO_FORBIDDEN`.
- Additive schemas only: `Delegation` gains `mode`/`task_class` as `#[serde(default)]`; telemetry gains optional fields and one kind, `TELEMETRY_SCHEMA_VERSION` unchanged.
- No network sink of any kind; money is integer micro-USD, never floats.
- Windows dev box: diff failure-name lists against `main`; never chase the baselined `wrap::`/`adapters::` failures.

## Open questions

None. The operator delegated selection with the instruction to verify impact first; selections and rejections are recorded above.

## Brainstorm

- Q: Which five? A (self-resolved): the five whose gap is verified on `main` and whose effect lands on every supervised session or worker: parallelism (#267), prefix bytes (#275, #299), spend visibility (#264), latency (#293).
- Q: #294 instead of #299? A: No; #294 is L and consumes #293's metrics, so it belongs in the round after.
- Q: One track or several? A: Five worktree tracks in two waves: A #267, C #293, D #275 in parallel (disjoint files); then B #264 (after A, shares `agent.rs`/`log.rs`/`spawnreq.rs`) and E #299 (after D, shares `compile.rs`), merged by the orchestrator.

## Acceptance criteria

- [ ] Two `read-only` workers run concurrently with `max_writers = 1`; a second `writing` worker in the same tree is refused with a one-line reason; `--worktree` allocates a fresh worktree; writer permits are swept like heavy permits; delegation rows carry `mode`.
- [ ] `zirv context lint` reports CTX001-CTX005 deterministically, writes nothing, warns on `common.md` at 4047/4096 and errors at 4097; CTX001/CTX005 wired into `context sync --report` and a `context-lint` check kind.
- [ ] `price()` is pure and table-tested; `zirv ctx spend --by harness|model|task-class|worker` prints deterministic totals over a fixture ledger; status gains one spend line; dashboard aggregate row renders `--` without a live source; stale price table is flagged.
- [ ] Events carry `at_ms`; `score.rs` reports p50/max turn latency, p50 TTFT, tool-error rate; missing timestamps yield `None`; `zirv workflow stats` prints a speed block; existing rot verdicts unchanged.
- [ ] `FAKE_AGENT_PROMPT_LOG` records byte-exact framed prompts per turn; tests fail on prefix drift with the first differing offset and owning layer; memory harvest and roster refresh perturb the suffix only; `common.md` over 4096 bytes fails a test.
- [ ] All five gates pass; Linux Docker suite passes; PR body closes the five issues.
