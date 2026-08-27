# Token-cost benchmarks

Measurement procedure for issue #155 ("make zirv sessions dramatically
cheaper to run without losing response quality"). Every phase of
`docs/superpowers/plans/2026-08-26-token-cost-reduction.md` is judged by the
numbers this document defines. Without a written procedure, "before/after"
is an opinion — this document exists so two people measuring the same
change get the same number.

**Non-negotiable rule for every row below**: a cell is either a number that
was actually observed with the command shown next to it, or it says "not yet
measured" with the reason and the command that will produce it once data
exists. No estimated, extrapolated, or "plausible" number belongs in this
document. A fabricated number here corrupts issue #155's acceptance
evidence, which is exactly the thing this table exists to prevent.

## 1. Metric definitions

Zirv currently has **two differently-shaped token records**, and the most
common way to get a wrong number is to apply one record's formula to the
other. Read this section before computing anything.

| Source | `input_tokens` means | Cache fields |
| --- | --- | --- |
| `event::TranscriptUsage` (backs `zirv ctx usage --sessions` and `log::Delegation` / `delegations.jsonl`) | raw **uncached** input only | `cache_creation_input_tokens` and `cache_read_input_tokens` are **additional** classes, summed with `input_tokens` by `TranscriptUsage::context_total()` |
| `workflow::telemetry::TelemetryEvent` (`zirv workflow stats`, `TelemetryKind::ReviewRun` etc.) | the **combined** total (raw input + both cache classes already folded in), kept this way since schema v2 (issue #155) | `cache_read_input_tokens` / `cache_creation_input_tokens` are **subsets** of `input_tokens`, never added to it |

Both structs carry the same four field names. They are not interchangeable.

### 1.1 Tokens per completed task

**What counts as "one completed task" today:** the plan's Task 6.4 spec
allows grouping by *one workflow id* (`zirv workflow start` / `status`) or
*one work group id*. As of this writing (2026-08-27, machine state verified
below in §4), **work group grouping does not exist yet** —
`work_group_id` is hard-coded `None` on every `Delegation` record
(`src/commands/ctx/agent.rs`), and no CLI reads `delegations.jsonl` back
(`log::tail_delegations` is `#[allow(dead_code)]`, landed ahead of its
Phase 5 reader). So until Phase 5 ships, "one completed task" means **one
`zirv workflow` id**, and only for work actually run through the workflow
engine (`zirv workflow start/advance/review`) — not for ad hoc chat/agent
sessions, which have no task boundary the tooling can see.

**Arithmetic**: sum the four raw classes (`input_tokens`,
`cache_creation_input_tokens`, `cache_read_input_tokens`, `output_tokens`)
across every session/delegation attributed to the task. Do **not** mix the
two source shapes above in one sum without converting to a common basis
first (e.g. always sum `TranscriptUsage`'s three input-side fields
separately, never `TelemetryEvent.input_tokens` plus its own cache
subsets).

**Commands**:

```bash
# Cross-repo, last 24h, one line per Claude/Codex session (scans every
# ~/.claude/projects/**/*.jsonl transcript on the account — NOT scoped to
# one repository or one task; filter the output yourself by session id).
zirv ctx usage --sessions

# Delegations made specifically through `zirv ctx agent` (has parent_session
# and, once Phase 5 ships, work_group_id, so it is the more precise source
# once populated).
cat "$(zirv ctx status | head -1 | awk '{print $3}')/logs/delegations.jsonl"

# Workflow-scoped telemetry, once a workflow has actually been run:
zirv workflow stats
zirv workflow status <id>
```

### 1.2 Cache-hit ratio

Two formulas, matched to the two shapes in the table above — using the
wrong one for a given source silently under- or over-states the ratio.

- **`TranscriptUsage` sources** (`zirv ctx usage --sessions`,
  `delegations.jsonl`; this is also what `docs/superpowers/specs/...
  -design.md`'s Measurement section formula refers to):

  ```
  cache_read_input_tokens / (input_tokens + cache_creation_input_tokens + cache_read_input_tokens)
  ```

  i.e. `cache_read_input_tokens / context_total()`. This is exactly what
  `usage::render_sessions` prints as `cache-hit %` per session
  (`src/commands/ctx/usage.rs`).

- **`TelemetryEvent` sources** (`zirv workflow stats` and anything reading
  workflow telemetry directly): `input_tokens` is *already* the combined
  total, so the ratio is

  ```
  cache_read_input_tokens / input_tokens
  ```

  This is `TelemetryEvent::cache_hit_ratio()`
  (`src/commands/workflow/telemetry.rs`). Re-deriving it as
  `cache_read / (input + cache_creation + cache_read)` here double-counts
  `cache_creation_input_tokens` and produces a ratio that is too low.

Standing caveat (both formulas, both usage.rs and telemetry.rs say this
verbatim): the vendor's own token-class weighting against its rate limiter
is undocumented, so this ratio is an approximation of **cost**, though an
exact measure of **cache behaviour**.

### 1.3 Review count per change

- **Independent reviewer launches**: `TelemetryKind::ReviewRun` events per
  `workflow_id` (`src/commands/workflow/review.rs:1205` emits one per review
  round via `telemetry::record`).
- **Total review-diff bytes shipped**: `ReviewPackage::diff.len()`
  (`src/commands/workflow/review.rs`) summed over every round for the
  change.
- **Command**: `zirv workflow stats` today only reports an aggregate
  (`events`, `verification`, `findings`, `frontend` counters) — there is no
  per-workflow or per-`ReviewRun` breakdown CLI yet. Until one exists, this
  number has to be read by listing the raw event files directly:

  ```bash
  # <state>/workflow-telemetry/<repo-slug>/*.json, one file per event.
  find "$(dirname "$(zirv ctx status | head -1 | awk '{print $3}')")/../workflow-telemetry" \
    -iname '*.json' | xargs -I{} sh -c 'grep -l "review-run" {}' 2>/dev/null
  ```

  (`workflow-telemetry` lives at `<state>/workflow-telemetry/<repo-slug>/`,
  a sibling of `logs/`; see §4 for whether this directory exists on a given
  machine before relying on it.)

### 1.4 Other columns issue #155's architect comment requires

Issue #155's review comment (the one that added the `WorkGroup`/telemetry
design) is explicit that a single mean is not enough evidence: report
**median and p95** per completed task for the four raw token classes,
**plus**:

- **Five-hour usage delta**: `zirv ctx usage`'s `five_hour: NN.N% used`
  line (collector-sourced when the statusline tee is wired, estimator
  otherwise), read immediately before and immediately after the run;
  delta = after − before. This meter is **account-wide**, not scoped to one
  repository or task — it is contaminated by any other concurrent session on
  the same account (see §4's honesty note: this very session shares an
  account with four other concurrently active agents).
- **Cache-hit ratio**: §1.2.
- **Tool calls**: not currently counted anywhere in zirv's own logs; would
  need to come from the harness transcript directly (`tool_use` blocks per
  session) — not yet measured, no zirv-side command exists.
- **Wall time**: `Delegation.wall_ms` per delegation record, or
  `Event::DelegatedStart`/`DelegatedFinish` timestamps for `zirv ctx agent`
  runs specifically.
- **Completion rate**: fraction of runs that reach `WorkflowCompleted` /
  a non-error `exit_code` on `Delegation.outcome`. Needs a corpus of runs to
  be meaningful — not a single-session number.

## 2. Controlled-comparison protocol

Issue #155's architect comment is explicit: **"A model/settings change
cannot count toward savings."** Pin every one of the following before a
run, and record what was pinned alongside the numbers:

1. **Repository state**: exact commit SHA (not a branch name — branches
   move). Record `git rev-parse HEAD` for both the "before" and "after"
   run; the two runs being compared must otherwise be identical except for
   the code change under test.
2. **Task text**: the literal prompt/task description, verbatim, same for
   both runs.
3. **Model IDs**: exact model id per seat (e.g. orchestrator seat, worker
   seat), never "whichever model was selected" — see
   `.claude/agents/*.md` frontmatter or the explicit `model` parameter each
   dispatch already carries per this project's dispatch policy
   (`CLAUDE.md`: "EVERY `Agent` dispatch sets `model` explicitly").
4. **Effort/settings**: reasoning effort, `--fast` mode on/off, any
   provider-side flags in effect.
5. **Review policy**: number of independent reviewers, their effort level,
   whether delta re-review or full-diff re-review is in effect.
6. **Concurrency**: number of parallel worker/sessions the run fans out to.
   A run sharing the machine/account with unrelated concurrent sessions is
   not a clean comparison — say so explicitly if it wasn't isolated.
7. **Capture window**: run `zirv ctx usage --sessions` (or, once available,
   read `delegations.jsonl`) immediately before starting and immediately
   after the run completes, so the token delta is bounded to the run's own
   wall-clock window rather than "whatever the 24h lookback happens to
   contain."

State plainly, every time: **these are single-run observations on one
machine, not a benchmark suite.** An honest small number beats a fabricated
rigorous one. To get median/p95 (§1.4), the same pinned configuration has
to be run multiple times — a single run only ever contributes one data
point to that distribution.

## 3. Results table

One row per phase. `status` is `measured` only when every cell in that row
came from an actual paired before/after run under the protocol in §2;
otherwise it is `not yet measured` and the row says why.

| Phase | Tokens/task (median) | Tokens/task (p95) | Cache-hit ratio | Review count/change | 5h usage Δ | Tool calls | Wall time | Completion rate | Status |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 — prompt-injection diet | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured — no paired before/after run has been executed under the §2 protocol on this machine |
| 2 — telemetry (P0) | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured — telemetry only records what §4 shows is currently zero events on this machine |
| 3 — context dedupe | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured — same reason as Phase 1 |
| 4 — review convergence | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured — needs `ReviewRun` telemetry, which is empty (§4) |
| 5 — work groups / budgets | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | not started per the epic status as of 2026-08-27 |
| 6 — model-aware rotation / quota scheduling | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | not started per the epic status as of 2026-08-27 |

To fill a row in: run the §2 protocol on the commit immediately before and
immediately after that phase's merge, using the exact commands in §1, and
replace the corresponding cells.

## 4. What data actually exists on this machine (2026-08-27)

Investigated before writing this document, so the "not yet measured" rows
above are a finding, not a placeholder. Everything below is a real command
run on this machine with its real output.

**`zirv --version` / repo state**: installed binary reports `Version:
2.31.0`; `origin/main` is at the same tag (commit `110332e`, "Merge pull
request #161 from Glubiz/release/2.31.0"). The Phase 2 telemetry shapes
described in §1 (`TranscriptUsage`'s four raw classes,
`TelemetryEvent` schema v2, `Delegation`/`delegations.jsonl`) are already
present in this exact source tree — this is not a future-looking spec.

**`delegations.jsonl` does not exist anywhere on this machine**:

```
$ find "/Users/jonathansolskov/Library/Application Support/zirv" -iname "delegations.jsonl"
(no output)
```

The write path is wired (`agent.rs` calls `log::append_delegation` after
every `zirv ctx agent` run, best-effort), but zero files exist. The likely
reason, confirmed by cross-checking `zirv ctx status` (below): the
sessions actually observed on this machine were launched as native
Claude/Codex agent-teammate sessions (the harness's own `Agent`/`Task`
tool — the same mechanism that spawned the session writing this document),
**not** through the `zirv ctx agent` CLI subcommand. `zirv ctx usage
--sessions` still sees them, because it scans transcript files directly
rather than reading `delegations.jsonl`; the delegation-accounting path
specifically has simply never been exercised here.

**`zirv workflow stats` reports zero events**:

```
$ zirv workflow stats
events: 0
slowest phase: unknown
most token-expensive phase: unknown
verification: 0 runs, 0 failures
findings: 0 total, 0 meaningful, 0 dismissed
frontend: detector 0 runs/0 failures, render 0 runs/0 failures, visual review 0 runs/0 failures
```

Telemetry is enabled by default (`workflow.telemetry_enabled = true`,
unoverridden in this repo's `.zirv/ctx.toml`), and no
`<state>/workflow-telemetry/` directory exists at all. This means the
`zirv workflow` engine (`start`/`advance`/`review`) has never actually run
on this machine — the epic's own PRs (#156–#159) were built and reviewed
through native harness sessions, not through `zirv workflow`. Consequence:
**§1.1's workflow-id grouping and §1.3's review-count metric have no data
source yet**, for a concrete, verifiable reason — not because the feature
is unbuilt (it is built and covered by the inline tests in
`telemetry.rs`/`review.rs`), but because the workflow engine itself hasn't
been invoked here.

**`decisions.jsonl` has real data, but no token counts**: 1,545 lines,
global across every repository on this machine (no per-repo path segment,
unlike `memory/`, `mail/`, `optimize/`). It records verb/verdict/score/action
for rotation decisions (compact, prompt-injected, etc.), not tokens — it is
useful for correlating *when* a session was active, not *how much* it cost.

**`zirv ctx usage --sessions` is the one command on this machine that
genuinely produces real, current raw-token numbers**, and it does so
today:

```
$ zirv ctx usage --sessions
collector (server-authoritative, from the statusline tee):
  five_hour: 63.0% used (collector, observed 44s ago, resets at unix 1787823000)
  seven_day: 20.0% used (collector, observed 44s ago, resets at unix 1788343200)

estimator: off (set pace.five_hour_budget_tokens or pace.seven_day_budget_tokens to enable it)

pacing:
  ceiling 99.0%
  wait bound: five_hour up to 21600s, seven_day up to 608400s (each window's own length plus slack)
  verdict: usage 63.0% of the limit (collector data), proceeding

sessions (last 24h, raw token classes):
  agent-a1337d8561fd219c4: input 1476 | cache_creation 1257737 | cache_read 304024707 | output 301174 | cache-hit 99.6% | events 738
  6e5ac20f-cd0a-4ab8-9908-423a13474bd7: input 950 | cache_creation 2943765 | cache_read 104321445 | output 430913 | cache-hit 97.3% | events 476
  agent-ae057f621d6b26048: input 744 | cache_creation 2146791 | cache_read 94807871 | output 242974 | cache-hit 97.8% | events 372
  ... (46 sessions total, cache-hit ranging 0.0%-99.6%)
  token class weighting is undocumented, so treat these as an approximation, never ground truth.
```

This is a **real, reproducible snapshot**, not a benchmark result: it is
explicitly excluded from §3's table because it fails several §2 protocol
requirements by construction —

- it aggregates the **last 24 hours across every repository** on the
  account, not one pinned task;
- it is **not a paired before/after** of one specific code change;
- it was captured while **this exact session shares the account with at
  least four other concurrently active agents** (`team-lead`, `planner-p5`,
  `planner-p6`, `worker-41`, `worker-52` — visible in the output above as
  separate `agent-a*` rows), so the five-hour delta and aggregate cache-hit
  ratio reflect simultaneous unrelated work, not one isolated run.

It does, however, prove the instrumentation this whole document depends on
is live and correctly shaped today — the per-session raw four-class
breakdown and cache-hit percentage are real numbers, not a design-doc
promise.

## 5. Producing the Phase 1–6 numbers once data exists

1. Land the phase's PR (or check it out at its merge commit).
2. `git rev-parse HEAD~1` (before) and `git rev-parse HEAD` (after, if the
   phase is exactly one commit — record the actual before/after SHAs used).
3. Run the same pinned task (§2) on each commit, back to back, with no
   other concurrent session on the account.
4. `zirv ctx usage --sessions` immediately before and after each run;
   `zirv workflow stats` / the workflow-telemetry files (§1.3) if the task
   was run through `zirv workflow`.
5. Compute §1.1–§1.2 by hand from the captured numbers (no CLI currently
   automates the subtraction/aggregation across a capture window — that is
   a gap, not a step to skip).
6. Repeat enough times to report median and p95 (§1.4); a single run is one
   data point, not a distribution.
7. Fill the row in §3, replacing "not yet measured" with the number and a
   link/reference to the raw capture.
