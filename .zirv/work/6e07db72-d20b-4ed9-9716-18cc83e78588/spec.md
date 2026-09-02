# Specification

## Context

Five verified gaps in the cost, speed and intelligence of supervised sessions (see intent.md). Each issue body (#267, #275, #264, #293, #299) already carries a file-level design with `file:line` evidence; this spec fixes the cross-issue decisions and the integration order.

Evidence checked on `main` at 985b5e5:

- `permit.rs`: one pool (`acquire(state, limit, label)`, `PermitRecord`, `HeavyPermit`, `live_records`, dead-owner sweep); `config.rs:162 supervise.max_heavy_operations` is `REPO_FORBIDDEN`; `agent.rs` `Args` has `role/group/budget_tokens/force/workdir`, no mode.
- `src/commands/ctx/` has no `price`, `spend`, `measure`, `context_lint` or `envelope` module. `log::Delegation` (`log.rs:85`) carries the four token classes, `wall_ms`, `exit_code`, `outcome`, `agent`, `model`, `parent_session`, `work_group_id`; nothing aggregates it. `agent.rs:1259` already passes `args.group` into `work_group_id`; the dashboard Spawn overlay is a delegation root by construction, so the "always None" premise of #264 is stale and attribution work reduces to `task_class` and telemetry `parent_session_id`.
- `event.rs:170 NormalizedEvent` has no timestamp on any variant; `window.rs:207 parse_iso8601_utc` returns whole seconds; `score.rs` has no latency derivation; `telemetry.rs:71 TelemetryKind` has no speed kind.
- `tests/fixtures/fake-agent.sh` logs `ARGV`, `SESSION/GROUP/PARENT/HEADLESS` env, `CWD`, mode sequence, compaction event; nothing captures the prompt. `.zirv/context/common.md` is 4047 bytes against `context.max_common_bytes = 4096`.
- `context.rs` has no lint verb; `compile.rs:735 compile` / `:779 compile_with_harness_roster` assemble layers with per-layer budgets and a `context-truncated` log line on overflow.

## Goals

- #267: `--mode read-only|writing` (default `writing`) on `zirv ctx agent` (and the `zirv agent` alias), a second permit pool `supervise.max_writers` (default 1, `REPO_FORBIDDEN`), writer permit held for a writing worker's whole lifetime keyed by canonical tree path, refusal of a second writer in a live tree with a retryable one-line reason, `--worktree` allocating `<repo>/.zirv/worktrees/<short>` from the session base and passing it as `--workdir`, `mode` on `SpawnRequest`/ack and on `Delegation` rows, `auto_spawn_decision` marking review/test/verify spawns `read-only`, one integration-owner sentence in each orchestrator prompt.
- #275: pure `context_lint.rs` over the layers `compile.rs` already assembles: CTX001 headroom (warn ≥ 90 %, error on overflow), CTX002 duplicate imperative sentence (token-Jaccard ≥ 0.6), CTX003 contradiction candidate, CTX004 proportionality counts, CTX005 dedupe leak; `zirv context lint [--json] [--fix-plan]` never writes; CTX001/CTX005 surface in `context sync --report` and as `CheckKind::ContextLint`; pair cap `context.lint_max_pairs` (default 20 000, `REPO_FORBIDDEN`) with a `degraded` note.
- #264: pure `price.rs` (built-in table with `as_of`, integer micro-USD, `~/.zirv/prices.toml` operator override, stale after `price.stale_after_days` default 90), `Delegation.task_class`, telemetry `parent_session_id` populated plus `cost_micros`/`price_as_of`, read-only `zirv ctx spend [--session|--group|--since|--by harness|model|task-class|worker] [--json]` with `"schema": 1`, one `spend:` line in `zirv ctx status`, a dashboard aggregate row under a source contract (`--` without a live source), cost per workflow/step in `zirv workflow stats`.
- #293: `at_ms: Option<u64>` on `TurnStart`, `AssistantFinal`, `ToolCall`, `ToolResult` (as fields, since #293's design accepts touching the exhaustive matches; where a match would break, add the field with `..` only in that match) plus a new `AssistantFirstText { at_ms }` variant; `parse_iso8601_utc_ms` in `window.rs`; both adapters fill timestamps (codex may fill `None`); `score.rs` derives p50/max turn latency, p50 TTFT, tool-error rate; `TelemetryKind::TurnLatencySampled` with optional fields; `zirv workflow stats` speed block.
- #299: `FAKE_AGENT_PROMPT_LOG` in the fixture (framed `\x1e<turn>\x1e` + raw bytes, no normalisation); a shared test helper `prefix_diff(a, b) -> Option<(offset, context_a, context_b)>` plus layer attribution from `CompiledContext` provenance; tests for prefix identity across N turns, declared-suffix perturbations (memory harvest, roster refresh, mail arrival), a deliberate one-byte prefix change reporting offset and layer, and `common.md < 4096` with the headroom printed on failure.

## Non-goals

- Path-scoped leases between writers, enforcement inside Codex's sandbox (#267).
- A quality grade, auto-rewriting layers (#275).
- Dollar budget enforcement, pricing the orchestrator seat beyond the usage tee (#264).
- Any network telemetry; a latency-driven rot signal (#293).
- Prompt-variant outcome comparison, a scripted response queue, real provider cache hits (#299).
- #294 `zirv ctx measure`; it consumes #293's metrics and lands later.

## Design

Cross-issue decisions:

1. **`Delegation` gains two additive fields.** #267 adds `mode: Option<WorkerMode>` and #264 adds `task_class: Option<TaskClass>`, both `#[serde(default)]`; readers of old rows see `None`. The row-writing sites are `agent.rs` (own-process path) and `dash/spawnreq.rs` fulfilment; both tracks touch them, Track B (#264) merges after Track A (#267).
2. **Telemetry stays additive.** #293 adds one kind and four optional fields; #264 adds `cost_micros`, `price_as_of`, populates `parent_session_id`. `TELEMETRY_SCHEMA_VERSION` is not bumped; a legacy event file round-trips. `zirv workflow stats` gains two blocks, "speed" (#293) and "cost" (#264), each printing "no data" when absent. Track C (#293) lands its telemetry change first; Track B rebases.
3. **Timestamps are data.** Adapters parse them; `score.rs` derives; `rot.rs` receives events whose timestamp fields it never reads. A pinned test asserts identical verdicts with timestamps present and absent.
4. **Lint and prefix harness share `compile.rs`'s layer view.** #275 needs the assembled layers with budgets and provenance; #299 needs provenance to name the owning layer of a differing byte. One accessor on `CompiledContext` (layers in emission order with `PromptSource`, byte range, budget) serves both; Track D (#275) adds it, Track E (#299) uses it.
5. **Spend is read-only; prices are operator-only.** `price.rs` and `spend.rs` are pure over ledger rows; `[price]` keys are `REPO_FORBIDDEN`; the built-in table is the only default and an unknown model yields `None`, never zero.
6. **Permits: two pools, one file scheme.** Writer permits reuse `PermitRecord` with a `kind` discriminator and the same `create_new` + dead-owner sweep; the heavy pool is unchanged. A read-only worker takes heavy permits only for heavy commands, exactly as today.
7. **Injection budgets unchanged.** None of the five adds bytes to the prefix. The `spend:` status line and lint output are operator-facing only.

## Interfaces

- `zirv ctx agent ... --mode read-only|writing --worktree`; `zirv agent` alias passes both through.
- `zirv context lint [--json] [--budget compact|standard|full] [--fix-plan]`; exit 1 on any CTX001/CTX005 error, 0 otherwise.
- `zirv ctx spend [--session <short>|--group <id>|--since <dur>] [--by harness|model|task-class|worker] [--json]`.
- `~/.zirv/prices.toml`: `[models."<name>"] input_micros, cache_write_micros, cache_read_micros, output_micros` per million tokens, `as_of = "YYYY-MM-DD"`.
- `FAKE_AGENT_PROMPT_LOG=<path>` in the fixture.

## Risks

- `NormalizedEvent` field additions ripple through exhaustive matches across the crate; Track C must compile the whole crate and keep every existing rot test green.
- `--worktree` runs `git worktree add`; on Windows path case-folding must match `permit`'s tree key. Test with a temp repo.
- The fixture change and adapter timestamp parsing need the Linux Docker suite.
- Merge order A -> B and C -> B and D -> E is required to avoid conflicting edits in `agent.rs`, `log.rs`, `telemetry.rs`, `compile.rs`.

## Verification

Per track: `cargo nextest run <modules> --no-fail-fast`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt -- --check` in the worktree, FOREGROUND. After merge: the full five gates on Windows with the failure-name diff against the baseline, then the Linux Docker full serial suite plus clippy, then `zirv workflow review run` on claude (sonnet) and codex.
