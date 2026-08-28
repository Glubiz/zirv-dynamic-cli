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
- **Command** (issue #155 measurement closeout, landed alongside this
  update): `zirv workflow stats` now reports a per-`workflow_id` breakdown
  in addition to the flat aggregate — `WorkflowStats`
  (`src/commands/workflow/telemetry.rs`) carries each workflow's
  `ReviewRun` count, its findings totals (`findings_total` = every finding
  *reported*, `findings_meaningful` = the confirmed subset: Major/Critical
  severity and not dismissed, `findings_dismissed`), its token totals, and
  `confirmed_findings_per_review` (`findings_meaningful / review_runs`).
  The overall report also carries a combined `review_runs` count and
  `review_defect_rate` (confirmed findings per review round, across every
  workflow) — the "no regression in review-confirmed defect rates"
  accounting hook the epic's acceptance criteria calls for. Plain-text
  output:

  ```
  $ zirv workflow stats
  ...
  review-confirmed defect rate: 0.00 confirmed findings/review round (2 review runs)
  workflows:
    136dc486-...: 2 review runs, findings 3 total/1 confirmed/2 dismissed, 45231 tokens (2 measured events), 0.50 confirmed/review
  ...
  ```

  `--json` emits the same data as the `workflows` map and top-level
  `review_runs`/`review_defect_rate` fields on `StatsReport`. Before this
  breakdown existed, the only way to read this data was listing the raw
  event files directly (`<state>/workflow-telemetry/<repo-slug>/*.json`,
  one file per event, a sibling of `logs/`) and grepping for
  `"review-run"` — that path still works for anything the summary doesn't
  cover (e.g. `ReviewPackage::diff.len()` above, which is not recorded in
  telemetry at all).

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

All six phases merged within roughly 36 hours of each other (v2.31.0:
2026-08-27 07:42 UTC, phases 1-4; v2.32.0: 2026-08-28 04:02 UTC, phases 5-6
— both `git log` timestamps for the release merge commits). No transcript
data exists between those merges in isolation, so **no individual phase can
be separated from the others on this dataset** — every phase row below says
so explicitly rather than claiming a per-phase number the data cannot
support.

| Phase | Tokens/task (median) | Tokens/task (p95) | Cache-hit ratio | Review count/change | 5h usage Δ | Tool calls | Wall time | Completion rate | Status |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 — prompt-injection diet | not separable from combined epoch measurement on this dataset | not separable from combined epoch measurement on this dataset | not separable from combined epoch measurement on this dataset | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | shipped in v2.31.0 (2026-08-27); see §3.1 for the combined pre/post-epoch measurement covering all 6 phases together |
| 2 — telemetry (P0) | not separable from combined epoch measurement on this dataset | not separable from combined epoch measurement on this dataset | not separable from combined epoch measurement on this dataset | not yet measured (0 production `ReviewRun` events — see §4) | not yet measured | not yet measured | not yet measured | not yet measured | shipped in v2.31.0 (2026-08-27); the instrumentation itself is real and unit-tested (`telemetry.rs`), but has never recorded a production event outside the one synthetic run in §4 |
| 3 — context dedupe | not separable from combined epoch measurement on this dataset | not separable from combined epoch measurement on this dataset | not separable from combined epoch measurement on this dataset | not yet measured | not yet measured | not yet measured | not yet measured | not yet measured | shipped in v2.31.0 (2026-08-27); see §3.1 |
| 4 — review convergence | not separable from combined epoch measurement on this dataset | not separable from combined epoch measurement on this dataset | not separable from combined epoch measurement on this dataset | not yet measured (0 production `ReviewRun` events) | not yet measured | not yet measured | not yet measured | not yet measured | shipped in v2.31.0 (2026-08-27); needs `ReviewRun` telemetry, which is still empty in production (§4) |
| 5 — work groups / budgets | not separable from combined epoch measurement on this dataset | not separable from combined epoch measurement on this dataset | not separable from combined epoch measurement on this dataset | not applicable | not yet measured | not yet measured | not yet measured | not yet measured | shipped in v2.32.0 (2026-08-28); see §3.1 |
| 6 — model-aware rotation / quota scheduling | not separable from combined epoch measurement on this dataset | not separable from combined epoch measurement on this dataset | not separable from combined epoch measurement on this dataset | not applicable | not yet measured | not yet measured | not yet measured | not yet measured | shipped in v2.32.0 (2026-08-28); see §3.1 |

To fill a row in individually: run the §2 protocol on the commit
immediately before and immediately after that phase's merge, using the
exact commands in §1, and replace the corresponding cells. That has not
been done for any phase — every "measured" number in this document (§3.1)
is a combined pre/post-epoch comparison across all 6 phases together, not a
controlled single-phase A/B run.

### 3.1 Combined epoch measurement (issue #155 acceptance evidence)

**Method** (full detail and caveats in §4.1): every `*.jsonl` Claude Code
transcript under this machine's zirv-related project directories, bucketed
by each transcript's latest timestamp into pre-epoch (before
2026-08-27T00:00:00Z, before any phase shipped) and post-epoch (on or
after). The post-epoch bucket is coarser than the release timeline: the
v2.31.0 release merge landed at 2026-08-27T07:42Z, so post-epoch sessions
from the first ~7.7 hours of 2026-08-27 predate every shipped phase, and
the full 6-phase stack (v2.31.0 through the current v2.33.x) is only
guaranteed in effect for sessions after that merge; this slice is not
separated out and its size is unquantified. One
transcript file = one "session" (including `subagents/*.jsonl` files,
matching `window::session_spend`'s own definition — see §4.1). Produced by
`docs/benchmarks/token_cost_analysis.py`; raw output committed at
`docs/benchmarks/token_cost_report.json`.

This is **not** the workflow-id-scoped "one completed task" §1.1 defines —
production `zirv workflow` usage is still zero (§4), so no workflow-id
grouping exists to measure. It is the best proxy this dataset supports: one
transcript file's total spend. **This is a single uncontrolled comparison
across every session on one developer's machine over ~2 weeks, not a
paired before/after run under the §2 protocol** — read as directional
evidence, not a rigorous per-phase benchmark.

The dominant session type in both buckets is a `subagents/*.jsonl` file
(489 of 538 pre-epoch sessions, 108 of 112 post-epoch), so that population
is the fairest same-shape before/after comparison available:

| Metric (subagent sessions, n=489 pre / 108 post) | Pre-epoch (median) | Post-epoch (median) | Pre-epoch (p95) | Post-epoch (p95) |
| --- | --- | --- | --- | --- |
| Output tokens/session | 21,723 | 817.5 | 124,343 | 39,128 |
| Fresh input tokens/session (`input_tokens + cache_creation_input_tokens`) | 318,155 | 157,062 | 1,982,406 | 1,489,852 |
| Cache-read tokens/session | 4,591,413 | 1,401,650.5 | — | — |
| Cache-hit ratio/session (`cache_read / context_total`) | 0.937 | 0.929 | — | — |
| Message count/session | 59 | 29 | — | — |

That is a **~96% reduction in median output tokens/session** and a **~51%
reduction in median fresh-input tokens/session**, with cache-hit ratio
essentially flat (0.937 → 0.929 — within the noise this dataset can
resolve, not a measured improvement). Totals across the same population:
pre-epoch 489 sessions summed 18,607,309 output / 304,007,086 fresh-input /
10,150,226,195 cache-read tokens; post-epoch 108 sessions summed 851,981 /
39,721,719 / 1,405,552,045 respectively (full figures in the committed
`token_cost_report.json`, `subagent_only` key).

**Caveat that matters most**: this reduction is *not* isolated to the code
change. It is confounded with a real shift in how sessions get launched —
post-epoch subagent sessions skew toward narrower, more numerous dispatches
(consistent with, but not proof of, Phase 1's prompt-injection diet and
Phase 3's context dedupe encouraging smaller per-agent scopes), and the
absolute post-epoch sample (108 sessions, ~1.5 days) is far smaller than
pre-epoch (489 sessions, ~2 weeks). The **all-sessions** and **top-level-only**
views in §4.1 show just how workload-dependent this number is: top-level
(non-subagent) sessions actually moved in the *opposite* direction
(median output tokens/session 2,650 pre → 283,921 post) because the 4
post-epoch top-level sessions on this machine are long multi-day
orchestration epics (this document's own authoring session among them),
not because per-session cost regressed — an n=4 bucket dominated by one
session shape is not comparable to an n=49 bucket of mostly short sessions.
**Neither direction should be read as "the phase made X% difference"; both
are real numbers from real logs, reported plainly, with the confound
stated.**

## 4. What data actually exists on this machine

Investigated before writing this document, so the "not yet measured" rows
above are a finding, not a placeholder. Everything below is a real command
run on this machine with its real output. §4 (below the 2026-08-27
sub-heading) is the original investigation; §4.1–§4.3 are the issue #155
measurement-closeout update run on 2026-08-28, kept alongside rather than
overwriting it — both are real snapshots of the same machine two days
apart, and the diff between them (workflow stats going from zero to
non-zero, decisions.jsonl growing) is itself informative.

### 4.0 Original investigation (2026-08-27)

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

### 4.1 Token-cost measurement method and script (2026-08-28)

§3.1's numbers come from `docs/benchmarks/token_cost_analysis.py`, run
against this machine's real logs, not estimated. Method, in full:

1. **Source data**: every `*.jsonl` file (recursively, including
   `subagents/*.jsonl`) under `C:\Users\josj\.claude\projects\` in any
   directory whose name contains `zirv-dynamic-cli` or `zirv-perms`. On
   this run that resolved to the primary `D--GitHub-zirv-dynamic-cli`
   project directory plus three worktree/scratchpad-cwd project
   directories that are also `zirv-dynamic-cli` work (sessions whose `cwd`
   was a scratchpad path still nested under this repo's work). **No
   `zirv-perms` project directory exists on this machine** — the
   cross-harness-permissions work referenced in this session's own memory
   is being done through a different harness (codex) that does not write
   `~/.claude/projects/` transcripts, so it contributes nothing to this
   measurement and is not claimed to. An unrelated sibling directory,
   `D--GitHub-zirv-fitness-tracking`, exists but was deliberately excluded
   — it is a different repository, not zirv-dynamic-cli or zirv-perms
   work.
2. **Session definition**: one `*.jsonl` file = one session, exactly
   `window::session_spend`'s own definition (`src/commands/ctx/window.rs`)
   — a `subagents/*.jsonl` file is its own session, not folded into its
   parent, because it spends the account's budget independently.
3. **Token extraction**: every row with `"type":"assistant"` and a
   `message.usage` object contributes its `input_tokens`,
   `cache_creation_input_tokens`, `cache_read_input_tokens`,
   `output_tokens`, summed across every such row in the file — mirroring
   `adapters::claude::fold_assistant_usage`'s summation (usage is
   per-API-call, not cumulative, so every row's usage is real spend, not a
   running total to take the last value of).
4. **Derived fields**: `fresh_input = input_tokens +
   cache_creation_input_tokens`; `context_total = fresh_input +
   cache_read_input_tokens`; `cache_hit_ratio = cache_read_input_tokens /
   context_total` (§1.2's `TranscriptUsage`-source formula).
5. **Session date**: the latest parseable `timestamp` row in the file, or
   the file's mtime if no row has one.
6. **Bucketing**: `date < 2026-08-27T00:00:00Z` → pre-epoch;
   `date >= 2026-08-27T00:00:00Z` → post-epoch. This is a single cutoff
   for the whole epic, per the task brief for this measurement — it is
   coarser than the two actual release timestamps (v2.31.0 phases 1-4 at
   2026-08-27T07:42Z, v2.32.0 phases 5-6 at 2026-08-28T04:02Z), both of
   which land inside "post-epoch" by this cutoff, which is exactly why §3
   cannot separate the six phases from each other.
7. **p95**: nearest-rank, `sorted[ceil(0.95 * n) - 1]` — plain and
   reproducible by hand, no interpolation-method ambiguity.

Every number is the script's direct output; none is rounded, extrapolated,
or hand-adjusted (percentages in §3.1's prose are the only derived
arithmetic, computed from the exact medians in the table). The full script
is committed at `docs/benchmarks/token_cost_analysis.py`; its raw JSON
output from the run this document's numbers were taken from is committed
at `docs/benchmarks/token_cost_report.json`. To reproduce:

```bash
python docs/benchmarks/token_cost_analysis.py > docs/benchmarks/token_cost_report.json
```

(Run with `python.exe` directly, full path, rather than through this
machine's default shell — see this repository's CLAUDE.md note about a
broken Bash-tool PATH on this machine; the script itself has no zirv- or
cargo-specific dependency and works from any working directory.)

**Standing caveats, stated plainly** (same spirit as §2's protocol, applied
retroactively since this was not a paired before/after run):

- **Uncontrolled workload.** Sessions in both buckets cover unrelated
  tasks of wildly different shapes and sizes — this is not the same task
  run twice.
- **One machine, one account.** No cross-machine or cross-account
  variance is captured.
- **Concurrent sessions.** Many sessions in both buckets ran concurrently
  with other unrelated sessions sharing the account's rate limits; this
  affects wall-clock pacing, not the per-session token sums reported here
  (which come from each session's own transcript), but is worth stating.
- **Selection/mix confound, the dominant one for this dataset**: as §3.1
  says, the post-epoch population skews toward more, smaller subagent
  dispatches, and the tiny top-level-only sample (n=4) is dominated by
  long orchestration sessions including this document's own authoring
  session. This is disclosed in numeric form (top-level-only and
  all-sessions views both in `token_cost_report.json`), not hidden.
- **Not workflow-id scoped.** §1.1's "one completed task" definition
  (one `zirv workflow` id) has no data yet (§4.2) — this measurement uses
  "one transcript file" as the closest available proxy, which is a
  different and coarser unit.

### 4.2 Workflow telemetry: exercised for real, once (2026-08-28)

The original 2026-08-27 investigation below found `zirv workflow stats`
reporting zero events on this machine, and it is still true that no phase
of the epic's own PRs (#156–#161, #168–#170) were themselves built through
`zirv workflow` — they were built through native harness sessions. Per
this measurement closeout's own scope (best-effort, no paid harness
session launched to do it), one real local invocation was run in this
worktree to confirm the write path actually produces a non-zero
`zirv workflow stats` today, exercising the exact code path this document
depends on:

```
$ zirv workflow start bugfix --task "issue 155 measurement closeout: per-workflow ReviewRun breakdown in workflow stats" --path src/commands/workflow/telemetry.rs --changed-lines 150
workflow: 136dc486-d320-4202-ba75-92b51f69f32d
kind: bugfix
profile: Standard
status: Running
classification: Bugfix/Bounded risk=58 (High)
current: debug (debug, skill systematic-debugging)

$ zirv workflow advance 136dc486-d320-4202-ba75-92b51f69f32d --outcome success
$ zirv workflow advance 136dc486-d320-4202-ba75-92b51f69f32d --outcome success
current: test (test, skill testing)
completed: debug, implement

$ zirv workflow stats
events: 3
debug: 1 events, 75000 ms, 2858476 tokens (1 measured events), 0 failures
implement: 1 events, 17000 ms, 444770 tokens (1 measured events), 0 failures
adapter claude: 2 events, 92000 ms, 3303246 tokens (2 measured events), 0 failures
token sources: harness-transcript-delta=2
slowest phase: debug
most token-expensive phase: debug
verification: 0 runs, 0 failures
findings: 0 total, 0 meaningful, 0 dismissed
review-confirmed defect rate: no ReviewRun events recorded yet
workflows:
  136dc486-d320-4202-ba75-92b51f69f32d: 0 review runs, findings 0 total/0 confirmed/0 dismissed, 3303246 tokens (2 measured events), no review runs
frontend: detector 0 runs/0 failures, render 0 runs/0 failures, visual review 0 runs/0 failures
```

This proves two things with real output rather than a design-doc claim:
the per-workflow breakdown landed in this same change (§1.3) is live and
correct against a real workflow instance, and `enrich_transition_evidence`
pulled the token/duration numbers above (`harness-transcript-delta`) from
*this session's own real transcript delta* between the two `advance`
calls — not a fabricated or hand-entered figure. The workflow was
deliberately stopped at the `test` step rather than advanced into `review`:
reaching `zirv workflow review` requires an independent reviewer launch,
which this task's scope explicitly excludes (no paid harness session may
be launched to produce this document). Consequence: **§1.3's review-count
metric and §3's phase 2/4 "review telemetry" cells are still 0 production
events** — one synthetic `PhaseCompleted`/`PhaseCompleted` pair is not
production evidence of Phase 2 or Phase 4's effect, and this document does
not claim it is.

### 4.3 Refreshed findings (2026-08-28)

- **`decisions.jsonl`**: 1,700 lines now (was 1,545 on 2026-08-27), still
  global across every repository, still no token counts — useful for
  *when*, not *how much*.
- **`delegations.jsonl` still does not exist anywhere on this machine**,
  and no `groups/` directory exists under `<state>/ctx/` either — Phase
  5's `WorkGroup` state (`zirv ctx group create|status|close`, landed in
  v2.32.0 per `git log`: `fecd0b1`, `12dcab5`, `c2f720e`) has shipped in
  code but, like the workflow engine, has not actually been invoked as a
  CLI verb on this machine; the epic's own delegation work continues to
  run through native harness `Agent`/`Task` tool calls, which this
  accounting path was never wired to observe.
- **Installed binary**: `Version: 2.33.0` (this document's own change
  lands in the next release above that). Phases 5-6 (work groups,
  model-aware rotation/quota scheduling) **have shipped** — v2.32.0's
  release-merge commits (`6392620`, `7b37034`, plus `45d6454` /
  `a805ecd`) — contradicting the original 2026-08-27 investigation's "not
  started" phase-5/6 status; §3's phase 5/6 rows are updated accordingly.

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
