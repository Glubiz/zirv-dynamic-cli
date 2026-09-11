# Native-runtime quality baseline

Recorded-run protocol for issue #470 (step N01 of the native-runtime roadmap,
#469). No runtime default may migrate off `RuntimeKind::Harness` until a
`native` run of a representative task class has been measured against this
baseline and shown not to regress it. This document defines what counts as a
representative task, what gets recorded, and the non-regression rule a
`native` run has to clear. The companion `native-runtime-baseline.jsonl` is
created empty by this step — the orchestrator appends the first real,
measured row once one exists.

**Non-negotiable rule, the same one `docs/benchmarks/token-cost.md` states
for its own numbers**: a field in `native-runtime-baseline.jsonl` is either a
number actually observed for that run, or the row is not written yet. No
estimated, extrapolated, or "plausible" number belongs in this file — a
fabricated baseline defeats the one thing this document exists to prevent:
shipping a native default that is quietly worse and nobody can tell, because
nothing honest was ever recorded to compare it against.

## 1. What counts as a representative task

Two task classes, both already run routinely under the harness backend today
and both large enough to exercise a meaningful slice of the roadmap:

1. **A roadmap implementation step itself** (N01 through N23, #470-#492) —
   the PR that implements one step, reviewed and merged through zirv's own
   `workflow review`/orchestrator process.
2. **Any release-batch PR** — the recurring pattern already visible in this
   repository's history (e.g. the PR #400/#404/#442/#459 series): several
   issues fixed together, gated through the same test/review/verify flow.

A task qualifies the moment it merges. It does not need to be picked in
advance — record it retroactively from the artifacts named in §2.

## 2. What gets recorded, and where the numbers come from

One JSON line per recorded run in `native-runtime-baseline.jsonl`, fields:

| Field | Type | Source |
|---|---|---|
| `recorded_at` | ISO date | the date the row was written, not the date the task ran |
| `task` | string | the issue reference (`#470`) or PR reference (`#459`) the run implements |
| `runtime` | `"harness"` \| `"native"` | which `RuntimeKind` drove the work |
| `harness` | string | the harness that ran it (`claude`, `codex`, ...); for a `native` run, the provider route |
| `orchestrator_model` | string | the orchestrator seat's model for the run |
| `worker_models` | array of string | distinct models any dispatched worker used |
| `gates_first_pass` | bool | whether every workflow gate (test/review/verify) passed on its first run, per `zirv workflow status`/`zirv workflow stats` for that workflow id |
| `review_rounds` | int | `workflow review list`'s round count for the task, or the PR's own review-round history when no `zirv workflow` id exists |
| `confirmed_findings` | int | findings a reviewer raised that were actually fixed (not raised-and-dismissed) — read off the PR's review thread or `zirv workflow review list --json` |
| `delegations` | int | count of worker dispatches for the task, from `zirv ctx status --json`'s session history or `delegations.jsonl` |
| `tokens.input` / `tokens.output` / `tokens.cache_read` / `tokens.cache_creation` | int | `zirv ctx usage`/`zirv ctx spend` totals for the sessions that did the work, using `TranscriptUsage`'s field meanings (raw input separate from cache classes) per `docs/benchmarks/token-cost.md` §1 — never `TelemetryEvent`'s combined-total shape, to avoid exactly the mismatch that document warns about. `tokens` may be `null` when the harness does not expose per-session totals; the `notes` field must then say why. |
| `wall_minutes` | number | elapsed time from the task's first session start to its merge, from session/workflow timestamps |
| `notes` | string | anything that affects comparability (a rerun after a flake, a partial-scope task, a harness outage) |

## 3. Non-regression rule

Comparing a `native` run against the nearest `harness` run of the same task
class (roadmap step vs. roadmap step, release batch vs. release batch):

- `gates_first_pass` must not go from `true` to `false`.
- `confirmed_findings` must not increase for comparable scope — a `native`
  run finding *more* real bugs than review would have caught anyway is fine;
  needing more review rounds to reach the same confirmed set is a
  regression.
- `tokens.*` and `wall_minutes` are reported honestly either way — the rule
  is disclosure, not a ceiling. A `native` run costing more tokens or wall
  time than its `harness` comparison is not disqualifying by itself, but it
  must be visible in this file, not smoothed over.

A `native` run that fails either of the first two rules does not block that
one task from shipping, but it does block `native` from becoming any
default — the roadmap step responsible (N01 through N23) stays the owner of
fixing whatever regressed before that happens.

## 4. Machine state

As of this writing (2026-09-11), no `native` runtime exists yet — N02
through N23 have not landed. `native-runtime-baseline.jsonl` is created
empty by this step. The first row this file will ever honestly contain is a
`"runtime": "harness"` baseline for whichever roadmap step or release batch
is recorded first; a `"runtime": "native"` row cannot exist until a real
native backend exists to produce one.
