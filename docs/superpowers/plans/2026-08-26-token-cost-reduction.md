# Token-Cost Reduction Implementation Plan (issue #155)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a zirv-supervised session dramatically cheaper to run — fewer tokens per completed task, a higher provider cache-hit ratio, fewer redundant review rounds, bounded delegation — without lowering response quality, and with a measured before/after number for every phase.

**Architecture:** Six independent levers, one PR each. (1) `prompt.rs`/`compile.rs` stop invalidating their own provider prompt cache: one deduped memory layer moves to the tail behind the canonical context layer, and a truncated layer stops being silent. (2) `event.rs`/`telemetry.rs`/`log.rs` gain the raw four-category token shape and delegation lineage, so every later phase can be *measured* rather than asserted. (3) `context_cli.rs` stamps a canonical-content hash into the generated `CLAUDE.md`/`AGENTS.md`, and `compile.rs` skips injecting bytes the harness already read natively. (4) `prompt.rs`/`claude.rs`/`workflow/review.rs` collapse three stacked review mandates into one gate, re-review deltas instead of full diffs, and enforce the stop rule in code. (5) `prompt.rs`/`spawnreq.rs`/`agent.rs`/`sessions.rs`/`script_runner` gain sub-orchestrators, work groups, worker budgets and heavy-**operation** permits in place of workload-blind session counting. (6) `event.rs`/`rot.rs`/`pace.rs` make rotation thresholds ratios of the model's real context capacity, and let quota pressure gate *scheduling* — never rotation.

**Tech Stack:** Rust edition 2024, single binary `zirv`. `clap` (derive + `Args`/`Subcommand`), `serde`/`serde_json`, `toml`, `sha2`, `uuid`. Tests are inline `#[cfg(test)] mod tests`; the local loop is `cargo nextest run --no-fail-fast`.

**Spec:** `docs/superpowers/specs/2026-08-26-token-cost-reduction-design.md` — the authority. Read it before Task 1.1; every task below argues from one of its sections.

## Global Constraints

- **PRIMARY ACCEPTANCE CRITERION (issue #155):** tokens per completed task must fall measurably with response quality unchanged, and every phase lands with a before/after number. Where a choice trades measurability against a speculative saving, measurability wins — that is why Phase 2 (telemetry) precedes Phases 3–6.
- **Cheaper never means dumber.** A budget bounds *work*: it warns, checkpoints, and stops. It never silently downgrades a model, and zirv never restarts or compacts a session because of cost.
- Verify before claiming done — all five, in the FOREGROUND, never backgrounded: `cargo build`; `cargo nextest run --no-fail-fast`; `cargo test --verbose -- --test-threads=1`; `cargo fmt -- --check`; `cargo clippy --all-targets -- -D warnings`.
- Windows baseline: 7 `commands::ctx::wrap::tests` failures pre-exist on `main` on the dev machine. Judge by the sorted failure-**NAME** diff against `main`, never the count, and never against `git stash` (that diffs the branch's own HEAD). A `STATUS_ACCESS_VIOLATION` crash prints no `test result:` line and no `failures:` block, so a grep returns EMPTY and looks clean — confirm the `test result:` line exists before trusting any failure list.
- Anything touching `wrap`, `announce`, `pace`, or adapter argv needs a Linux/Docker pass (Phases 5 and 6): export with `git -c core.autocrlf=false archive HEAD` (plain `git archive` emits CRLF and corrupts `tests/fixtures/stub-tui.sh`), run on `rust:1-bookworm` as a NON-root user, `cargo test --bin zirv wrap:: -- --test-threads=1` plus `cargo clippy --all-targets -- -D warnings` (`#[cfg(unix)]` blocks never lint on Windows).
- `rot.rs` stays pure: no fs, clock, env, or net inside it. All I/O lives one layer up in `score.rs`. Phase 6 delivers capacity to it through `Capabilities`, which is already an input.
- `wrap` must never make a session worse: no `unwrap`/`expect` on its hot path, raw-mode restore in explicit arms (release profile is `panic = "abort"`), and any supervision failure degrades to pure passthrough.
- Repo-owned surfaces are UNTRUSTED and may only NARROW, never widen: `<repo>/.zirv/ctx.toml`, `system-prompt.md`, `context/*.md`, `memory/`, repo skills and checks. `REPO_FORBIDDEN` keys in `config.rs` hard-error from a repo layer; only `~/.zirv/ctx.toml`, `ZIRV_CTX_*`, or a flag may set them.
- Every new config key is optional (`#[serde(default)]` or `Option<T>`); an operator who writes no config gets the new defaults. Every key must also appear in `.zirv/ctx.toml` (active or commented) and every new `REPO_FORBIDDEN` entry must gain a row in BOTH `README.md` and `docs/obsidian/Concepts/Untrusted Configuration.md` — `config.rs` has tests that enforce exactly this.
- Old installed binaries hard-fail on unknown `.zirv` config keys. Install the new binary BEFORE writing config that uses new keys, and where a key is renamed (Phase 5) the old spelling must still PARSE, not merely be documented.
- Tests stay inline in `#[cfg(test)] mod tests`; `tests/fixtures/` is data only.
- Never assert an exact argv that depends on an installed-binary probe — assert the invariant.
- Never commit or push to `main`/`master`. Branch first, open a PR. No "Co-Authored-By" and no "Generated with Claude Code" lines in any commit message or PR body.
- Every phase's PR must raise `Cargo.toml`'s version above its base or CD fails on a duplicate release: 2.31.0 → 2.32.0 → 2.33.0 → 2.34.0 → 2.35.0 → 2.36.0, from a 2.30.1 base.
- Vault updates per the CLAUDE.md doc-update table are part of each phase, not a follow-up. Run the `vault-keeper` agent before pushing.
- Every substantive diff gets a codex cross-review round (`zirv agent codex "…"`); codex-cli is installed at `~/AppData/Local/Programs/OpenAI/Codex/bin` even when a roster line claims it is not.

## Phase → branch → version

| Phase | Branch | Base | `Cargo.toml` version |
| --- | --- | --- | --- |
| 1 | `feat/token-cost-p1-quick-wins` | `main` | `2.31.0` |
| 2 | `feat/token-cost-p2-telemetry` | Phase 1 branch (or `main` once merged) | `2.32.0` |
| 3 | `feat/token-cost-p3-context-dedupe` | Phase 2 branch | `2.33.0` |
| 4 | `feat/token-cost-p4-review-convergence` | Phase 3 branch | `2.34.0` |
| 5 | `feat/token-cost-p5-work-groups` | Phase 4 branch | `2.35.0` |
| 6 | `feat/token-cost-p6-model-aware` | Phase 5 branch | `2.36.0` |

## File Structure

| File | Responsibility after this plan |
| --- | --- |
| `src/commands/ctx/compile.rs` | One merged memory layer injected after the canonical context layer; loud truncation reporting; the Phase 3 native-dedupe skip. |
| `src/commands/ctx/prompt.rs` | `compose` no longer emits a memory layer (params dropped); `v8` shape; `PromptRole::SubOrchestrator`; the review-gate guard text in `HARNESS_PROMPT`. |
| `src/commands/ctx/context_cli.rs` | `render_generated` stamps `<!-- zirv:canonical-sha256:… -->`; `canonical_sha256` is the one hash function both sides use. |
| `src/commands/ctx/event.rs` | `TranscriptUsage` carries four raw token categories + `context_total()`; `Capabilities` carries `context_window_tokens`. |
| `src/commands/ctx/adapters/claude.rs` | Stops pre-summing usage; reports per-model context capacity; orchestrator prompt drops the "on top" review stacking. |
| `src/commands/ctx/adapters/codex.rs` | Fills the two new usage fields with `0` (no cache classes in its rollout totals); reports no capacity. |
| `src/commands/ctx/log.rs` | New `Delegation` record + `delegations.jsonl`; new decision actions `context-truncated`, `context-dedup-skip`, `delegation-complete`. |
| `src/commands/ctx/agent.rs` | `--group`, `--budget-tokens`, `--max-tool-calls`; writes the delegation checkpoint; quota-pressure spawn gate. |
| `src/commands/ctx/group.rs` (new) | `WorkGroup` persistence and `zirv ctx group create|status|close`. |
| `src/commands/ctx/dash/spawnreq.rs` | `SpawnRequest` gains `role`/`parent_session`/`work_group_id`. |
| `src/commands/ctx/dash/mod.rs` | Depth-cap refusal at spawn; permit-based heavy gate; quota-pressure spawn gate. |
| `src/commands/ctx/sessions.rs` | Heavy-**operation** permit accounting replaces `count_heavy_workers*`. |
| `src/commands/ctx/permit.rs` (new) | The machine-wide heavy-operation permit: classification, acquire/release, RAII guard. |
| `src/script_runner/command.rs` | Holds a heavy-operation permit across a classified heavy child's lifetime. |
| `src/commands/ctx/rot.rs` | Pure `token_gates(cfg, caps)`; `verdict_for`/`score_from` take `Capabilities`. |
| `src/commands/ctx/pace.rs` | Pure `spawn_gate(...)` over the same windows `decide` reads. |
| `src/commands/ctx/usage.rs` | `zirv ctx usage --sessions`. |
| `src/commands/ctx/status.rs` | Group tree with per-child raw token spend; permit occupancy line. |
| `src/commands/workflow/telemetry.rs` | Four raw categories + `sidechain_*` bucket + session lineage on `TelemetryEvent`. |
| `src/commands/workflow/review.rs` | Delta re-review from the last reviewed sha; stop-on-no-new-findings enforced in code. |
| `src/commands/ctx/config.rs` | New keys and their `REPO_FORBIDDEN`/`ALL_CONFIG_KEYS` rows; the `max_heavy_workers` → `max_heavy_operations` alias rewrite. |
| `.zirv/context/common.md` | Tightened below 4096 bytes so nothing is truncated. |
| `.zirv/ctx.toml`, `README.md`, `docs/obsidian/**`, `docs/benchmarks/token-cost.md` | Sample config, trust tables, vault pages, and the measurement procedure. |

Decomposition rationale: Phase 1 is behaviour-visible but tiny, so it validates the layer-order tests everything later depends on. Phase 2 is P0 measurement and deliberately changes no session behaviour, so a reviewer judges it on shape alone. Phases 3–6 each carry one lever and can be reverted independently; within a phase, each task is the smallest unit with its own test cycle worth a review gate.

---

# Phase 1 — quick wins (PR 1, version `2.31.0`, branch `feat/token-cost-p1-quick-wins`)

Spec section: "Phase 1 — quick wins". Three cheap, high-confidence changes plus the phase's closing gates.

### Task 1.1: A truncated context layer is never silent

Today `compile::read_context_layer` truncates to `cfg.context.max_common_bytes` / `max_harness_bytes` and returns a `truncated: bool` that reaches only `ContextProvenance::truncated` — a field `zirv context status` renders and nothing else looks at. This repo's own `.zirv/context/common.md` is 4917 bytes against a 4096-byte cap, so 821 bytes have been silently cut mid-word from every session ever launched here.

**Files:**
- Modify: `src/commands/ctx/compile.rs` (`ContextProvenance` at ~68, `with_canonical_context_layer` at ~236, `compile` at ~329, `compile_with_harness_roster` at ~371)
- Modify call sites (one new argument each): `src/commands/ctx/chat.rs:139`, `src/commands/ctx/chat.rs:512`, `src/commands/ctx/exec.rs:375`, `src/commands/ctx/exec.rs:1094`, `src/commands/ctx/run_loop.rs:194`, `src/commands/ctx/wrap.rs:1614`, `src/commands/ctx/resume.rs:113`, `src/commands/ctx/dash/mod.rs:2462`, `src/commands/ctx/context_status.rs:657`
- Modify test call sites (same new argument): `src/commands/ctx/exec.rs:1801`, `src/commands/ctx/run_loop.rs:774`, `src/commands/ctx/wrap.rs:6979`, `src/commands/ctx/resume.rs:696`, and every `compile(`/`compile_with_harness_roster(` call in `compile.rs`'s own `mod tests`
- Test: inline `#[cfg(test)] mod tests` in `src/commands/ctx/compile.rs`

**Interfaces:**
- Consumes: `log::append`, `log::Decision`, `log::tail` (`src/commands/ctx/log.rs`, all existing); `surface::ContextSurface::path()`.
- Produces:
  - `compile::ContextProvenance` gains `pub budget_key: &'static str`
  - `pub fn compile(home: Option<&Path>, repo: &Path, simple: bool, cfg: &CtxConfig, adapter: &dyn AgentAdapter, role: PromptRole, state: &StateDir, now: u64, mode: super::adapters::LaunchMode, log_truncation: bool) -> CompiledContext`
  - `pub fn compile_with_harness_roster(home: Option<&Path>, repo: &Path, simple: bool, cfg: &CtxConfig, adapter: &dyn AgentAdapter, role: PromptRole, state: &StateDir, now: u64, include_harness_roster: bool, mode: super::adapters::LaunchMode, log_truncation: bool) -> CompiledContext`
  - `pub const compile::TRUNCATED_ACTION: &str = "context-truncated";`

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/commands/ctx/compile.rs`:

```rust
    /// Issue #155, Phase 1(a): the single most expensive failure mode of a
    /// byte budget is one nobody is told about. A cut canonical layer must
    /// produce BOTH a decision-log entry naming the file and the exact lost
    /// byte count, AND a stderr note at compose time. Before this, the only
    /// evidence was `ContextProvenance::truncated`, which nothing but
    /// `zirv context status` ever reads.
    #[test]
    fn a_truncated_canonical_layer_is_logged_with_the_file_and_the_lost_bytes() {
        let repo = repo_with_context_files(&[("common.md", &"x".repeat(6000))]);
        let home = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(home.path().join("state"));
        let mut cfg = CtxConfig::default();
        cfg.context.max_common_bytes = 4096;

        let compiled = compile(
            Some(home.path()),
            repo.path(),
            false,
            &cfg,
            &ClaudeAdapter::new(None),
            PromptRole::Orchestrator,
            &state,
            now_secs(),
            LaunchMode::Interactive,
            true,
        );

        let cut = compiled
            .provenance
            .iter()
            .find(|p| p.truncated)
            .expect("the 6000-byte common layer must report as truncated");
        assert_eq!(cut.raw_bytes, 6000);
        assert_eq!(cut.delivered_bytes, 4096);
        assert_eq!(cut.budget_key, "context.max_common_bytes");

        let lines = crate::commands::ctx::log::tail(&state, 20).expect("decision log");
        let entry = lines
            .iter()
            .find(|line| line.contains("context-truncated"))
            .expect("a context-truncated decision must be written");
        assert!(entry.contains("common.md"), "got {entry}");
        assert!(entry.contains("1904"), "must name the LOST bytes: {entry}");
        assert!(entry.contains("context.max_common_bytes"), "got {entry}");
    }

    /// The other direction: a layer inside its budget writes nothing at all.
    /// A truncation warning that fires on healthy sessions is noise, and
    /// noise is how the real one gets ignored.
    #[test]
    fn a_layer_inside_its_budget_writes_no_truncation_decision() {
        let repo = repo_with_context_files(&[("common.md", "short and well within budget\n")]);
        let home = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(home.path().join("state"));

        let compiled = compile(
            Some(home.path()),
            repo.path(),
            false,
            &CtxConfig::default(),
            &ClaudeAdapter::new(None),
            PromptRole::Orchestrator,
            &state,
            now_secs(),
            LaunchMode::Interactive,
            true,
        );

        assert!(compiled.provenance.iter().all(|p| !p.truncated));
        let lines = crate::commands::ctx::log::tail(&state, 20).unwrap_or_default();
        assert!(
            !lines.iter().any(|line| line.contains("context-truncated")),
            "no decision may be written for an untruncated layer: {lines:?}"
        );
    }

    /// `zirv context status` compiles once per registered adapter purely to
    /// REPORT truncation. It must not also WRITE decisions doing so, or every
    /// status invocation would spam the log with entries describing a session
    /// that never launched. That is exactly what the explicit
    /// `log_truncation` parameter exists to force each call site to answer.
    #[test]
    fn a_read_only_report_compile_writes_no_truncation_decision() {
        let repo = repo_with_context_files(&[("common.md", &"x".repeat(6000))]);
        let home = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(home.path().join("state"));
        let mut cfg = CtxConfig::default();
        cfg.context.max_common_bytes = 4096;

        let compiled = compile(
            Some(home.path()),
            repo.path(),
            false,
            &cfg,
            &ClaudeAdapter::new(None),
            PromptRole::Orchestrator,
            &state,
            now_secs(),
            LaunchMode::Interactive,
            false,
        );

        assert!(
            compiled.provenance.iter().any(|p| p.truncated),
            "the report still SEES the truncation"
        );
        let lines = crate::commands::ctx::log::tail(&state, 20).unwrap_or_default();
        assert!(
            !lines.iter().any(|line| line.contains("context-truncated")),
            "a report must not write decisions: {lines:?}"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin zirv compile::tests::a_truncated_canonical_layer -- --test-threads=1`
Expected: FAIL to compile — `this function takes 9 arguments but 10 arguments were supplied`, and `no field budget_key on type ContextProvenance`.

- [ ] **Step 3: Write minimal implementation**

In `src/commands/ctx/compile.rs`, add the field to `ContextProvenance` (after `truncated`):

```rust
    /// Which configured budget cut this surface -- the exact `ctx.toml` key
    /// an operator has to raise. Carried as data rather than re-derived from
    /// the path at each reader, so the decision-log line, the stderr note and
    /// `zirv context status` can never name three different keys for one cut.
    pub budget_key: &'static str,
```

Populate it in `with_canonical_context_layer`'s candidate list by pairing each cap with its key. Change the candidate tuple to carry the key:

```rust
    let mut candidates: Vec<(context::PrecedenceTier, Layer, PathBuf, usize, &'static str)> = vec![(
        context::PrecedenceTier::CanonicalCommon,
        Layer::ContextCommon,
        context::common_path(repo),
        cfg.context.max_common_bytes,
        "context.max_common_bytes",
    )];
    if let Some((layer, path)) = harness_context_layer(adapter_name, repo) {
        candidates.push((
            context::PrecedenceTier::CanonicalHarnessSpecific,
            layer,
            path,
            cfg.context.max_harness_bytes,
            "context.max_harness_bytes",
        ));
    }
```

and in the loop, destructure `(_, layer, path, cap, budget_key)`, set `budget_key` on the pushed `ContextProvenance`, and emit the stderr note immediately after building it:

```rust
        if truncated {
            // Compose-time, unconditional: this is the operator-visible half
            // and it costs nothing when nothing was cut. The decision-log
            // half is gated per call site (`log_truncation`) because a
            // read-only report compiles too.
            eprintln!(
                "zirv: canonical context layer {} was truncated -- {} of {raw_bytes} bytes \
                 delivered, {} bytes LOST to {budget_key}. Shorten the file or raise the key \
                 in ~/.zirv/ctx.toml.",
                path.display(),
                delivered_bytes,
                raw_bytes.saturating_sub(delivered_bytes),
            );
        }
```

(`path` is moved into `optimize::Surface` below it, so capture `let display_path = path.display().to_string();` before the move and use that.)

Add the action constant and the log helper near the top of the module:

```rust
/// `log::Decision::action` for a canonical context layer cut by its budget.
pub const TRUNCATED_ACTION: &str = "context-truncated";

/// The decision-log half of the truncation report. Session-free on purpose:
/// `compile` runs before most launch paths have minted a session id (see
/// `run_loop.rs`, which mints one AFTER composing), and the surface path in
/// `detail` is the identity that actually matters here. `verb` is
/// `"compile"` for the same reason.
fn log_truncation_decisions(state: &StateDir, now: u64, provenance: &[ContextProvenance]) {
    for entry in provenance.iter().filter(|p| p.truncated) {
        let detail = format!(
            "{}: {} of {} bytes delivered, {} lost to {}",
            entry.surface.path().display(),
            entry.delivered_bytes,
            entry.raw_bytes,
            entry.raw_bytes.saturating_sub(entry.delivered_bytes),
            entry.budget_key,
        );
        let _ = super::log::append(
            state,
            &super::log::Decision {
                ts: now,
                session: "",
                verb: "compile",
                verdict: "n/a",
                score: 0,
                action: TRUNCATED_ACTION,
                detail: &detail,
            },
        );
    }
}
```

Add `log_truncation: bool` as the final parameter of both `compile` and `compile_with_harness_roster`; `compile` forwards it. In `compile_with_harness_roster`, immediately after `let (composed, provenance) = with_canonical_context_layer(...)`:

```rust
    if log_truncation {
        log_truncation_decisions(state, now, &provenance);
    }
```

Now the call sites. Real launches pass `true`; the two that are not a launch pass `false`:

```rust
// src/commands/ctx/chat.rs:139 -- a task-prompt FALLBACK builder, not the
// launch itself; chat.rs:512 below is the real launch and would double-log.
        super::adapters::LaunchMode::Interactive,
        false,
    );

// src/commands/ctx/chat.rs:512 -- the real interactive launch.
        super::adapters::LaunchMode::Interactive,
        true,
    );

// src/commands/ctx/exec.rs:375, src/commands/ctx/exec.rs:1094,
// src/commands/ctx/run_loop.rs:194, src/commands/ctx/wrap.rs:1614,
// src/commands/ctx/resume.rs:113, src/commands/ctx/dash/mod.rs:2462
// -- all real launches: pass `true` as the new final argument.

// src/commands/ctx/context_status.rs:657 -- `zirv context status` compiles
// once per registered adapter purely to render truncation in its own report.
// A status command must not write decisions.
        super::adapters::LaunchMode::Interactive,
        false,
    );
```

Every existing test call site in `exec.rs`, `run_loop.rs`, `wrap.rs`, `resume.rs` and `compile.rs`'s own `mod tests` gains `false` as the final argument, preserving each test's current assertions exactly.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin zirv compile:: context_status:: -- --test-threads=1`
Expected: PASS. Then `cargo build` to confirm every call site compiles.

- [ ] **Step 5: Commit**

```bash
git checkout -b feat/token-cost-p1-quick-wins
git add src/commands/ctx/compile.rs src/commands/ctx/chat.rs src/commands/ctx/exec.rs src/commands/ctx/run_loop.rs src/commands/ctx/wrap.rs src/commands/ctx/resume.rs src/commands/ctx/dash/mod.rs src/commands/ctx/context_status.rs
git commit -m "feat(ctx): report a truncated canonical context layer instead of losing it silently"
```

---

### Task 1.2: Fit this repository's `common.md` inside its own budget

With Task 1.1 in place this repo now prints a truncation warning on every single launch. The fix is editorial, not structural: `.zirv/context/common.md` is 4917 bytes against a 4096-byte cap, and the 821 lost bytes are the `## Git` section (never commit to `main`, bump `Cargo.toml`, no Co-Authored-By lines) plus most of the doc-update table — i.e. exactly the instructions worth keeping.

**Files:**
- Modify: `.zirv/context/common.md`
- Test: inline `#[cfg(test)] mod tests` in `src/commands/ctx/compile.rs`

**Interfaces:**
- Consumes: `ContextConfig::max_common_bytes` (default 4096), `compile::ContextProvenance` (Task 1.1).
- Produces: no code interface — a repository content change plus one pinning test.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/commands/ctx/compile.rs`:

```rust
    /// Issue #155, Phase 1(b): this repository's own canonical context must
    /// fit the budget zirv ships. Pinned as a test rather than fixed once,
    /// because the file grows with every session that edits it and a silent
    /// re-truncation is exactly the failure Task 1.1 exists to surface.
    /// `CARGO_MANIFEST_DIR` is the real repo, the same seam
    /// `config.rs::the_repo_ctx_toml_parses_and_stays_exhaustive` uses.
    #[test]
    fn this_repositorys_canonical_common_context_fits_the_shipped_budget() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let path = crate::commands::ctx::context::common_path(repo);
        let text = std::fs::read_to_string(&path).expect("read .zirv/context/common.md");
        let cap = CtxConfig::default().context.max_common_bytes;
        assert!(
            text.len() <= cap,
            "{} is {} bytes, over the shipped {cap}-byte context.max_common_bytes budget; \
             tighten it rather than raising the cap",
            path.display(),
            text.len()
        );
    }

    /// The harness-specific halves are inside their own independent budget
    /// too -- they are truncated separately, so a passing common.md says
    /// nothing about them.
    #[test]
    fn this_repositorys_canonical_harness_context_files_fit_the_shipped_budget() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let cap = CtxConfig::default().context.max_harness_bytes;
        for path in [
            crate::commands::ctx::context::claude_path(repo),
            crate::commands::ctx::context::codex_path(repo),
        ] {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            assert!(
                text.len() <= cap,
                "{} is {} bytes, over the shipped {cap}-byte context.max_harness_bytes budget",
                path.display(),
                text.len()
            );
        }
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin zirv compile::tests::this_repositorys_canonical -- --test-threads=1`
Expected: FAIL — `.zirv/context/common.md is 4917 bytes, over the shipped 4096-byte context.max_common_bytes budget`.

- [ ] **Step 3: Write minimal implementation**

Tighten `.zirv/context/common.md` below 4096 bytes **without losing meaning**. Every one of these sections must still be present and still say the same thing: the two-halves summary, the five verify commands, the nextest `--no-fail-fast` rationale, the module map, the conventions list (edition 2024, reserved built-ins, `rot.rs` purity, `wrap` never-worse, repo-untrusted narrowing, inline tests), the mandatory vault-update contract with its routing table, and the Git rules.

Concrete tightening levers, in the order to apply them:

1. Collapse the nextest paragraph (currently ~9 lines) to two sentences: nextest isolates each test in its own process so `env::set_var` races cannot happen, and `--no-fail-fast` is required because a fail-fast run cannot produce the complete sorted failure-NAME list a baseline is diffed against.
2. Compress the module map's five bullets into three, keeping every path token — the value is the paths, not the prose around them.
3. Turn the doc-update routing table from prose bullets into a compact `surface -> page` list with no repeated framing words.
4. Drop restatements that already exist verbatim in `.zirv/context/claude.md` (the Windows-specific and orchestrator-policy material). Do NOT drop anything that exists only here.

Verify with `wc -c .zirv/context/common.md` — the number must be at or below 4096 with headroom (target ≤ 3900 so the next edit does not immediately re-break it).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin zirv compile:: -- --test-threads=1`
Expected: PASS. Then run `zirv ctx status` (or any launch) and confirm no `zirv: canonical context layer … was truncated` line appears on stderr.

- [ ] **Step 5: Commit**

```bash
git add .zirv/context/common.md src/commands/ctx/compile.rs
git commit -m "docs(context): tighten canonical common.md inside the 4096-byte budget"
```

---

### Task 1.3: One deduped memory layer, at the cache-safe tail (`v7` → `v8`)

`compose` emits a memory layer inline, and `compile_with_harness_roster` appends a SECOND one (`prompt::with_memory_layer` at ~405) for the retrieval selection — so `describe()` renders `memory` twice, and the git-derived retrieval layer sits AHEAD of the canonical context layer. Retrieval is selected from live `git diff`/`git ls-files` output (`compile::changed_repo_paths`) and is recomputed on every recompose, so during active editing it invalidates the provider prompt cache for everything after it: the canonical context layer (up to 8 KiB) plus mail.

**Files:**
- Modify: `src/commands/ctx/prompt.rs` (`DEFAULT_PROMPT_VERSION` at ~42, `compose` at ~714)
- Modify: `src/commands/ctx/compile.rs` (`compile_with_harness_roster` at ~371)
- Test: inline `#[cfg(test)] mod tests` in `src/commands/ctx/compile.rs` and `src/commands/ctx/prompt.rs`

**Interfaces:**
- Consumes: `prompt::with_memory_layer(composed, entries, cap)`, `prompt::MemoryLine`, `prompt::memory_injection_summary`, `prompt::PromptSource::{Memory, Context}` (all existing).
- Produces:
  - `pub const prompt::DEFAULT_PROMPT_VERSION: &str = "v8";`
  - `pub fn prompt::compose(home: Option<&Path>, repo: &Path, simple: bool, cfg: &PromptConfig, role: PromptRole, harness_lines: &[String], harness_roster_cap: usize) -> Option<ComposedPrompt>` — the `memory: &[MemoryLine]` and `memory_cap: usize` parameters are REMOVED
  - `pub(crate) fn compile::merge_memory_layers(core: &[prompt::MemoryLine], retrieved: &[prompt::MemoryLine]) -> Vec<prompt::MemoryLine>`

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/commands/ctx/compile.rs`:

```rust
    /// Issue #155, Phase 1(c)+(d): ONE memory layer, and it sits AFTER the
    /// canonical context layer.
    ///
    /// The retrieval half of memory is selected from live `git diff`/`git
    /// ls-files` output and is recomputed on every recompose (a nudge
    /// relaunch, a loop cycle, a dashboard sweep). Everything positioned
    /// after it therefore falls out of the provider's prompt cache whenever
    /// the working tree moves. Putting the whole memory layer at the tail --
    /// as late as it can go while still preceding mail -- keeps the ~8 KiB
    /// canonical context layer in the cacheable prefix.
    #[test]
    fn memory_is_one_layer_and_follows_the_canonical_context_layer() {
        let repo = repo_with_context_files(&[("common.md", "canonical common instructions\n")]);
        let home = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(home.path().join("state"));
        let cfg = CtxConfig::default();
        let slug = super::super::state::repo_slug(repo.path());
        memory::upsert_scoped(
            memory::MemoryScope::Private,
            repo.path(),
            &state,
            &slug,
            &cfg,
            &memory::Entry {
                key: "deploy-cmd".to_string(),
                written_by: "test".to_string(),
                written: 100,
                verified: 100,
                source: "explicit".to_string(),
                body: "zirv deploy".to_string(),
                importance: None,
                confidence: None,
                tags: Vec::new(),
                paths: Vec::new(),
            },
        )
        .expect("remember");

        let compiled = compile(
            Some(home.path()),
            repo.path(),
            false,
            &cfg,
            &ClaudeAdapter::new(None),
            PromptRole::Orchestrator,
            &state,
            now_secs(),
            LaunchMode::Interactive,
            false,
        );
        let composed = compiled.composed.expect("composed");

        let memory_positions: Vec<usize> = composed
            .sources
            .iter()
            .enumerate()
            .filter(|(_, s)| **s == PromptSource::Memory)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            memory_positions.len(),
            1,
            "exactly one memory layer, not two: {:?}",
            composed.sources
        );
        let context_position = composed
            .sources
            .iter()
            .position(|s| *s == PromptSource::Context)
            .expect("a canonical context layer");
        assert!(
            context_position < memory_positions[0],
            "canonical context must precede memory: {:?}",
            composed.sources
        );

        let described = composed.describe();
        assert!(described.starts_with("v8 "), "got {described}");
        assert_eq!(
            described.matches("memory").count(),
            1,
            "describe() listed memory twice: {described}"
        );
    }

    /// Core and retrieval selections still report SEPARATELY -- `zirv context
    /// status` shows where an entry came from. Only the injection is unified.
    #[test]
    fn the_merged_injection_does_not_collapse_the_two_reported_selections() {
        let core = vec![prompt::MemoryLine {
            key: "Deploy-Cmd".to_string(),
            body: "zirv deploy".to_string(),
            verified: 100,
            written: 100,
            shared: false,
        }];
        let retrieved = vec![
            // Same key, different case: the merge must drop it, because
            // `gather_memory` already excluded it from retrieval by key and a
            // second copy in the prompt would say the same thing twice.
            prompt::MemoryLine {
                key: "deploy-cmd".to_string(),
                body: "zirv deploy".to_string(),
                verified: 90,
                written: 90,
                shared: false,
            },
            prompt::MemoryLine {
                key: "lint-cmd".to_string(),
                body: "cargo clippy".to_string(),
                verified: 80,
                written: 80,
                shared: false,
            },
        ];

        let merged = merge_memory_layers(&core, &retrieved);
        assert_eq!(
            merged.iter().map(|e| e.key.as_str()).collect::<Vec<_>>(),
            vec!["Deploy-Cmd", "lint-cmd"],
            "core order first, retrieval appended, deduped case-insensitively"
        );
    }

    /// A shared entry and a private entry may legitimately carry the same
    /// key: `select_memory_within_cap` resolves that conflict itself, with
    /// private structurally outranking shared. The merge must not pre-empt
    /// that by dropping one on key alone -- the dedupe key is (shared, key).
    #[test]
    fn merging_keys_on_scope_too_so_the_shared_suppression_rule_still_runs() {
        let core = vec![prompt::MemoryLine {
            key: "deploy-cmd".to_string(),
            body: "private".to_string(),
            verified: 100,
            written: 100,
            shared: false,
        }];
        let retrieved = vec![prompt::MemoryLine {
            key: "deploy-cmd".to_string(),
            body: "shared".to_string(),
            verified: 90,
            written: 90,
            shared: true,
        }];
        assert_eq!(merge_memory_layers(&core, &retrieved).len(), 2);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin zirv compile::tests::memory_is_one_layer -- --test-threads=1`
Expected: FAIL to compile — `cannot find function merge_memory_layers in this scope`; once that resolves, FAIL on `exactly one memory layer, not two`.

- [ ] **Step 3: Write minimal implementation**

In `src/commands/ctx/prompt.rs`:

```rust
/// v8 (issue #155, 2026-08-26): memory became ONE layer instead of two, and
/// moved to the tail -- after the canonical `.zirv/context/` layer, before
/// mail. `compose` no longer emits it at all; `compile.rs` owns the single
/// injection, because it is the only place that has both selections in hand.
/// The retrieval half is derived from live `git diff`/`git ls-files` output
/// and changes whenever the working tree does, so anything positioned after
/// it falls out of the provider's prompt cache. Everything cacheable now
/// precedes it.
pub const DEFAULT_PROMPT_VERSION: &str = "v8";
```

Delete the `memory: &[MemoryLine]` and `memory_cap: usize` parameters from `compose`, and delete the `with_memory_layer` call inside its body (the one that runs right after the harness block). `PromptSource::Memory`'s doc comment must be updated: it no longer "sits after the harness layer and before the user layer" — it sits last before `Mail`, and is added by `compile.rs`, the same "a caller adds this layer, but it still gets a `PromptSource` variant so `describe()` can name it" shape `Context`, `Mail` and `ReportBack` already have.

In `src/commands/ctx/compile.rs`, add the merge:

```rust
/// The single memory list injected into a composed prompt: the core
/// selection in its own order, then any retrieval entry not already present.
///
/// Deduped on `(shared, key.to_lowercase())`, not on `key` alone: a private
/// and a shared entry may legitimately carry the same key, and resolving
/// that conflict is `prompt::select_memory_within_cap`'s job (private
/// structurally outranks shared there). Case-insensitive because the private
/// scope never validates or normalizes a key's case, the same reasoning
/// `select_memory_within_cap`'s own key-conflict suppression already states.
///
/// `gather_memory` already filters retrieval against the core keys, so this
/// is belt-and-braces for that path -- and load-bearing for any future
/// caller that assembles the two lists differently.
pub(crate) fn merge_memory_layers(
    core: &[prompt::MemoryLine],
    retrieved: &[prompt::MemoryLine],
) -> Vec<prompt::MemoryLine> {
    let mut seen: std::collections::HashSet<(bool, String)> = core
        .iter()
        .map(|entry| (entry.shared, entry.key.to_lowercase()))
        .collect();
    let mut merged = core.to_vec();
    for entry in retrieved {
        if seen.insert((entry.shared, entry.key.to_lowercase())) {
            merged.push(entry.clone());
        }
    }
    merged
}
```

In `compile_with_harness_roster`: drop the two memory arguments from the `prompt::compose` call, DELETE the `let composed = prompt::with_memory_layer(composed, &retrieved_memory, cfg.memory.retrieval_max_bytes);` line at ~405, and add the single injection AFTER `with_canonical_context_layer` and after the truncation logging from Task 1.1:

```rust
    let (composed, provenance) =
        with_canonical_context_layer(composed, adapter.name(), repo, home, cfg);
    if log_truncation {
        log_truncation_decisions(state, now, &provenance);
    }
    // Issue #155: the one memory layer, injected last of everything zirv
    // composes deterministically -- mail and the command-line layer are the
    // only things after it, and both are already per-launch. The cap is the
    // sum of the two configured budgets, so neither selection can crowd the
    // other out of the space it was already allotted.
    let composed = prompt::with_memory_layer(
        composed,
        &merge_memory_layers(&memory_entries, &retrieved_memory),
        cfg.memory
            .core_max_bytes
            .saturating_add(cfg.memory.retrieval_max_bytes),
    );
```

`core_memory` and `retrieved_memory` in `CompiledContext` keep being computed exactly as today (`prompt::memory_injection_summary` over each selection separately) — the reporting split is unchanged.

Finally, `prompt.rs`'s own `mod tests`: every `compose(` call drops its 6th and 7th arguments (`&memory`, `memory_cap`). Any test that asserted the memory layer's presence or position *inside* `compose`'s output moves to `compile.rs`'s `mod tests` (where the layer now lives) or calls `with_memory_layer` directly — `with_memory_layer`'s own unit tests are unaffected and must keep passing byte-for-byte.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin zirv prompt:: compile:: context_status:: -- --test-threads=1`
Expected: PASS. Grep the tree for any remaining `"v7"` literal (`rg '"v7"' src/`) and update it — a decision-log assertion pinning the old shape is a real failure, not noise.

- [ ] **Step 5: Commit**

```bash
git add src/commands/ctx/prompt.rs src/commands/ctx/compile.rs
git commit -m "perf(ctx): compose one deduped memory layer at the cacheable tail (v8)"
```

---

### Task 1.4: Version bump and vault updates for Phase 1

**Files:**
- Modify: `Cargo.toml` (version), `Cargo.lock` (regenerated by `cargo build`)
- Modify: `docs/obsidian/Modules/Ctx Subsystem.md`, `docs/obsidian/Development/Decision Log.md`, `docs/obsidian/Development/Work Journal.md`, `docs/obsidian/Development/Active Work.md`
- Test: `src/commands/version.rs` (if it pins a version literal — check with `rg '2\.30\.1' src/`)

**Interfaces:**
- Consumes: Tasks 1.1–1.3.
- Produces: `Cargo.toml` version `2.31.0`.

- [ ] **Step 1: Bump the version**

```toml
version = "2.31.0"
```

Then `cargo build` so `Cargo.lock` picks it up, and `rg '2\.30\.1' src/ docs/` to catch any pinned literal.

- [ ] **Step 2: Update the vault**

Per the CLAUDE.md doc-update table, Phase 1 changes prompt-layer behaviour and a contract, so these pages must change (and each gets its `last-verified` bumped):

- `Modules/Ctx Subsystem.md` — the composed-prompt layer order becomes `v8`; memory is one layer at the tail after canonical context; a truncated canonical layer now emits a `context-truncated` decision plus a stderr note; `compile`/`compile_with_harness_roster` gained `log_truncation`.
- `Development/Decision Log.md` — one entry: why memory moved to the tail (git-derived retrieval invalidates the provider cache for every layer after it), why the two layers merged (`describe()` double-counted, and two budgets fragmented one concept), and why truncation logging is per-call-site rather than unconditional (a read-only report compiles too).
- `Development/Work Journal.md` — the session entry for Phase 1.
- `Development/Active Work.md` — Phase 1 in "Recently Completed" with next-session context: Phase 2 (telemetry) is next and is P0 for measuring Phases 3–6.

Run the `vault-keeper` agent to check the contract rather than hand-checking it.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock docs/obsidian
git commit -m "chore: bump to 2.31.0 and record the phase 1 prompt-shape change"
```

---

### Task 1.5: Phase 1 verification gates, cross-review, and PR

**Files:** none modified by default — this task runs the gates and fixes whatever they surface.

**Interfaces:**
- Consumes: Tasks 1.1–1.4.
- Produces: a green gate run and an open PR against `main`.

- [ ] **Step 1: Capture the `main` baseline FIRST**

```bash
git switch main
cargo nextest run --no-fail-fast 2>&1 | tee /tmp/main-baseline.txt
git switch feat/token-cost-p1-quick-wins
```

Confirm the output contains a `test result:` line before trusting it — a `STATUS_ACCESS_VIOLATION` crash prints none and a failure grep then returns EMPTY, which looks identical to a clean run. Extract the sorted failure NAMES; expect roughly 7, all in `commands::ctx::wrap::tests`. **Never** use `git stash` as the baseline.

- [ ] **Step 2: Run all five gates in the FOREGROUND**

```bash
cargo build
cargo nextest run --no-fail-fast
cargo test --verbose -- --test-threads=1
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
```

Diff the sorted failure-NAME list against the `main` baseline. Any name not in the baseline is a regression this branch owns. Never judge by count.

- [ ] **Step 3: Measure the phase**

Record the before/after numbers this phase is judged on (spec, "Measurement"):

- `wc -c .zirv/context/common.md` before and after — bytes of canonical context no longer lost to truncation (expected: 821 → 0).
- The byte offset of the first git-derived layer in the composed prompt, before and after — i.e. how many bytes of prompt moved from "behind a churny layer" into the cacheable prefix (expected: the whole canonical context layer, ~4 KiB here).
- `zirv ctx usage` cache figures are NOT yet available; Phase 2 delivers them. State that explicitly in the PR body rather than estimating.

- [ ] **Step 4: Codex cross-review**

```bash
zirv agent codex "Review the diff on branch feat/token-cost-p1-quick-wins against main. Focus on: (1) whether moving the memory layer after the canonical context layer changes what a session is TOLD, not just where it is told it; (2) whether merge_memory_layers can drop an entry select_memory_within_cap would have kept; (3) whether any compile() call site got the wrong log_truncation value. Reply with confirmed, concrete findings only." -- --model gpt-5-codex
```

Triage what comes back, fix what is real, and re-review only what the fixes touched. Stop as soon as a round yields no new confirmed findings.

- [ ] **Step 5: Open the PR**

```bash
git push -u origin feat/token-cost-p1-quick-wins
gh pr create --base main --title "perf(ctx): phase 1 token-cost quick wins (#155)" --body-file <path>
```

The body states: the three changes, the measured byte numbers from Step 3, the `main` failure-NAME baseline used, and that Phase 2 delivers the cache-ratio measurement this phase's real payoff will be judged by. No "Co-Authored-By", no "Generated with Claude Code".

If `gh pr edit` is needed later and fails on a Projects-classic GraphQL error, PATCH via `gh api repos/../pulls/<n> -F body=@file` and verify.

**Phase 1 acceptance criteria (all measurable):**
- `.zirv/context/common.md` ≤ 4096 bytes; 0 bytes of canonical context lost per session (was 821).
- `ComposedPrompt::describe()` contains `"memory"` exactly once and starts with `"v8 "`.
- `PromptSource::Context` precedes `PromptSource::Memory` in `compiled.composed.sources` on every launch path.
- A truncated layer produces exactly one `context-truncated` decision naming the file, the delivered bytes, the lost bytes and the budget key; an untruncated layer produces none; a `zirv context status` compile produces none.

---

# Phase 2 — raw lineage telemetry (PR 2, version `2.32.0`, branch `feat/token-cost-p2-telemetry`)

Spec section: "Phase 2 — raw lineage telemetry". **P0 for measurement.** This phase changes no session behaviour at all: it changes what zirv can *say* about one. Phases 3–6 are judged by numbers only this phase can produce, so it lands before them.

### Task 2.1: `TranscriptUsage` carries four raw token categories

`adapters::claude::transcript_usage` folds `input_tokens + cache_creation_input_tokens + cache_read_input_tokens` into `input_tokens` via `context_tokens_of` before anything downstream ever sees them. Cache creation (expensive, written once) and cache read (cheap, the dominant class in a healthy session) become indistinguishable, so no cache-hit ratio is computable anywhere in the binary.

**Files:**
- Modify: `src/commands/ctx/event.rs` (`TranscriptUsage` at ~45)
- Modify: `src/commands/ctx/adapters/claude.rs` (`context_tokens_of` at ~108, `transcript_usage` at ~206)
- Modify: `src/commands/ctx/adapters/codex.rs` (`transcript_usage` at ~1159)
- Modify: `src/commands/workflow/engine.rs` (`cumulative_snapshot` at ~549, `usage_since` at ~583, `enrich_transition_evidence` at ~604)
- Test: inline `#[cfg(test)] mod tests` in `src/commands/ctx/event.rs`, `src/commands/ctx/adapters/claude.rs`, `src/commands/ctx/adapters/codex.rs`

**Interfaces:**
- Consumes: `serde_json::Value`, `window::parse_rollout_record` / `RolloutTokenTotals` (existing).
- Produces:
  - `event::TranscriptUsage { input_tokens: u64, cache_creation_input_tokens: u64, cache_read_input_tokens: u64, output_tokens: u64 }`, still `Debug + Clone + Copy + Default + PartialEq + Eq`
  - `pub fn event::TranscriptUsage::context_total(&self) -> u64`
  - `pub fn adapters::claude::usage_categories(usage: &serde_json::Value) -> TranscriptUsage` (one row's four raw numbers)
  - `pub fn adapters::claude::context_tokens_of(usage: &serde_json::Value) -> u64` — unchanged signature and unchanged result, now implemented as `usage_categories(usage).context_total()`

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/commands/ctx/event.rs`:

```rust
    /// Issue #155, Phase 2: the four categories are recorded RAW. Before
    /// this, claude's adapter summed input + cache_creation + cache_read into
    /// `input_tokens` at the adapter boundary, which made a cache-hit ratio
    /// -- the one number that says whether prompt-shape work helped --
    /// uncomputable anywhere downstream.
    #[test]
    fn transcript_usage_keeps_the_cache_classes_apart_and_can_still_combine_them() {
        let usage = TranscriptUsage {
            input_tokens: 1_000,
            cache_creation_input_tokens: 8_000,
            cache_read_input_tokens: 91_000,
            output_tokens: 500,
        };
        assert_eq!(usage.context_total(), 100_000, "the pre-2.32.0 combined number");
        assert_eq!(TranscriptUsage::default().context_total(), 0);
    }
```

Add to `mod tests` in `src/commands/ctx/adapters/claude.rs`:

```rust
    /// The adapter stops pre-summing. `context_tokens_of` keeps its exact old
    /// meaning and value, because rot's context gate and every display path
    /// still want one combined "real context size" number -- it is just no
    /// longer the ONLY thing that survives the boundary.
    #[test]
    fn transcript_usage_reports_each_token_class_separately() {
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"usage":{"input_tokens":10,"#,
            r#""cache_creation_input_tokens":200,"cache_read_input_tokens":3000,"#,
            r#""output_tokens":40}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"usage":{"input_tokens":5,"#,
            r#""cache_creation_input_tokens":0,"cache_read_input_tokens":3100,"#,
            r#""output_tokens":7}}}"#,
        );
        let usage = transcript_usage(jsonl).expect("usage");
        assert_eq!(usage.input_tokens, 15);
        assert_eq!(usage.cache_creation_input_tokens, 200);
        assert_eq!(usage.cache_read_input_tokens, 6_100);
        assert_eq!(usage.output_tokens, 47);
        assert_eq!(
            usage.context_total(),
            6_315,
            "context_total must equal what the old pre-summed input_tokens was"
        );
    }

    /// A sidechain row still does not reach the MAIN-session usage total:
    /// Task 2.2 gives subagent spend its own bucket rather than folding it
    /// into a number whose meaning is "this session's own context".
    #[test]
    fn transcript_usage_still_excludes_sidechain_rows_from_the_main_total() {
        let jsonl = concat!(
            r#"{"type":"assistant","isSidechain":true,"message":{"usage":{"input_tokens":900,"#,
            r#""cache_read_input_tokens":900,"output_tokens":900}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"usage":{"input_tokens":1,"output_tokens":2}}}"#,
        );
        let usage = transcript_usage(jsonl).expect("usage");
        assert_eq!(usage.input_tokens, 1);
        assert_eq!(usage.cache_read_input_tokens, 0);
        assert_eq!(usage.output_tokens, 2);
    }
```

Add to `mod tests` in `src/commands/ctx/adapters/codex.rs`:

```rust
    /// Codex's rollout `TokenCount` totals expose no cache classes at all, so
    /// its two new fields stay 0 and `context_total()` degrades to exactly
    /// today's number. Never guess a class an adapter does not report.
    #[test]
    fn codex_reports_zero_for_the_cache_classes_it_cannot_see() {
        let adapter = CodexAdapter::new(None);
        let usage = adapter
            .transcript_usage(CODEX_TOKEN_COUNT_FIXTURE)
            .expect("usage");
        assert_eq!(usage.cache_creation_input_tokens, 0);
        assert_eq!(usage.cache_read_input_tokens, 0);
        assert_eq!(usage.context_total(), usage.input_tokens);
    }
```

(`CODEX_TOKEN_COUNT_FIXTURE` is the same rollout JSONL literal the existing `transcript_usage_uses_the_latest_cumulative_token_snapshot` test already builds — lift it to a `const` in that `mod tests` so both use one fixture.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin zirv event::tests::transcript_usage adapters::claude::tests::transcript_usage -- --test-threads=1`
Expected: FAIL to compile — `struct TranscriptUsage has no field named cache_creation_input_tokens`.

- [ ] **Step 3: Write minimal implementation**

`src/commands/ctx/event.rs`:

```rust
/// Cumulative token usage read from a harness transcript, in the four RAW
/// classes the provider bills separately. Adapters return `None` when their
/// transcript does not expose a verified usage shape, and `0` for a class
/// their transcript genuinely does not report (codex's cumulative
/// `TokenCount` totals carry no cache classes) -- never a guess.
///
/// Recorded raw, not pre-summed (issue #155, 2026-08-26): cache CREATION is
/// expensive and written once, cache READ is cheap and dominant in a healthy
/// session, and folding them together at the adapter boundary made the
/// cache-hit ratio -- the one number that says whether prompt-shape work
/// helped -- uncomputable anywhere downstream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TranscriptUsage {
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
}

impl TranscriptUsage {
    /// The combined "real context size" figure this type carried in
    /// `input_tokens` before 2.32.0: uncached input plus both cache classes.
    /// Every caller that genuinely wants ONE context-size number -- rot's
    /// token gate, status display -- calls this. Saturating, like every other
    /// token arithmetic in this crate.
    pub fn context_total(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_creation_input_tokens)
            .saturating_add(self.cache_read_input_tokens)
    }
}
```

`src/commands/ctx/adapters/claude.rs`:

```rust
/// The four raw token classes from one `message.usage` object. A missing
/// field is `0`, the same tolerance `context_tokens_of` has always had.
pub fn usage_categories(usage: &Value) -> TranscriptUsage {
    let field = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    TranscriptUsage {
        input_tokens: field("input_tokens"),
        cache_creation_input_tokens: field("cache_creation_input_tokens"),
        cache_read_input_tokens: field("cache_read_input_tokens"),
        output_tokens: field("output_tokens"),
    }
}

/// Real context size is `input_tokens` plus both cache fields; the bare
/// `input_tokens` field is near zero once prompt caching kicks in. Now a
/// DERIVED helper over [`usage_categories`] rather than the only thing that
/// survives the adapter boundary -- same signature, same value, so
/// `parse_events`' `AssistantFinal { input_tokens }` (which feeds rot's
/// context gate) is byte-for-byte unchanged.
pub fn context_tokens_of(usage: &Value) -> u64 {
    usage_categories(usage).context_total()
}
```

and in `transcript_usage`, replace the two `saturating_add` lines with a four-field fold:

```rust
        observed = true;
        let row = usage_categories(current);
        usage.input_tokens = usage.input_tokens.saturating_add(row.input_tokens);
        usage.cache_creation_input_tokens = usage
            .cache_creation_input_tokens
            .saturating_add(row.cache_creation_input_tokens);
        usage.cache_read_input_tokens = usage
            .cache_read_input_tokens
            .saturating_add(row.cache_read_input_tokens);
        usage.output_tokens = usage.output_tokens.saturating_add(row.output_tokens);
```

`src/commands/ctx/adapters/codex.rs`: the `TranscriptUsage` literal in `transcript_usage` gains `cache_creation_input_tokens: 0, cache_read_input_tokens: 0` with a comment naming *why* (`RolloutTokenTotals` has no such fields; a guessed class is worse than an honest zero).

`src/commands/workflow/engine.rs`: `UsageCheckpoint` gains `cumulative_cache_creation_input_tokens: u64` and `cumulative_cache_read_input_tokens: u64` (both `#[serde(default)]` on the persisted struct), and `usage_since`'s cumulative-delta arm subtracts all four fields rather than two. `enrich_transition_evidence`'s cross-session `saturating_add` fold likewise covers all four.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin zirv event:: adapters:: workflow::engine:: -- --test-threads=1`
Expected: PASS. `rg 'TranscriptUsage \{' src/` and confirm every literal names all four fields (or uses `..Default::default()`).

- [ ] **Step 5: Commit**

```bash
git checkout -b feat/token-cost-p2-telemetry
git add src/commands/ctx/event.rs src/commands/ctx/adapters src/commands/workflow/engine.rs
git commit -m "feat(ctx): record the four raw token classes instead of pre-summing them"
```

---

### Task 2.2: Session lineage and a sidechain bucket on `TelemetryEvent`

`TelemetryEvent` has `input_tokens`, `output_tokens`, `model`, `adapter`, `role` (a free string), `worker_count` (a bare int) and `workflow_id` — no session id, no parent link, no cache classes, no tool-call counts. Meanwhile `transcript_usage` drops `isSidechain` rows entirely, so subagent spend vanishes from workflow accounting while `window::sum_transcripts` still charges it to the 5h/7d quota estimate. Child cost is billed and unattributable at once.

**Files:**
- Modify: `src/commands/workflow/telemetry.rs` (`TelemetryEvent` at ~86, `TelemetryEvent::new` at ~114, `record`'s label-truncation loop at ~150)
- Modify: `src/commands/ctx/adapters/claude.rs` (a new sidechain-only reader beside `transcript_usage`)
- Modify: `src/commands/workflow/engine.rs` (`usage_checkpoint`, `usage_since`, `enrich_transition_evidence`)
- Test: inline `#[cfg(test)] mod tests` in `src/commands/workflow/telemetry.rs` and `src/commands/ctx/adapters/claude.rs`

**Interfaces:**
- Consumes: `event::TranscriptUsage` (Task 2.1), `adapters::claude::usage_categories` (Task 2.1).
- Produces:
  - `TelemetryEvent` gains nine `#[serde(default)]` fields (below)
  - `pub fn adapters::claude::sidechain_transcript_usage(jsonl: &str) -> Option<TranscriptUsage>`
  - `TELEMETRY_SCHEMA_VERSION` bumped by one

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/commands/workflow/telemetry.rs`:

```rust
    /// Issue #155, Phase 2: an event must carry enough to attribute spend to
    /// a session, its parent, and its work group -- and to separate the cache
    /// classes. `role` was a free string and `worker_count` a bare integer;
    /// neither could say WHICH worker cost what.
    #[test]
    fn a_telemetry_event_carries_raw_categories_and_session_lineage() {
        let mut event = TelemetryEvent::new(TelemetryKind::PhaseCompleted);
        event.input_tokens = Some(1_000);
        event.cache_creation_input_tokens = Some(8_000);
        event.cache_read_input_tokens = Some(91_000);
        event.output_tokens = Some(500);
        event.sidechain_input_tokens = Some(40);
        event.sidechain_cache_creation_input_tokens = Some(0);
        event.sidechain_cache_read_input_tokens = Some(12_000);
        event.sidechain_output_tokens = Some(90);
        event.session_id = Some("sess-child".to_string());
        event.parent_session_id = Some("sess-parent".to_string());
        event.work_group_id = Some("wg-1".to_string());

        let json = serde_json::to_string(&event).expect("serialize");
        let back: TelemetryEvent = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(back, event);

        let cached = back.cache_read_input_tokens.unwrap_or(0) as f64;
        let total = (back.input_tokens.unwrap_or(0)
            + back.cache_creation_input_tokens.unwrap_or(0)
            + back.cache_read_input_tokens.unwrap_or(0)) as f64;
        assert!(
            (cached / total - 0.91).abs() < 1e-9,
            "a cache-hit ratio must be computable from ONE event"
        );
    }

    /// Back-compatibility: an event written by 2.31.0 has none of these
    /// fields. Reading it must still work -- the telemetry directory is
    /// retained for days and an upgrade must not orphan it.
    #[test]
    fn an_event_written_before_this_change_still_deserialises() {
        let old = r#"{"schema_version":1,"id":"e1","timestamp":10,"workflow_id":null,
            "kind":"phase-completed","phase":null,"intent":null,"complexity":null,
            "risk":null,"duration_ms":null,"adapter":null,"model":null,"role":null,
            "input_tokens":7,"output_tokens":3,"succeeded":true,"findings_total":0,
            "findings_meaningful":0,"findings_dismissed":0,"fix_round":0,
            "artifact_count":0,"worker_count":0}"#;
        let event: TelemetryEvent = serde_json::from_str(old).expect("old events still parse");
        assert_eq!(event.input_tokens, Some(7));
        assert_eq!(event.cache_read_input_tokens, None);
        assert_eq!(event.session_id, None);
    }
```

Add to `mod tests` in `src/commands/ctx/adapters/claude.rs`:

```rust
    /// Subagent turns live in `isSidechain` rows. They are charged to the
    /// account (`window::sum_transcripts` walks `subagents/` too) but were
    /// dropped from workflow accounting entirely. Counted separately here, so
    /// the main-session number keeps meaning "this session's own context"
    /// while the child spend stops being invisible.
    #[test]
    fn sidechain_usage_is_counted_separately_rather_than_dropped() {
        let jsonl = concat!(
            r#"{"type":"assistant","isSidechain":true,"message":{"usage":{"input_tokens":900,"#,
            r#""cache_read_input_tokens":12000,"output_tokens":90}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"usage":{"input_tokens":1,"output_tokens":2}}}"#,
        );
        let side = sidechain_transcript_usage(jsonl).expect("sidechain usage");
        assert_eq!(side.input_tokens, 900);
        assert_eq!(side.cache_read_input_tokens, 12_000);
        assert_eq!(side.output_tokens, 90);

        assert_eq!(
            sidechain_transcript_usage(
                r#"{"type":"assistant","message":{"usage":{"input_tokens":1}}}"#
            ),
            None,
            "no sidechain rows means None, not a zeroed reading"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin zirv workflow::telemetry::tests -- --test-threads=1`
Expected: FAIL to compile — `no field cache_creation_input_tokens on type TelemetryEvent`.

- [ ] **Step 3: Write minimal implementation**

In `src/commands/workflow/telemetry.rs`, append to `TelemetryEvent` (every one `#[serde(default)]`, matching the existing `work_domain`/`token_usage_source` pattern) and to `TelemetryEvent::new` (each initialised `None`):

```rust
    /// Issue #155: the raw classes behind `input_tokens`. `input_tokens`
    /// keeps its existing meaning (uncached input) and these two carry what
    /// the adapter used to fold into it.
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
    /// Subagent (`isSidechain`) spend for the same phase, in the same four
    /// classes. Its own bucket rather than folded into the main numbers: the
    /// main numbers mean "this session's own context", and a subagent's
    /// tokens are not part of it -- but they ARE charged to the account, so
    /// dropping them (the pre-2.32.0 behaviour) made a phase look cheaper
    /// than it was.
    #[serde(default)]
    pub sidechain_input_tokens: Option<u64>,
    #[serde(default)]
    pub sidechain_cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    pub sidechain_cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    pub sidechain_output_tokens: Option<u64>,
    /// The harness session this event was produced by, its parent (the
    /// session that delegated the work), and the work group both belong to.
    /// `role`/`worker_count` said what KIND of thing ran and how many; these
    /// say WHICH, which is what makes a delegation tree's cost attributable.
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub parent_session_id: Option<String>,
    #[serde(default)]
    pub work_group_id: Option<String>,
```

Bump `TELEMETRY_SCHEMA_VERSION` by one, and add `&mut event.session_id`, `&mut event.parent_session_id`, `&mut event.work_group_id` to `record`'s `MAX_LABEL_BYTES` truncation loop — they are strings that reach disk and every other string field is already bounded there.

In `src/commands/ctx/adapters/claude.rs`, add beside `transcript_usage`:

```rust
/// The same fold as [`transcript_usage`], over the rows it deliberately
/// skips: `isSidechain == true` assistant turns, i.e. subagent work. `None`
/// when the transcript has no sidechain rows at all -- an honest "no data",
/// never a zeroed reading, the same distinction `transcript_usage`'s own
/// `observed` flag draws.
pub fn sidechain_transcript_usage(jsonl: &str) -> Option<TranscriptUsage> { … }
```

Implement it by extracting the shared body of `transcript_usage` into a private `fold_assistant_usage(jsonl: &str, want_sidechain: bool) -> Option<TranscriptUsage>` and having both call it — one fold, two filters, so the two can never drift on what counts as an assistant usage row.

In `src/commands/workflow/engine.rs`, `enrich_transition_evidence` populates the six new numeric fields from `usage_since` (main) and a matching sidechain read over the same byte range, and sets `session_id` from `session_identity()`'s first element. `parent_session_id` and `work_group_id` stay `None` until Phase 5 supplies them — leave the fields, do not invent values.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin zirv workflow:: adapters::claude:: -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/commands/workflow/telemetry.rs src/commands/workflow/engine.rs src/commands/ctx/adapters/claude.rs
git commit -m "feat(workflow): record cache classes, sidechain spend and session lineage on telemetry"
```

---

### Task 2.3: A per-delegation checkpoint record

`log::Decision { ts, session, verb, verdict, score, action, detail }` is a rotation log. Nothing anywhere records what a delegated worker cost, so "was delegating cheaper than doing it on the seat?" is unanswerable — which is the exact question Phase 5's design rests on.

**Files:**
- Modify: `src/commands/ctx/log.rs` (new record + appender + reader)
- Modify: `src/commands/ctx/agent.rs` (`run_with` at ~545, after `exec::run_with` returns)
- Test: inline `#[cfg(test)] mod tests` in `src/commands/ctx/log.rs` and `src/commands/ctx/agent.rs`

**Interfaces:**
- Consumes: `event::TranscriptUsage` (Task 2.1), `state::{create_private_dir_all, open_private_append}`, `adapters::AgentAdapter::transcript_usage`/`transcript_path`.
- Produces:
  - `pub const log::DELEGATION_FILE: &str = "delegations.jsonl";`
  - `pub struct log::Delegation<'a> { ts, session, parent_session, work_group_id: Option<&'a str>, agent, model: Option<&'a str>, input_tokens, cache_creation_input_tokens, cache_read_input_tokens, output_tokens, wall_ms, exit_code, outcome }`
  - `pub fn log::append_delegation(state: &StateDir, record: &Delegation<'_>) -> CtxResult<()>`
  - `pub fn log::tail_delegations(state: &StateDir, count: usize) -> CtxResult<Vec<String>>`
  - `pub const log::DELEGATION_ACTION: &str = "delegation-complete";`

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/commands/ctx/log.rs`:

```rust
    /// Issue #155, Phase 2: a delegation checkpoint. Its own file, like
    /// `SafetyDecision`'s own daily buckets -- the decision log is a rotation
    /// log and mixing a per-delegation cost record into it would make both
    /// harder to read. The main log still gets a one-line
    /// `delegation-complete` decision so a reader who only looks there sees
    /// that a delegation happened.
    #[test]
    fn a_delegation_record_appends_as_jsonl_with_all_four_token_classes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        append_delegation(
            &state,
            &Delegation {
                ts: 1_700_000_000,
                session: "sess-child",
                parent_session: "sess-parent",
                work_group_id: None,
                agent: "codex",
                model: Some("gpt-5-codex"),
                input_tokens: 1_000,
                cache_creation_input_tokens: 8_000,
                cache_read_input_tokens: 91_000,
                output_tokens: 500,
                wall_ms: 42_000,
                exit_code: 0,
                outcome: "ok",
            },
        )
        .expect("append");

        let lines = tail_delegations(&state, 10).expect("tail");
        assert_eq!(lines.len(), 1);
        let value: serde_json::Value = serde_json::from_str(&lines[0]).expect("json");
        assert_eq!(value["agent"], "codex");
        assert_eq!(value["model"], "gpt-5-codex");
        assert_eq!(value["cache_read_input_tokens"], 91_000);
        assert_eq!(value["wall_ms"], 42_000);
        assert_eq!(value["outcome"], "ok");
        assert_eq!(value["exit_code"], 0);
    }

    /// An empty file (or none at all) is an empty list, never an error --
    /// same contract `tail` already has for the decision log.
    #[test]
    fn tailing_delegations_before_any_exist_is_empty_not_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        assert!(tail_delegations(&state, 10).expect("tail").is_empty());
    }
```

Add to `mod tests` in `src/commands/ctx/agent.rs`:

```rust
    /// The classifier half, testable without spawning anything: a completed
    /// delegation's outcome label must distinguish the supervisor's own two
    /// failure modes from an ordinary non-zero exit, because "the worker
    /// failed" and "zirv gave up on the worker" cost very different things.
    #[test]
    fn a_delegation_outcome_names_the_supervisors_own_failures() {
        assert_eq!(delegation_outcome(0), "ok");
        assert_eq!(delegation_outcome(exec::EXIT_ROT_EXHAUSTED), "rot-exhausted");
        assert_eq!(delegation_outcome(exec::EXIT_TIMEOUT), "timeout");
        assert_eq!(delegation_outcome(1), "failed");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin zirv log::tests::a_delegation_record agent::tests::a_delegation_outcome -- --test-threads=1`
Expected: FAIL to compile — `cannot find struct Delegation in this scope`.

- [ ] **Step 3: Write minimal implementation**

`src/commands/ctx/log.rs`:

```rust
pub const DELEGATION_FILE: &str = "delegations.jsonl";

/// `Decision::action` for the one-line marker written into the MAIN decision
/// log alongside every delegation record.
pub const DELEGATION_ACTION: &str = "delegation-complete";

/// One completed `zirv ctx agent` delegation, with what it actually cost.
///
/// Its own file rather than a `Decision` variant (issue #155): the decision
/// log is a rotation log keyed by verdict/score, and a cost record has
/// neither. `Delegation` is what answers "was delegating this cheaper than
/// doing it on the orchestrator seat", which is the question every later
/// phase's design rests on.
///
/// Token classes are the four RAW ones (`event::TranscriptUsage`), never a
/// pre-summed total: a delegated worker's cache-hit ratio is precisely how
/// you tell a well-shaped worker prompt from a badly-shaped one.
#[derive(Debug, Serialize)]
pub struct Delegation<'a> {
    pub ts: u64,
    pub session: &'a str,
    pub parent_session: &'a str,
    pub work_group_id: Option<&'a str>,
    pub agent: &'a str,
    pub model: Option<&'a str>,
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
    pub wall_ms: u64,
    pub exit_code: i32,
    pub outcome: &'a str,
}

pub fn append_delegation(state: &StateDir, record: &Delegation<'_>) -> CtxResult<()> { … }

pub fn tail_delegations(state: &StateDir, count: usize) -> CtxResult<Vec<String>> { … }
```

Both bodies mirror `append`/`tail` exactly (`state.logs()`, `create_private_dir_all`, `open_private_append`, `writeln!` of `serde_json::to_string`; a missing file tails to an empty `Vec`).

`src/commands/ctx/agent.rs` — add the outcome classifier and the write. `run_with` already resolves `adapter`, `args.name`, and the worker model, and already mints the session id it hands to `ExecArgs`. Capture that id in a local before the struct is built, time the run, and read the worker's own transcript after it returns:

```rust
/// Which of the supervisor's outcomes this exit code represents. Mirrors
/// `exit_note`'s own two special cases: those are zirv giving up, not the
/// worker failing, and they cost very differently.
fn delegation_outcome(code: i32) -> &'static str {
    match code {
        0 => "ok",
        exec::EXIT_ROT_EXHAUSTED => "rot-exhausted",
        exec::EXIT_TIMEOUT => "timeout",
        _ => "failed",
    }
}
```

and, replacing the bare `let code = exec::run_with(&exec_args, w, repo, &env)?;`:

```rust
    let started = std::time::Instant::now();
    let code = exec::run_with(&exec_args, w, repo, &env)?;
    let wall_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    // Best effort throughout: a delegation that ran must never fail because
    // its accounting could not be written.
    if let Ok(state_dir) = super::state::StateDir::resolve(env) {
        let usage = adapter
            .transcript_path(&super::event::SessionRef {
                id: SessionId::parse(&worker_session),
                cwd: repo.to_path_buf(),
            })
            .pipe_read_to_string()
            .and_then(|body| adapter.transcript_usage(&body))
            .unwrap_or_default();
        let model = adapters::last_worker_model(&cfg, &args.name, adapter.as_ref());
        let detail = format!(
            "{} ({}): {} in / {} cache-read / {} out in {}ms -- {}",
            args.name,
            model.as_deref().unwrap_or("default worker model"),
            usage.input_tokens,
            usage.cache_read_input_tokens,
            usage.output_tokens,
            wall_ms,
            delegation_outcome(code),
        );
        let _ = super::log::append_delegation(&state_dir, &super::log::Delegation { … });
        let _ = super::log::append(
            &state_dir,
            &super::log::Decision {
                ts: super::state::now_secs(),
                session: &worker_session,
                verb: "agent",
                verdict: "n/a",
                score: 0,
                action: super::log::DELEGATION_ACTION,
                detail: &detail,
            },
        );
    }
```

Two implementation notes the worker must resolve rather than invent:
- `pipe_read_to_string` above is shorthand for `std::fs::read_to_string(path).ok()`; write it plainly, no helper.
- `adapters::last_worker_model` does not exist. The model actually launched is whatever `worker_launch_flags` produced: reuse that value by capturing `let command = worker_launch_flags(...)` (already in `run_with`) and reading the token following `--model`/`-m` out of it with the existing `adapters::classify_model_flag`. If none is present, pass `None` — never guess.
- `parent_session` is this process's own session id from the environment where one exists (the same lookup `workflow::engine::session_identity` performs); `""` otherwise. `work_group_id` is `None` until Phase 5.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin zirv log:: agent:: -- --test-threads=1`
Expected: PASS. Then run a real delegation (`zirv agent codex "print hello"`) and confirm one line lands in `<state>/logs/delegations.jsonl` and one `delegation-complete` line in `decisions.jsonl`.

- [ ] **Step 5: Commit**

```bash
git add src/commands/ctx/log.rs src/commands/ctx/agent.rs
git commit -m "feat(ctx): record what every delegated worker actually cost"
```

---

### Task 2.4: `zirv ctx usage --sessions`

`zirv ctx usage` is account/provider-scoped: two percentages and a pacing verdict. Nothing answers "which sessions spent that, and how much of it was cache".

**Files:**
- Modify: `src/commands/ctx/usage.rs` (`UsageArgs` at ~15, `run_with` at ~268)
- Modify: `src/commands/ctx/window.rs` (a per-file variant of the existing sum)
- Test: inline `#[cfg(test)] mod tests` in `src/commands/ctx/window.rs` and `src/commands/ctx/usage.rs`

**Interfaces:**
- Consumes: `window::{projects_root, parse_iso8601_utc}`, `adapters::claude::usage_categories` (Task 2.1).
- Produces:
  - `usage::UsageArgs` gains `#[arg(long)] pub sessions: bool`
  - `pub struct window::SessionSpend { pub session: String, pub input_tokens: u64, pub cache_creation_input_tokens: u64, pub cache_read_input_tokens: u64, pub output_tokens: u64, pub events: usize, pub newest_at: u64 }`
  - `pub fn window::session_spend(projects_root: &Path, now: u64, window_secs: u64) -> Vec<SessionSpend>`
  - `pub fn usage::render_sessions<W: Write>(w: &mut W, spend: &[window::SessionSpend]) -> CtxResult<()>`

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/commands/ctx/window.rs`:

```rust
    /// Issue #155, Phase 2: per-session spend in the four raw classes, over a
    /// trailing window. `sum_transcripts` already walks every transcript
    /// including `subagents/`, because those tokens are charged too -- this
    /// keeps the same walk and stops throwing the file identity away.
    #[test]
    fn session_spend_reports_each_transcript_separately_in_raw_classes() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            root.path().join("sess-a.jsonl"),
            concat!(
                r#"{"type":"assistant","timestamp":"2026-08-26T10:00:00Z","message":{"usage":"#,
                r#"{"input_tokens":10,"cache_creation_input_tokens":100,"#,
                r#""cache_read_input_tokens":900,"output_tokens":5}}}"#,
            ),
        )
        .expect("write");
        std::fs::write(
            root.path().join("sess-b.jsonl"),
            concat!(
                r#"{"type":"assistant","timestamp":"2026-08-26T10:00:00Z","message":{"usage":"#,
                r#"{"input_tokens":7,"output_tokens":1}}}"#,
            ),
        )
        .expect("write");

        let now = parse_iso8601_utc("2026-08-26T11:00:00Z").expect("now");
        let mut spend = session_spend(root.path(), now, 86_400);
        spend.sort_by(|a, b| a.session.cmp(&b.session));
        assert_eq!(spend.len(), 2);
        assert_eq!(spend[0].session, "sess-a");
        assert_eq!(spend[0].cache_read_input_tokens, 900);
        assert_eq!(spend[0].cache_creation_input_tokens, 100);
        assert_eq!(spend[1].session, "sess-b");
        assert_eq!(spend[1].cache_read_input_tokens, 0);
    }

    /// A row older than the window is not counted -- and a session whose rows
    /// are ALL outside it does not appear at all, rather than appearing as a
    /// zero.
    #[test]
    fn session_spend_drops_a_session_with_nothing_inside_the_window() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            root.path().join("stale.jsonl"),
            concat!(
                r#"{"type":"assistant","timestamp":"2026-08-01T10:00:00Z","message":{"usage":"#,
                r#"{"input_tokens":10,"output_tokens":5}}}"#,
            ),
        )
        .expect("write");
        let now = parse_iso8601_utc("2026-08-26T11:00:00Z").expect("now");
        assert!(session_spend(root.path(), now, 86_400).is_empty());
    }
```

Add to `mod tests` in `src/commands/ctx/usage.rs`:

```rust
    /// The report is honest about being an approximation and never invents a
    /// percentage: these are raw counts, and the only derived figure is the
    /// cache-hit ratio, which is a pure function of them.
    #[test]
    fn the_sessions_report_shows_raw_counts_and_a_cache_hit_ratio() {
        let spend = vec![window::SessionSpend {
            session: "sess-a".to_string(),
            input_tokens: 1_000,
            cache_creation_input_tokens: 8_000,
            cache_read_input_tokens: 91_000,
            output_tokens: 500,
            events: 12,
            newest_at: 1_700_000_000,
        }];
        let mut out = Vec::new();
        render_sessions(&mut out, &spend).expect("render");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("sess-a"), "got {text}");
        assert!(text.contains("91000"), "raw cache-read count: {text}");
        assert!(text.contains("91.0%"), "cache-hit ratio: {text}");
    }

    /// Nothing to report is a sentence, not an empty section.
    #[test]
    fn the_sessions_report_says_so_when_there_is_nothing_in_the_window() {
        let mut out = Vec::new();
        render_sessions(&mut out, &[]).expect("render");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("no session activity"), "got {text}");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin zirv window::tests::session_spend usage::tests::the_sessions_report -- --test-threads=1`
Expected: FAIL to compile — `cannot find function session_spend in this scope`.

- [ ] **Step 3: Write minimal implementation**

`src/commands/ctx/window.rs`: `session_spend` reuses `sum_transcripts`'s directory walk verbatim (including descending into `subagents/`, for the same reason: those tokens are charged), but folds per file. The session name is the file stem. A row is counted when its `timestamp` parses, is not more than `FUTURE_SKEW_TOLERANCE_SECS` in the future, and is within `window_secs` of `now` — the same three guards `sum_file` applies. The four classes come from `super::adapters::claude::usage_categories`, so this function and `TranscriptUsage` can never disagree about what a class is. A file contributing no in-window rows produces no entry.

`src/commands/ctx/usage.rs`: add the flag and the renderer.

```rust
pub struct UsageArgs {
    #[command(subcommand)]
    pub action: Option<UsageAction>,
    /// Break the last 24 hours down per session, in raw token classes.
    #[arg(long, default_value_t = false)]
    pub sessions: bool,
}
```

In `run_with`'s `None` arm, after the existing `report(...)` call, when `args.sessions` is set: resolve `window::projects_root()`, call `session_spend(&root, now, 86_400)`, sort by `input + cache_creation + cache_read + output` descending, and `render_sessions`. A failure to resolve the projects root degrades to the "no session activity" line — this is a report, and it must never turn a working `zirv ctx usage` into an error.

`render_sessions` prints a header, one line per session (`session`, the four raw counts, the cache-hit ratio to one decimal, `events`), and the "token class weighting is undocumented, treat as an approximation" caveat the existing estimator section already carries.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin zirv window:: usage:: -- --test-threads=1`
Expected: PASS. Then run `zirv ctx usage` (unchanged output) and `zirv ctx usage --sessions` (adds the breakdown).

- [ ] **Step 5: Commit**

```bash
git add src/commands/ctx/usage.rs src/commands/ctx/window.rs
git commit -m "feat(ctx): add zirv ctx usage --sessions with per-session raw token classes"
```

---

### Task 2.5: Version bump and vault updates for Phase 2

**Files:**
- Modify: `Cargo.toml` (`2.32.0`), `Cargo.lock`
- Modify: `docs/obsidian/Modules/Ctx Subsystem.md`, `docs/obsidian/Modules/Ctx Adapters.md`, `docs/obsidian/Modules/Built-in Commands.md`, `docs/obsidian/Modules/Usage and Pacing.md`, `docs/obsidian/Development/{Decision Log,Work Journal,Active Work}.md`

**Interfaces:**
- Consumes: Tasks 2.1–2.4.
- Produces: `Cargo.toml` version `2.32.0`.

- [ ] **Step 1: Bump the version to `2.32.0`**, then `cargo build`, then `rg '2\.31\.0' src/ docs/`.

- [ ] **Step 2: Update the vault** (bump each page's `last-verified`):
- `Modules/Ctx Adapters.md` — `TranscriptUsage` is four raw classes; `context_tokens_of` is now derived; codex reports `0` for classes it cannot see; `sidechain_transcript_usage` exists.
- `Modules/Ctx Subsystem.md` — `delegations.jsonl` and the `delegation-complete` decision action.
- `Modules/Built-in Commands.md` — `zirv ctx usage --sessions`.
- `Modules/Usage and Pacing.md` — the per-session breakdown, and the standing caveat that class weighting is undocumented.
- `Development/Decision Log.md` — why the four classes are recorded raw (a cache-hit ratio is the one number that says whether prompt-shape work helped), why sidechain spend gets its own bucket rather than being folded in or dropped, and why the delegation record lives in its own file.
- `Development/Work Journal.md`, `Development/Active Work.md` — Phase 2 completed; Phase 3 next.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock docs/obsidian
git commit -m "chore: bump to 2.32.0 and record the telemetry shape change"
```

---

### Task 2.6: Phase 2 verification gates, cross-review, and PR

**Files:** none modified by default.

- [ ] **Step 1: Capture the `main` baseline** exactly as in Task 1.5 Step 1 (confirm the `test result:` line exists; sorted failure NAMES only).

- [ ] **Step 2: Run all five gates in the FOREGROUND** (build, nextest `--no-fail-fast`, `cargo test --verbose -- --test-threads=1`, `fmt --check`, `clippy --all-targets -D warnings`). Diff failure NAMES, never counts.

- [ ] **Step 3: Take the BASELINE measurement this phase exists to enable**

This is the phase's real deliverable. On a machine that has run sessions today:

```bash
zirv ctx usage --sessions
```

Record, and paste into the PR body: total tokens per class across the last 24h, the aggregate cache-hit ratio, and the per-session breakdown. This is the **before** number that Phases 3–6 are measured against. Also confirm `delegations.jsonl` is being written by running one real `zirv agent` delegation.

- [ ] **Step 4: Codex cross-review**

```bash
zirv agent codex "Review the diff on branch feat/token-cost-p2-telemetry against main. Focus on: (1) whether context_tokens_of still returns EXACTLY the value it did before, since rot's context gate depends on it; (2) whether any TranscriptUsage construction site was missed and silently defaults a class to 0 where real data existed; (3) whether TelemetryEvent still deserialises from a 2.31.0-written JSON file. Reply with confirmed, concrete findings only." -- --model gpt-5-codex
```

- [ ] **Step 5: Open the PR** against the Phase 1 branch (or `main` if Phase 1 is merged), with the baseline numbers from Step 3 in the body.

**Phase 2 acceptance criteria (all measurable):**
- All four token classes are non-zero on a real cached claude transcript, and `context_total()` equals the pre-2.32.0 combined `input_tokens` exactly.
- A cache-hit ratio is computable from a single `TelemetryEvent`.
- A `TelemetryEvent` JSON written by 2.31.0 still deserialises.
- One `delegations.jsonl` record and one `delegation-complete` decision per completed `zirv ctx agent` run.
- `zirv ctx usage --sessions` names ≥1 session with a non-zero `cache_read_input_tokens` on a machine that ran a session today.

---

# Phase 3 — canonical-context dedupe (PR 3, version `2.33.0`, branch `feat/token-cost-p3-context-dedupe`)

Spec section: "Phase 3 — canonical-context dedupe". `context_cli::render_generated` writes `<repo>/CLAUDE.md` (8295 bytes here) and `<repo>/AGENTS.md` (7680 bytes) as the managed marker plus `common.md` plus the harness file, verbatim. Claude Code reads `CLAUDE.md` natively at session start with no zirv involvement. Then `compile::with_canonical_context_layer` injects the same bytes AGAIN into the system prompt, unconditionally. Roughly 8 KiB duplicated per session, in the one layer that would otherwise be perfectly cacheable.

### Task 3.1: Stamp a canonical-content hash into the generated files

**Files:**
- Modify: `src/commands/ctx/context_cli.rs` (`MANAGED_MARKER` at ~75, `is_managed` at ~86, `render_generated` at ~136, `run_generate` at ~328)
- Test: inline `#[cfg(test)] mod tests` in `src/commands/ctx/context_cli.rs`

**Interfaces:**
- Consumes: `sha2::{Digest, Sha256}` (already a dependency — confirm with `rg '^sha2' Cargo.toml`; if absent, add it in this task and say so in the PR body).
- Produces:
  - `pub const context_cli::CANONICAL_HASH_PREFIX: &str = "<!-- zirv:canonical-sha256:";`
  - `pub fn context_cli::canonical_sha256(common: Option<&str>, harness_specific: Option<&str>) -> String` — 64 lowercase hex chars
  - `pub fn context_cli::embedded_canonical_sha256(text: &str) -> Option<String>`
  - `render_generated` emits the hash line immediately after `MANAGED_MARKER`

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/commands/ctx/context_cli.rs`:

```rust
    /// Issue #155, Phase 3: a generated file states, in its own bytes, which
    /// canonical content it was rendered from. That is what lets `compile.rs`
    /// prove -- not guess -- that the harness has already read these exact
    /// instructions natively, and skip re-injecting them.
    #[test]
    fn a_generated_file_carries_the_hash_of_the_canonical_content_it_rendered() {
        let rendered = render_generated(Some("common text"), Some("claude text"));
        let embedded = embedded_canonical_sha256(&rendered).expect("a hash line");
        assert_eq!(embedded, canonical_sha256(Some("common text"), Some("claude text")));
        assert_eq!(embedded.len(), 64);
        assert!(embedded.chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
        assert!(
            is_managed(&rendered),
            "the hash line must sit INSIDE the managed prefix, so an older binary still \
             recognises the file as managed"
        );
    }

    /// Domain separation: the two halves must not be concatenable into each
    /// other. Without a length-or-delimiter-committed hash, moving one
    /// character across the boundary would produce the same digest, and the
    /// dedupe would then skip injecting content the file does NOT hold.
    #[test]
    fn the_canonical_hash_cannot_be_collided_by_moving_the_boundary() {
        assert_ne!(
            canonical_sha256(Some("ab"), Some("c")),
            canonical_sha256(Some("a"), Some("bc"))
        );
        assert_ne!(
            canonical_sha256(Some("x"), None),
            canonical_sha256(None, Some("x"))
        );
    }

    /// The hash is computed over the same TRIMMED text `render_generated`
    /// actually writes, so trailing-whitespace churn in `.zirv/context/`
    /// never invalidates a file whose delivered bytes did not change.
    #[test]
    fn the_canonical_hash_ignores_whitespace_the_render_itself_trims() {
        assert_eq!(
            canonical_sha256(Some("common text"), None),
            canonical_sha256(Some("\n\n  common text  \n"), None)
        );
        assert_eq!(
            canonical_sha256(Some("common"), Some("   ")),
            canonical_sha256(Some("common"), None),
            "an all-whitespace harness file renders nothing, so it must hash as nothing"
        );
    }

    /// A file with no hash line at all -- one generated by an older zirv --
    /// reads as `None`, never as an error and never as a wrong hash.
    #[test]
    fn a_file_generated_before_this_change_has_no_embedded_hash() {
        let old = format!("{MANAGED_MARKER}\n\ncommon text\n");
        assert!(is_managed(&old));
        assert_eq!(embedded_canonical_sha256(&old), None);
    }

    /// Regeneration stays byte-for-byte deterministic: no clock, no session
    /// detail. `generate_one`'s `Unchanged` outcome depends on it.
    #[test]
    fn rendering_twice_with_the_same_inputs_produces_identical_bytes() {
        assert_eq!(
            render_generated(Some("common"), Some("harness")),
            render_generated(Some("common"), Some("harness"))
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin zirv context_cli::tests::a_generated_file_carries -- --test-threads=1`
Expected: FAIL to compile — `cannot find function canonical_sha256 in this scope`.

- [ ] **Step 3: Write minimal implementation**

```rust
/// The prefix of a generated file's canonical-content provenance line. Sits
/// on the line immediately after [`MANAGED_MARKER`], so it is inside the
/// managed prefix [`is_managed`] recognises and a zirv old enough not to know
/// about it still reads the file as managed.
pub const CANONICAL_HASH_PREFIX: &str = "<!-- zirv:canonical-sha256:";

/// SHA-256 over the exact canonical inputs a render used, as 64 lowercase
/// hex characters.
///
/// Domain-separated by length prefix, not by a delimiter: hashing
/// `common + harness` directly would give `("ab","c")` and `("a","bc")` the
/// same digest, and a collision here would make `compile.rs` skip injecting
/// content the native file does not actually hold -- a silent instruction
/// loss, which is the one failure this whole phase must not introduce.
///
/// Hashes the TRIMMED text, matching what [`render_generated`] actually
/// writes, so trailing-whitespace churn in `.zirv/context/` cannot
/// invalidate a file whose delivered bytes are unchanged. An absent or
/// all-whitespace input is `None`/empty for both, so the two agree.
pub fn canonical_sha256(common: Option<&str>, harness_specific: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    let normalize = |t: Option<&str>| -> String {
        t.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("").to_string()
    };
    let common = normalize(common);
    let harness = normalize(harness_specific);
    let mut hasher = Sha256::new();
    hasher.update(b"zirv-canonical-v1");
    for part in [&common, &harness] {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

/// The hash a generated file states it was rendered from, if it states one.
/// `None` for a file generated before this change, for a native file, or for
/// a malformed line -- every one of which correctly means "cannot prove
/// equality", which `compile.rs` reads as "inject as before".
pub fn embedded_canonical_sha256(text: &str) -> Option<String> {
    let line = text
        .lines()
        .take(4)
        .find_map(|line| line.trim().strip_prefix(CANONICAL_HASH_PREFIX))?;
    let hex = line.trim().strip_suffix("-->")?.trim();
    (hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()))
        .then(|| hex.to_string())
}
```

In `render_generated`, immediately after the `MANAGED_MARKER` line:

```rust
    out.push_str(CANONICAL_HASH_PREFIX);
    out.push_str(&canonical_sha256(common, harness_specific));
    out.push_str(" -->\n");
```

`run_generate` needs no change — it already passes the same two `Option<&str>` values to `render_generated`, and both sides now derive the hash from exactly those.

Note for the worker: adding a line to `render_generated` changes the bytes of every generated file, so `generate_one` will report `regenerated` rather than `unchanged` on the first run after this ships. That is correct and expected; do not special-case it.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin zirv context_cli:: drift:: context_status:: -- --test-threads=1`
Expected: PASS. `context_cli::surfaces_for_drift` filters on `is_managed`, which still holds — confirm no drift test flipped.

Then regenerate this repo's own files and confirm the line is present and correct:

```bash
zirv context sync --generate
head -3 CLAUDE.md
```

- [ ] **Step 5: Commit**

```bash
git checkout -b feat/token-cost-p3-context-dedupe
git add src/commands/ctx/context_cli.rs
git commit -m "feat(context): stamp the canonical-content hash into generated harness files"
```

---

### Task 3.2: Skip the canonical injection the harness already read natively

**Files:**
- Modify: `src/commands/ctx/compile.rs` (`with_canonical_context_layer` at ~236)
- Modify: `src/commands/ctx/config.rs` (`ContextConfig` at ~335 and its `Default`; `ALL_CONFIG_KEYS` at ~4188)
- Modify: `.zirv/ctx.toml`
- Test: inline `#[cfg(test)] mod tests` in `src/commands/ctx/compile.rs` and `src/commands/ctx/config.rs`

**Interfaces:**
- Consumes: `context_cli::{canonical_sha256, embedded_canonical_sha256, is_managed}` (Task 3.1), `context::{common_path, claude_path, codex_path}`, `log::{append, Decision}`.
- Produces:
  - `ContextConfig` gains `pub dedupe_native: bool` (default `true`)
  - `pub const compile::DEDUP_SKIP_ACTION: &str = "context-dedup-skip";`
  - `fn compile::native_context_path(adapter_name: &str, repo: &Path) -> Option<PathBuf>`
  - `fn compile::native_file_already_carries_canonical(adapter_name: &str, repo: &Path, cfg: &CtxConfig) -> bool`

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/commands/ctx/compile.rs`:

```rust
    /// Issue #155, Phase 3: claude reads `<repo>/CLAUDE.md` natively at
    /// session start, with no zirv involvement. When that file is a
    /// zirv-managed render of the CURRENT canonical content -- proven by the
    /// embedded hash, not assumed -- injecting the same ~8 KiB again into the
    /// system prompt buys nothing and costs the most cacheable layer there is.
    #[test]
    fn a_matching_native_file_skips_the_canonical_context_injection() {
        let repo = repo_with_context_files(&[
            ("common.md", "canonical common instructions\n"),
            ("claude.md", "claude-specific addition\n"),
        ]);
        let home = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(home.path().join("state"));
        std::fs::write(
            repo.path().join("CLAUDE.md"),
            crate::commands::ctx::context_cli::render_generated(
                Some("canonical common instructions\n"),
                Some("claude-specific addition\n"),
            ),
        )
        .expect("write native CLAUDE.md");

        let compiled = compile(
            Some(home.path()),
            repo.path(),
            false,
            &CtxConfig::default(),
            &ClaudeAdapter::new(None),
            PromptRole::Orchestrator,
            &state,
            now_secs(),
            LaunchMode::Interactive,
            true,
        );
        let composed = compiled.composed.expect("composed");

        assert!(
            !composed.sources.contains(&PromptSource::Context),
            "the canonical layer must be skipped: {:?}",
            composed.sources
        );
        assert!(
            !composed.text.contains("canonical common instructions"),
            "and its bytes must actually be absent"
        );
        assert!(
            compiled.provenance.iter().all(|p| p.delivered_bytes == 0),
            "provenance still REPORTS the surfaces, at zero delivered bytes"
        );
        let lines = crate::commands::ctx::log::tail(&state, 20).expect("decision log");
        assert!(
            lines.iter().any(|line| line.contains("context-dedup-skip")),
            "the skip must be recorded: {lines:?}"
        );
    }

    /// The fallback, and the safety property this phase rests on: a native
    /// file that does not PROVABLY hold the current canonical bytes changes
    /// nothing. Editing `.zirv/context/` without regenerating must restore
    /// full injection on the very next compose, or the session silently loses
    /// instructions.
    #[test]
    fn a_stale_or_absent_or_unmanaged_native_file_injects_exactly_as_before() {
        let home = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(home.path().join("state"));
        let cases: [(&str, Option<String>); 3] = [
            ("no native file at all", None),
            (
                "a hand-written native file",
                Some("# My own CLAUDE.md\n\ncanonical common instructions\n".to_string()),
            ),
            (
                "a managed file rendered from OLDER canonical content",
                Some(crate::commands::ctx::context_cli::render_generated(
                    Some("what common.md used to say\n"),
                    None,
                )),
            ),
        ];

        for (label, native) in cases {
            let repo = repo_with_context_files(&[("common.md", "canonical common instructions\n")]);
            if let Some(text) = native {
                std::fs::write(repo.path().join("CLAUDE.md"), text).expect("write");
            }
            let compiled = compile(
                Some(home.path()),
                repo.path(),
                false,
                &CtxConfig::default(),
                &ClaudeAdapter::new(None),
                PromptRole::Orchestrator,
                &state,
                now_secs(),
                LaunchMode::Interactive,
                false,
            );
            let composed = compiled.composed.expect("composed");
            assert!(
                composed.sources.contains(&PromptSource::Context),
                "{label}: must inject as before"
            );
            assert!(
                composed.text.contains("canonical common instructions"),
                "{label}: bytes must be present"
            );
        }
    }

    /// Codex's native file is `AGENTS.md`, and the two must never cross: a
    /// matching CLAUDE.md says nothing about what a codex session read.
    #[test]
    fn the_dedupe_checks_each_harnesss_own_native_file_only() {
        let repo = repo_with_context_files(&[("common.md", "canonical common instructions\n")]);
        let home = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(home.path().join("state"));
        std::fs::write(
            repo.path().join("CLAUDE.md"),
            crate::commands::ctx::context_cli::render_generated(
                Some("canonical common instructions\n"),
                None,
            ),
        )
        .expect("write");

        let codex = compile(
            Some(home.path()),
            repo.path(),
            false,
            &CtxConfig::default(),
            &CodexAdapter::new(None),
            PromptRole::Orchestrator,
            &state,
            now_secs(),
            LaunchMode::Interactive,
            false,
        );
        assert!(
            codex
                .composed
                .expect("composed")
                .sources
                .contains(&PromptSource::Context),
            "a matching CLAUDE.md must not suppress codex's own injection"
        );
    }

    /// The operator's off switch, and the repo layer's one allowed direction.
    #[test]
    fn dedupe_native_false_always_injects() {
        let repo = repo_with_context_files(&[("common.md", "canonical common instructions\n")]);
        let home = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(home.path().join("state"));
        std::fs::write(
            repo.path().join("CLAUDE.md"),
            crate::commands::ctx::context_cli::render_generated(
                Some("canonical common instructions\n"),
                None,
            ),
        )
        .expect("write");
        let mut cfg = CtxConfig::default();
        cfg.context.dedupe_native = false;

        let compiled = compile(
            Some(home.path()),
            repo.path(),
            false,
            &cfg,
            &ClaudeAdapter::new(None),
            PromptRole::Orchestrator,
            &state,
            now_secs(),
            LaunchMode::Interactive,
            false,
        );
        assert!(
            compiled
                .composed
                .expect("composed")
                .sources
                .contains(&PromptSource::Context)
        );
    }
```

Add to `mod tests` in `src/commands/ctx/config.rs`:

```rust
    /// `context.dedupe_native` is deliberately NOT `REPO_FORBIDDEN`, unlike
    /// the byte caps beside it: a repo layer can only ever set it `false`,
    /// which causes MORE context to be injected -- narrowing, the direction
    /// this trust model allows. A repo layer's `true` must not be able to
    /// SUPPRESS an operator's own `false`.
    #[test]
    fn a_repo_layer_may_disable_native_dedupe_but_never_re_enable_it() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv").join(CTX_CONFIG_FILE),
            "[context]\ndedupe_native = false\n",
        )
        .expect("write home layer");

        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv").join(CTX_CONFIG_FILE),
            "[context]\ndedupe_native = true\n",
        )
        .expect("write repo layer");

        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");
        assert!(
            !cfg.context.dedupe_native,
            "the operator's own false must survive a repo layer's true"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin zirv compile::tests::a_matching_native_file config::tests::a_repo_layer_may_disable -- --test-threads=1`
Expected: FAIL to compile — `no field dedupe_native on type ContextConfig`.

- [ ] **Step 3: Write minimal implementation**

`src/commands/ctx/config.rs` — add to `ContextConfig` (and `true` in its `Default`):

```rust
    /// Issue #155, Phase 3: skip injecting the canonical `.zirv/context/`
    /// layer when the harness's own native instruction file
    /// (`<repo>/CLAUDE.md` for claude, `<repo>/AGENTS.md` for codex) is a
    /// zirv-managed render that PROVABLY holds the current canonical bytes
    /// -- proven by the hash `context_cli::render_generated` stamps into it,
    /// never assumed. The harness reads that file natively at session start,
    /// so injecting the same ~8 KiB again is pure duplication in the single
    /// most cacheable layer there is.
    ///
    /// Deliberately NOT `REPO_FORBIDDEN`, unlike the byte caps above it: a
    /// repo layer can only ever set it `false`, and `false` injects MORE
    /// context, which is narrowing. `CtxConfig::load` folds it the same way
    /// `pace.enabled` folds (the stricter layer wins), so a repo `true`
    /// cannot re-enable a skip the operator turned off.
    pub dedupe_native: bool,
```

Fold it in `CtxConfig::load` exactly like `pace.enabled`: lift `context.dedupe_native` out of both the home and repo layers before the deep merge (`take_nested(&mut merged, "context", "dedupe_native")` / same on `repo_layer`), then re-insert `narrow_pace_bool(home.unwrap_or(default), repo)` — `narrow_pace_bool`'s existing semantics are "the stricter bool wins", and here `false` (inject more) is the stricter one, so pass the values in the order that yields that. Add a comment stating which direction is strict here, because it is the opposite polarity from `pace.enabled`. Add `("context", "dedupe_native")` to `ALL_CONFIG_KEYS` and a commented line to `.zirv/ctx.toml`'s `[context]` section.

`src/commands/ctx/compile.rs`:

```rust
/// `log::Decision::action` for a canonical context layer skipped because the
/// harness's own native file already carries those exact bytes.
pub const DEDUP_SKIP_ACTION: &str = "context-dedup-skip";

/// The harness's own native instruction file for `adapter_name` -- the file
/// that harness reads by itself, with no zirv involvement. `None` for an
/// adapter with no such file, which then always injects.
fn native_context_path(adapter_name: &str, repo: &Path) -> Option<PathBuf> {
    match adapter_name {
        "claude" => Some(repo.join("CLAUDE.md")),
        "codex" => Some(repo.join("AGENTS.md")),
        _ => None,
    }
}

/// Whether `adapter_name`'s native file PROVES it already holds the current
/// canonical content: it exists, it is zirv-managed, it carries an embedded
/// canonical hash, and that hash equals the hash of what
/// `with_canonical_context_layer` would inject right now.
///
/// Every other outcome -- absent, unreadable, hand-written, generated by an
/// older zirv with no hash line, or stamped with a stale hash -- is `false`,
/// and `false` means "inject exactly as before". The dedupe is an
/// optimisation over a PROVEN-identical byte sequence, never a guess: a
/// wrong `true` here silently strips instructions from a session, which is
/// the one failure this phase must not introduce.
fn native_file_already_carries_canonical(
    adapter_name: &str,
    repo: &Path,
    cfg: &CtxConfig,
) -> bool {
    let Some(native) = native_context_path(adapter_name, repo) else {
        return false;
    };
    let Ok(native_text) = std::fs::read_to_string(&native) else {
        return false;
    };
    if !super::context_cli::is_managed(&native_text) {
        return false;
    }
    let Some(embedded) = super::context_cli::embedded_canonical_sha256(&native_text) else {
        return false;
    };
    let common = std::fs::read_to_string(context::common_path(repo)).ok();
    let harness = harness_context_layer(adapter_name, repo)
        .and_then(|(_, path)| std::fs::read_to_string(path).ok());
    // A layer that WOULD be truncated is not the same bytes the native file
    // holds -- `run_generate` writes the untruncated text. Never dedupe
    // against a file that carries more than the injection would have.
    if common.as_deref().is_some_and(|t| t.len() > cfg.context.max_common_bytes)
        || harness.as_deref().is_some_and(|t| t.len() > cfg.context.max_harness_bytes)
    {
        return false;
    }
    embedded == super::context_cli::canonical_sha256(common.as_deref(), harness.as_deref())
}
```

Wire it at the top of `with_canonical_context_layer`'s candidate loop: build `provenance` for every candidate exactly as today, but when `cfg.context.dedupe_native && native_file_already_carries_canonical(adapter_name, repo, cfg)`, push each `ContextProvenance` with `delivered_bytes: 0` and `truncated: false`, append nothing to `composed.text`, and do not push `PromptSource::Context`. Emit one `log::Decision` with `action = DEDUP_SKIP_ACTION` and a `detail` naming the native file and the bytes skipped — gated on the same `log_truncation` flag Task 1.1 introduced, for the same reason (a read-only report must not write decisions). That means `with_canonical_context_layer` gains `state: Option<&StateDir>` and `now: u64` parameters, passed `Some(state)`/`now` from `compile_with_harness_roster` when `log_truncation` is set and `None`/`now` otherwise.

Note that `native_file_already_carries_canonical` must hash the SAME inputs `render_generated` was given: the raw file contents of `context::common_path(repo)` and of `harness_context_layer(adapter_name, repo)`'s path, **before** any `max_common_bytes`/`max_harness_bytes` truncation — because that is what `run_generate` passes. A truncated injection and a full native file are not the same bytes, so if either layer would truncate, the dedupe must not fire: check `raw_bytes <= cap` for both candidates before returning `true`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin zirv compile:: config:: context_status:: context_cli:: -- --test-threads=1`
Expected: PASS.

Then measure on this repo:

```bash
zirv context sync --generate
zirv context status          # canonical surfaces now report 0 delivered bytes for claude
```

- [ ] **Step 5: Commit**

```bash
git add src/commands/ctx/compile.rs src/commands/ctx/config.rs .zirv/ctx.toml
git commit -m "perf(ctx): skip canonical injection the harness already read natively"
```

---

### Task 3.3: Version bump and vault updates for Phase 3

**Files:**
- Modify: `Cargo.toml` (`2.33.0`), `Cargo.lock`
- Modify: `docs/obsidian/Modules/Ctx Subsystem.md`, `docs/obsidian/Concepts/Untrusted Configuration.md`, `README.md`, `docs/obsidian/Development/{Decision Log,Work Journal,Active Work}.md`

- [ ] **Step 1: Bump to `2.33.0`**, `cargo build`, `rg '2\.32\.0' src/ docs/`.

- [ ] **Step 2: Update the vault and the trust tables**
- `Modules/Ctx Subsystem.md` — the `<!-- zirv:canonical-sha256:… -->` provenance line, the dedupe rule and its four fallback cases, the `context-dedup-skip` decision action.
- `Concepts/Untrusted Configuration.md` + `README.md` — a row for `context.dedupe_native` explaining that it is NOT `REPO_FORBIDDEN` because a repo layer can only narrow it (a repo `false` injects more, a repo `true` is ignored). `config.rs` has a test asserting every `REPO_FORBIDDEN` key has a row in both tables; this key is not one, so state explicitly in the row why it is listed anyway.
- `Development/Decision Log.md` — why a hash rather than a byte comparison (the native file also carries the managed marker and the hash line itself, so it is never byte-identical to the injected layer), and why a truncating layer disables the dedupe.
- `Development/{Work Journal,Active Work}.md`.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock docs README.md
git commit -m "chore: bump to 2.33.0 and document canonical-context dedupe"
```

---

### Task 3.4: Phase 3 verification gates, cross-review, and PR

- [ ] **Step 1: Capture the `main` baseline** (as Task 1.5 Step 1).
- [ ] **Step 2: Run all five gates in the FOREGROUND**, diff failure NAMES.
- [ ] **Step 3: Measure**

```bash
zirv context sync --generate
zirv context status
zirv ctx usage --sessions     # compare the cache-hit ratio against the Phase 2 baseline
```

Record: bytes of canonical context no longer injected per claude session (expected ≈ 8 KiB on this repo), the count of `context-dedup-skip` decisions over a working session, and the cache-hit ratio before/after.

- [ ] **Step 4: Codex cross-review**

```bash
zirv agent codex "Review the diff on branch feat/token-cost-p3-context-dedupe against main. The safety property is: a session must NEVER silently lose canonical instructions. Find any path where native_file_already_carries_canonical returns true but the native file does not actually hold the current canonical bytes -- consider truncation caps, an all-whitespace harness file, a symlinked or case-differing path on Windows, and a CLAUDE.md regenerated from a different repo root. Reply with confirmed, concrete findings only." -- --model gpt-5-codex
```

- [ ] **Step 5: Open the PR** with the byte and cache-ratio numbers in the body.

**Phase 3 acceptance criteria (all measurable):**
- With `CLAUDE.md` freshly generated, a claude launch skips the canonical layer entirely (≈8 KiB on this repo) and logs exactly one `context-dedup-skip`.
- Editing `.zirv/context/common.md` without regenerating restores full injection on the very next compose — asserted by test, not by inspection.
- A matching `CLAUDE.md` never suppresses a codex session's injection, and vice versa.
- `context.dedupe_native = false` in a home layer survives a repo layer's `true`.

---

# Phase 4 — review-train convergence (PR 4, version `2.34.0`, branch `feat/token-cost-p4-review-convergence`)

Spec section: "Phase 4 — review-train convergence". Three sources independently demand a review round and none knows about the others; every reviewer in every round receives the FULL diff against the workflow's fixed `base_sha` (capped at 96 KiB ≈ 24k tokens); and the "stop when a round yields no new findings" rule exists only as prose in `HARNESS_PROMPT`.

### Task 4.1: Name the single review gate in both prompt layers

**Files:**
- Modify: `src/commands/ctx/prompt.rs` (`HARNESS_PROMPT` at ~133 — the final bullet, and the `(v9)` version token on its first line)
- Modify: `src/commands/ctx/adapters/claude.rs` (`ORCHESTRATOR_PROMPT` at ~31 — the final bullet)
- Test: inline `#[cfg(test)] mod tests` in `src/commands/ctx/prompt.rs` and `src/commands/ctx/adapters/claude.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: revised `HARNESS_PROMPT` (version token `(v10)`) and `ORCHESTRATOR_PROMPT` text.

- [ ] **Step 1: Write the failing test**

```rust
    /// Issue #155, Phase 4(a): three sources independently demanded a review
    /// round -- this layer, the claude adapter's orchestrator layer, and the
    /// workflow engine's risk-based reviewer count -- and the claude layer
    /// explicitly stacked itself ON TOP of this one. A Medium-risk change was
    /// therefore reviewed three times over the same full diff. Where a
    /// `zirv workflow` gate is active, it is the single source of truth.
    #[test]
    fn the_harness_layer_defers_to_an_active_workflow_review_gate() {
        assert!(HARNESS_PROMPT.contains("zirv workflow"), "must name the gate");
        assert!(
            HARNESS_PROMPT.contains("single source of truth"),
            "must say which one wins"
        );
        assert!(
            HARNESS_PROMPT.contains("(v10)"),
            "a changed instruction layer must bump its own version token"
        );
    }
```

and in `adapters/claude.rs`:

```rust
    /// The specific sentence being corrected: the orchestrator layer used to
    /// say a session carrying the zirv meta-harness layer follows that
    /// layer's cross-harness review round "on top" of its own /code-review.
    /// That instruction is what turned one change into three review rounds.
    #[test]
    fn the_orchestrator_layer_no_longer_stacks_a_review_round_on_top() {
        assert!(
            !ORCHESTRATOR_PROMPT.contains("on top"),
            "the stacking instruction must be gone"
        );
        assert!(
            ORCHESTRATOR_PROMPT.contains("zirv workflow"),
            "and must instead defer to the workflow gate when one is active"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin zirv prompt::tests::the_harness_layer_defers adapters::claude::tests::the_orchestrator_layer_no_longer -- --test-threads=1`
Expected: FAIL — `must name the gate`.

- [ ] **Step 3: Write minimal implementation**

Bump `HARNESS_PROMPT`'s first line to `zirv meta-harness (v10)` and replace its final bullet's opening with:

> Finish every substantive development task with ONE review round, and one only. **If a `zirv workflow` review gate is active for this change, that gate is the single source of truth: do not run an additional native or cross-harness round on top of it — `zirv workflow review run` is the round.** Otherwise: this harness's own native full-diff review, plus one review worker per other enabled harness via `zirv agent`, each given a self-contained brief naming the diff and asking for confirmed, concrete findings — for a substantive or risky diff only; a small mechanical diff gets the native pass alone. […rest of the existing bullet unchanged: capacity-limited harnesses, triage, re-review only what the fixes touched, stop as soon as a round yields no new confirmed findings, hard-stop after 2 fix rounds…]

Replace `ORCHESTRATOR_PROMPT`'s final sentence ("A session that also carries the zirv meta-harness layer follows that layer's cross-harness review round on top.") with:

> If a `zirv workflow` review gate is active for this change, that gate is the single source of truth and this native review does not run at all: `zirv workflow review run` already routes an independent reviewer at the depth this change's risk band requires.

Both edits must keep every other constraint in place, in particular the "route to the review model named in the harness roster, never this seat's own model, and never a high-or-above fan-out" rule — that is a separate cost control and this task must not weaken it.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin zirv prompt:: adapters::claude:: -- --test-threads=1`
Expected: PASS. Byte-size assertions on these constants, if any exist, will need their numbers updated — check with `rg 'HARNESS_PROMPT.len\(\)|ORCHESTRATOR_PROMPT.len\(\)' src/`.

- [ ] **Step 5: Commit**

```bash
git checkout -b feat/token-cost-p4-review-convergence
git add src/commands/ctx/prompt.rs src/commands/ctx/adapters/claude.rs
git commit -m "feat(prompt): make an active workflow review gate the single review source of truth"
```

---

### Task 4.2: Re-review the delta, not the whole diff again

**Files:**
- Modify: `src/commands/workflow/review.rs` (`ReviewRunEvidence` at ~122, `ReviewPackage` at ~168, `package` at ~560, the evidence push at ~1039)
- Test: inline `#[cfg(test)] mod tests` in `src/commands/workflow/review.rs`

**Interfaces:**
- Consumes: `git_diff_capped(repo, base_sha)`, `git(repo, args)`, `verification::change_fingerprint`, `review_round(state, fingerprint)` (all existing).
- Produces:
  - `ReviewRunEvidence` gains `#[serde(default)] pub head_sha: Option<String>`
  - `ReviewPackage` gains `pub diff_base_sha: String` and `pub diff_is_delta: bool`
  - `fn review::delta_base(state: &WorkflowState, repo: &Path, review_round: u8) -> Option<String>`

- [ ] **Step 1: Write the failing test**

```rust
    /// Issue #155, Phase 4(b): round 2 of a fix loop re-sent every byte round
    /// 1 already sent -- the full diff against the workflow's fixed base_sha,
    /// to every reviewer, every round, capped at 96 KiB (~24k tokens). Round
    /// 2 onward diffs from the sha the LAST reviewer actually reviewed.
    #[test]
    fn a_later_round_diffs_from_the_last_reviewed_sha_not_the_workflow_base() {
        let repo = git_repo_with_commits(&["base", "first change", "fix after review"]);
        let shas = git_log_shas(repo.path());   // oldest first
        let mut state = running_review_state(repo.path(), &shas[0]);
        state.review_evidence.push(ReviewRunEvidence {
            id: "ev-1".to_string(),
            change_fingerprint: 1,
            adapter: "codex".to_string(),
            review_round: 1,
            completed_at: 10,
            head_sha: Some(shas[1].clone()),
        });

        let base = delta_base(&state, repo.path(), 2).expect("a delta base for round 2");
        assert_eq!(base, shas[1], "the sha round 1 actually reviewed");
        assert_eq!(
            delta_base(&state, repo.path(), 1),
            None,
            "round 1 has nothing to delta against and must send the full diff"
        );
    }

    /// Every way the chain can break must fall back to the FULL diff. A
    /// reviewer that silently receives less than it needs is a worse outcome
    /// than an expensive review.
    #[test]
    fn a_broken_evidence_chain_falls_back_to_the_full_diff() {
        let repo = git_repo_with_commits(&["base", "first change"]);
        let shas = git_log_shas(repo.path());
        let base_state = running_review_state(repo.path(), &shas[0]);

        // (1) evidence with no recorded sha -- written by an older zirv.
        let mut no_sha = base_state.clone();
        no_sha.review_evidence.push(ReviewRunEvidence {
            id: "ev-1".to_string(),
            change_fingerprint: 1,
            adapter: "codex".to_string(),
            review_round: 1,
            completed_at: 10,
            head_sha: None,
        });
        assert_eq!(delta_base(&no_sha, repo.path(), 2), None);

        // (2) a recorded sha that no longer resolves -- a rebase or a reset.
        let mut gone = base_state.clone();
        gone.review_evidence.push(ReviewRunEvidence {
            id: "ev-1".to_string(),
            change_fingerprint: 1,
            adapter: "codex".to_string(),
            review_round: 1,
            completed_at: 10,
            head_sha: Some("0".repeat(40)),
        });
        assert_eq!(delta_base(&gone, repo.path(), 2), None);

        // (3) no evidence at all.
        assert_eq!(delta_base(&base_state, repo.path(), 2), None);
    }

    /// The package states plainly which diff a reviewer is holding. A
    /// reviewer told "this is the whole change" when it is a delta will
    /// report false findings about code it cannot see.
    #[test]
    fn the_package_declares_whether_its_diff_is_a_delta() {
        let repo = git_repo_with_commits(&["base", "first change"]);
        let shas = git_log_shas(repo.path());
        let state = running_review_state(repo.path(), &shas[0]);
        let state_dir = StateDir::from_root(
            tempfile::tempdir().expect("tempdir").path().to_path_buf(),
        );

        let package = package(&state_dir, &state, Some(&shas[0])).expect("package");
        assert!(!package.diff_is_delta, "round 1 is never a delta");
        assert_eq!(package.diff_base_sha, package.base_sha);
        assert_eq!(package.head_sha.len(), 40);
    }
```

(`git_repo_with_commits` and `git_log_shas` are new local test helpers in this `mod tests`; `running_review_state` builds a `WorkflowState` at `WorkflowStatus::Running` on a `WorkflowPhase::Review` step with `repo` and the given base — reuse whatever the existing review tests already build for the same purpose rather than adding a second builder.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin zirv workflow::review::tests::a_later_round_diffs -- --test-threads=1`
Expected: FAIL to compile — `struct ReviewRunEvidence has no field named head_sha`.

- [ ] **Step 3: Write minimal implementation**

Add `#[serde(default)] pub head_sha: Option<String>` to `ReviewRunEvidence` (`None` for evidence written before this change, which `delta_base` reads as a broken chain), and set it from `package.head_sha` at the push site in `run_independent_review`.

```rust
/// The sha a later review round should diff FROM: the HEAD the most recent
/// completed reviewer actually reviewed.
///
/// `None` -- meaning "send the full diff against the workflow's base_sha,
/// exactly as before" -- whenever the chain cannot be proven intact: round 1,
/// no evidence at all, evidence written before `head_sha` existed, or a
/// recorded sha that no longer resolves in this repository (a rebase, a
/// reset, a fresh clone). A reviewer that silently receives LESS than the
/// change it is judging is a worse outcome than an expensive review, so
/// every ambiguous case falls back.
fn delta_base(state: &WorkflowState, repo: &Path, review_round: u8) -> Option<String> {
    if review_round <= 1 {
        return None;
    }
    let sha = state
        .review_evidence
        .iter()
        .max_by_key(|evidence| (evidence.review_round, evidence.completed_at))?
        .head_sha
        .clone()?;
    // Must still resolve to a commit in THIS repository, or the diff below
    // would fail outright rather than degrade.
    git(repo, &["rev-parse", "--verify", "--end-of-options", &format!("{sha}^{{commit}}")]).ok()?;
    Some(sha)
}
```

In `package`, after `review_round` is computed and its `MAX_FIX_REVIEW_ROUNDS` guard has run, choose the diff base:

```rust
    let diff_base_sha = delta_base(state, &state.repo, review_round).unwrap_or_else(|| base_sha.clone());
    let diff_is_delta = diff_base_sha != base_sha;
    let (mut diff, mut diff_truncated) = git_diff_capped(&state.repo, &diff_base_sha)?;
```

`changed_paths` keeps being computed against **`base_sha`**, not `diff_base_sha`: the reviewer needs the full set of files this change touches for context even when the diff it holds is a delta. Untracked-file bodies are appended exactly as today. Add `diff_base_sha` and `diff_is_delta` to `ReviewPackage` and populate them.

Finally, the reviewer must be told. Wherever the package is rendered into the reviewer's brief, a `diff_is_delta` package must state: this diff covers only what changed since the previously reviewed commit `<diff_base_sha>`; the full set of files this change touches is `changed_paths`; the findings already recorded are in `existing_findings`. Do not let a delta package read as a whole change.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin zirv workflow::review:: workflow::engine:: -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/commands/workflow/review.rs
git commit -m "perf(workflow): re-review the delta since the last reviewed sha"
```

---

### Task 4.3: Enforce stop-on-no-new-findings in code

`MAX_FIX_REVIEW_ROUNDS = 3` is enforced. "Stop as soon as a round yields no new confirmed findings" is prose in `HARNESS_PROMPT` and enforced nowhere, so a compliant-looking loop still burns rounds 2 and 3 on a converged change.

**Files:**
- Modify: `src/commands/workflow/review.rs` (`finding_key` at ~85, `append_reviewer_findings` and `run_independent_review` at ~993)
- Test: inline `#[cfg(test)] mod tests` in `src/commands/workflow/review.rs`

**Interfaces:**
- Consumes: `finding_key(&ReviewFinding) -> String` (existing), `ReviewFinding`, `WorkflowState::review_findings`.
- Produces:
  - `pub fn review::new_finding_count(existing: &[ReviewFinding], incoming: &[ReviewFinding]) -> usize`
  - `pub struct review::RoundOutcome { pub new_findings: usize, pub converged: bool }`
  - `run_independent_review` reports convergence on stdout and returns `0` for a converged round

- [ ] **Step 1: Write the failing test**

```rust
    /// Issue #155, Phase 4(c): "stop when a round yields no new findings" was
    /// prompt text only. A converged change still burned rounds 2 and 3 --
    /// each one a full reviewer launch over a 96 KiB diff.
    #[test]
    fn a_round_with_no_new_findings_converges() {
        let existing = vec![finding_at("src/lib.rs", 10, "off-by-one in the loop bound")];
        let repeat = vec![finding_at("src/lib.rs", 10, "off by one in the loop bound!!")];
        assert_eq!(
            new_finding_count(&existing, &repeat),
            0,
            "the same path:line is the same finding, whatever the wording"
        );

        let fresh = vec![finding_at("src/other.rs", 3, "unchecked unwrap")];
        assert_eq!(new_finding_count(&existing, &fresh), 1);
        assert_eq!(new_finding_count(&existing, &[]), 0);
    }

    /// "New" must mean the same thing here as everywhere else in this module,
    /// or the stop rule and the escalation rule will disagree about whether a
    /// finding recurred. Both go through `finding_key`.
    #[test]
    fn convergence_uses_the_same_identity_as_the_escalation_rule() {
        let pathless_a = finding_without_path("the error message is swallowed");
        let pathless_b = finding_without_path("the  error   message is swallowed");
        assert_eq!(
            new_finding_count(std::slice::from_ref(&pathless_a), &[pathless_b]),
            0,
            "finding_key normalises whitespace for a pathless finding"
        );
    }

    /// A converged round is a SUCCESS, not an exhausted budget: it must not
    /// consume the remaining rounds and must not report failure.
    #[test]
    fn a_converged_round_ends_the_loop_successfully_with_rounds_left() {
        let outcome = RoundOutcome { new_findings: 0, converged: true };
        assert!(outcome.converged);
        assert_eq!(
            round_exit_code(&outcome, 0),
            0,
            "convergence with a zero reviewer exit is success"
        );
    }
```

(`finding_at` / `finding_without_path` are small local helpers building a `ReviewFinding` with a fixed severity and `created_at`.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin zirv workflow::review::tests::a_round_with_no_new_findings -- --test-threads=1`
Expected: FAIL to compile — `cannot find function new_finding_count in this scope`.

- [ ] **Step 3: Write minimal implementation**

```rust
/// How many of `incoming` are findings this workflow has not already
/// recorded, by the same `finding_key` identity `has_repeated_meaningful_
/// finding` uses -- path:line where a path exists, whitespace-normalised
/// lowercased summary otherwise. One identity across this module, so the
/// stop rule and the escalation rule can never disagree about whether a
/// finding recurred.
pub fn new_finding_count(existing: &[ReviewFinding], incoming: &[ReviewFinding]) -> usize {
    let seen: BTreeSet<String> = existing.iter().map(finding_key).collect();
    let mut fresh = BTreeSet::new();
    incoming
        .iter()
        .map(finding_key)
        .filter(|key| !seen.contains(key) && fresh.insert(key.clone()))
        .count()
}

/// What one completed review round concluded. `converged` is the code
/// enforcement of the rule `HARNESS_PROMPT` could only ask for: a round that
/// surfaced nothing new ends the loop successfully, whatever budget remains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoundOutcome {
    pub new_findings: usize,
    pub converged: bool,
}
```

In `run_independent_review`, compute `new_finding_count(&state.review_findings, &parsed_findings)` **before** `append_reviewer_findings` merges them, build the `RoundOutcome`, and when `converged` is true and the reviewer exited 0 and the fingerprint was unchanged: write a line saying the round surfaced no findings not already recorded, that the review loop is complete, and how many rounds went unused — then return `0` without demanding another round. `round_exit_code(&outcome, reviewer_code)` is the small pure helper the test pins, so the decision is testable without a launch.

The evidence push, telemetry event and finding merge all still happen: convergence is a *stopping* rule, not a skip. Nothing about `MAX_FIX_REVIEW_ROUNDS`'s hard cap changes — it remains the upper bound for a change that does NOT converge.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin zirv workflow:: -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/commands/workflow/review.rs
git commit -m "feat(workflow): end the review loop when a round yields no new findings"
```

---

### Task 4.4: Version bump and vault updates for Phase 4

**Files:**
- Modify: `Cargo.toml` (`2.34.0`), `Cargo.lock`
- Modify: `docs/obsidian/Modules/Ctx Subsystem.md`, `docs/obsidian/Modules/Built-in Commands.md`, `docs/obsidian/Development/{Decision Log,Work Journal,Active Work}.md`

- [ ] **Step 1: Bump to `2.34.0`**, `cargo build`, `rg '2\.33\.0' src/ docs/`.
- [ ] **Step 2: Update the vault**
- `Modules/Ctx Subsystem.md` — `HARNESS_PROMPT` is `(v10)`; an active workflow gate is the single review source of truth.
- `Modules/Built-in Commands.md` — `zirv workflow review run` now converges instead of always consuming its round budget; a package may be a delta and says so (`diff_base_sha`, `diff_is_delta`).
- `Development/Decision Log.md` — why the claude layer's "on top" stacking was the specific text removed; why `changed_paths` stays against `base_sha` while the diff moves to `diff_base_sha`; why every ambiguous chain state falls back to a full diff.
- `Development/{Work Journal,Active Work}.md`.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock docs/obsidian
git commit -m "chore: bump to 2.34.0 and document review-train convergence"
```

---

### Task 4.5: Phase 4 verification gates, cross-review, and PR

- [ ] **Step 1: Capture the `main` baseline** (as Task 1.5 Step 1).
- [ ] **Step 2: Run all five gates in the FOREGROUND**, diff failure NAMES.
- [ ] **Step 3: Measure** — on a real Medium-risk workflow: independent reviewer launches per change (expected 3 → 1), and total review-diff bytes shipped across a 2-round fix loop (expected: round 2 strictly smaller than round 1). Pull the reviewer counts from `TelemetryKind::ReviewRun` events, which now carry the raw token classes from Phase 2.
- [ ] **Step 4: Codex cross-review**

```bash
zirv agent codex "Review the diff on branch feat/token-cost-p4-review-convergence against main. Focus on: (1) whether a delta package can leave a reviewer unable to judge a finding it is asked about -- check changed_paths, existing_findings and the brief wording; (2) whether delta_base can ever return a sha that is not an ancestor of HEAD, and what git diff does then; (3) whether the convergence rule can end a loop while a Critical finding is still Open. Reply with confirmed, concrete findings only." -- --model gpt-5-codex
```

- [ ] **Step 5: Open the PR** with the reviewer-count and diff-byte numbers in the body.

**Phase 4 acceptance criteria (all measurable):**
- A Medium-risk change runs exactly one independent reviewer, not three.
- Round 2's diff is strictly smaller than round 1's on the same change, and the package declares itself a delta.
- Every broken-chain case (no evidence, no recorded sha, unresolvable sha) falls back to the full diff — asserted by test.
- A round returning only already-recorded findings ends the loop with exit 0 and launches no further reviewer.

---

# Phase 5 — work groups, sub-orchestrators, budgets (PR 5, version `2.35.0`, branch `feat/token-cost-p5-work-groups`)

Spec section: "Phase 5 — work groups, sub-orchestrators, budgets". The largest phase. **Touches `dash`, `agent` and `sessions`, so it needs the Linux/Docker verification pass.**

### Task 5.1: `PromptRole::SubOrchestrator`

`PromptRole` is `{Orchestrator, Worker}`. A worker never gets `HARNESS_PROMPT` or the roster and is told not to delegate onward — correct, and it means an orchestrator cannot decompose a large batch without either doing it itself on the expensive seat, or handing a worker a brief it is forbidden to split.

**Files:**
- Modify: `src/commands/ctx/prompt.rs` (`PromptRole` at ~181, `PromptSource` at ~192, `compose` at ~714, `prompt_file_for_role` at ~766, `role_layer` at ~1234)
- Modify: `src/commands/ctx/adapters/mod.rs` (`AgentAdapter::worker_system_prompt` and its neighbours at ~1040)
- Modify: `src/commands/ctx/adapters/claude.rs` (a `SUB_ORCHESTRATOR_PROMPT` beside `WORKER_PROMPT` at ~72)
- Test: inline `#[cfg(test)] mod tests` in `src/commands/ctx/prompt.rs` and `src/commands/ctx/adapters/claude.rs`

**Interfaces:**
- Consumes: `PromptRole`, `PromptSource::Harness`/`Harnesses`, `AgentAdapter::{base_system_prompt, worker_system_prompt}`.
- Produces:
  - `PromptRole::SubOrchestrator` (third variant)
  - `pub const prompt::SUB_ORCHESTRATOR_PROMPT_FILE: &str = "system-prompt.sub-orchestrator.md";`
  - `pub const adapters::claude::SUB_ORCHESTRATOR_PROMPT: &str`
  - `fn AgentAdapter::sub_orchestrator_system_prompt(&self) -> Option<&'static str>` (default: `self.worker_system_prompt()`)
  - `pub fn PromptRole::may_spawn_workers(self) -> bool` and `pub fn PromptRole::label(self) -> &'static str`

- [ ] **Step 1: Write the failing test**

```rust
    /// Issue #155, Phase 5(a): a third role between Orchestrator and Worker.
    /// It may split a batch and dispatch Workers, so it needs the delegation
    /// vocabulary a Worker is denied -- but it must NOT learn to spawn
    /// further coordinators, because an unbounded delegation tree is exactly
    /// the cost failure this phase exists to bound. The depth cap itself is
    /// enforced at spawn time (Task 5.3); this is only the vocabulary.
    #[test]
    fn a_sub_orchestrator_may_dispatch_workers_but_never_another_coordinator() {
        assert!(PromptRole::Orchestrator.may_spawn_workers());
        assert!(PromptRole::SubOrchestrator.may_spawn_workers());
        assert!(!PromptRole::Worker.may_spawn_workers());
        assert_eq!(PromptRole::SubOrchestrator.label(), "sub-orchestrator");
    }

    /// A sub-orchestrator gets NEITHER of the two orchestrator-only layers:
    /// the full meta-harness teaching, nor the roster of harnesses it could
    /// open a seat on. It coordinates inside a scope it was handed; it does
    /// not decide which harnesses run.
    #[test]
    fn a_sub_orchestrator_gets_neither_orchestrator_only_layer() {
        let repo = tempfile::tempdir().expect("tempdir");
        let composed = compose(
            None,
            repo.path(),
            false,
            &PromptConfig::default(),
            PromptRole::SubOrchestrator,
            &["claude -- ready".to_string()],
            4096,
        )
        .expect("composed");
        assert!(!composed.sources.contains(&PromptSource::Harness));
        assert!(!composed.sources.contains(&PromptSource::Harnesses));
        assert!(!composed.text.contains(HARNESS_PROMPT));
    }

    /// The trimmed coordination layer is real text, materially shorter than
    /// the orchestrator layer -- the whole point is that a coordinator seat
    /// costs less than the seat that spawned it -- and it must never coach
    /// onward coordinator spawning.
    #[test]
    fn the_sub_orchestrator_layer_is_short_and_forbids_spawning_coordinators() {
        assert!(SUB_ORCHESTRATOR_PROMPT.len() < ORCHESTRATOR_PROMPT.len());
        assert!(SUB_ORCHESTRATOR_PROMPT.contains("zirv agent"));
        assert!(
            SUB_ORCHESTRATOR_PROMPT.contains("sub-orchestrator"),
            "must name what it must not spawn"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin zirv prompt::tests::a_sub_orchestrator -- --test-threads=1`
Expected: FAIL to compile — `no variant named SubOrchestrator found for enum PromptRole`.

- [ ] **Step 3: Write minimal implementation**

Add the variant with a doc comment stating the rule, and the two helpers:

```rust
    /// A coordinator handed ONE scope by an Orchestrator. It may split that
    /// scope and dispatch Workers via `zirv agent`; it may not spawn another
    /// coordinator. Total delegation depth is capped at 2 (Orchestrator →
    /// SubOrchestrator → Worker), enforced at spawn time in
    /// `dash::fulfill_spawn_request` -- prompt text that asks nicely is not a
    /// cap. Gets neither `HARNESS_PROMPT` nor the roster: which harnesses run
    /// stays the Orchestrator's decision.
    SubOrchestrator,
```

`compose`'s `if role == PromptRole::Orchestrator` gate for the harness block and the roster stays EXACTLY as written — `SubOrchestrator` falls through it unchanged, which is the behaviour wanted. Every other `match role` in `prompt.rs` and `adapters/mod.rs` must gain an explicit `SubOrchestrator` arm; do not add a catch-all `_`, because the compiler exhaustiveness check is the thing that guarantees a future role cannot silently inherit orchestrator behaviour.

`prompt_file_for_role` maps `SubOrchestrator` to `SUB_ORCHESTRATOR_PROMPT_FILE`; `role_layer` maps it to `adapter.sub_orchestrator_system_prompt()`.

`adapters/mod.rs`:

```rust
    /// The adapter's own layer for a `PromptRole::SubOrchestrator` session.
    /// Defaults to the Worker layer: an adapter with nothing coordinator-
    /// specific to say should say the safer thing, not the more permissive
    /// one.
    fn sub_orchestrator_system_prompt(&self) -> Option<&'static str> {
        self.worker_system_prompt()
    }
```

`adapters/claude.rs` — `SUB_ORCHESTRATOR_PROMPT`, deliberately short. It must say: you coordinate ONE scope handed to you; split it and dispatch Workers with `zirv agent <name> "<prompt>" -- --model <m>`, always naming the cheapest model that can do the task; you must NOT spawn another sub-orchestrator or a dashboard coordinator; keep your own replies to decisions and outcomes, not implementation; report your scope's result back plainly when every child is done. It must NOT carry the orchestrator layer's review-round rules — the Orchestrator owns review gates (Phase 4).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin zirv prompt:: adapters:: compile:: -- --test-threads=1`
Expected: PASS. `cargo build` will surface every non-exhaustive `match role` — fix each with an explicit arm.

- [ ] **Step 5: Commit**

```bash
git checkout -b feat/token-cost-p5-work-groups
git add src/commands/ctx/prompt.rs src/commands/ctx/adapters
git commit -m "feat(prompt): add PromptRole::SubOrchestrator with a trimmed coordination layer"
```

---

### Task 5.2: `WorkGroup` state and `zirv ctx group create|status|close`

**Files:**
- Create: `src/commands/ctx/group.rs`
- Modify: `src/commands/ctx/mod.rs` (`mod` declaration, `CtxVerb` at ~348, the dispatch `match` at ~460)
- Modify: `src/commands/ctx/state.rs` (`StateDir` at ~267 — a `groups()` directory beside `sessions()`)
- Test: inline `#[cfg(test)] mod tests` in `src/commands/ctx/group.rs`

**Interfaces:**
- Consumes: `state::{StateDir, create_private_dir_all, write_private, now_secs, repo_slug}`, `uuid::Uuid`, `serde`.
- Produces:
  - `pub fn StateDir::groups(&self) -> PathBuf` (`<state>/groups`)
  - `pub struct group::WorkGroup { work_group_id, parent_session_id, scope, child_limit, token_budget, deadline_secs, completion_contract, created_at, closed_at }`
  - `pub fn group::create(state, &WorkGroup) -> CtxResult<()>`, `pub fn group::load(state, id) -> CtxResult<Option<WorkGroup>>`, `pub fn group::list(state) -> Vec<WorkGroup>`, `pub fn group::close(state, id, now) -> CtxResult<()>`
  - `pub struct group::GroupArgs` + `pub enum group::GroupVerb { Create(..), Status(..), Close(..) }` + `pub fn group::run<W: Write>(args: &GroupArgs, w: &mut W) -> CtxResult<i32>`
  - `CtxVerb::Group(group::GroupArgs)`

- [ ] **Step 1: Write the failing test**

```rust
    /// Issue #155, Phase 5(b): a work group is the unit an orchestrator
    /// actually reasons about -- this batch, this budget, this contract --
    /// replacing today's unit, which is "one process that happens to be
    /// alive". Persisted so a child spawned minutes later, in another
    /// process, can still find the terms it was launched under.
    #[test]
    fn a_work_group_round_trips_through_state_and_lists() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let group = WorkGroup {
            work_group_id: "wg-1".to_string(),
            parent_session_id: "sess-parent".to_string(),
            scope: "phase 5 implementation".to_string(),
            child_limit: 3,
            token_budget: Some(400_000),
            deadline_secs: Some(3_600),
            completion_contract: "every child reports a compact result by mail".to_string(),
            created_at: 1_700_000_000,
            closed_at: None,
        };
        create(&state, &group).expect("create");

        assert_eq!(load(&state, "wg-1").expect("load"), Some(group.clone()));
        assert_eq!(list(&state).len(), 1);
        assert_eq!(load(&state, "nope").expect("load"), None, "unknown id is None, not an error");
    }

    /// Closing is idempotent and preserves the terms: a closed group is
    /// evidence of what a batch was launched under, not a tombstone.
    #[test]
    fn closing_a_group_stamps_it_and_stays_idempotent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        create(&state, &sample_group("wg-1")).expect("create");

        close(&state, "wg-1", 1_700_000_500).expect("close");
        let closed = load(&state, "wg-1").expect("load").expect("still present");
        assert_eq!(closed.closed_at, Some(1_700_000_500));
        assert_eq!(closed.scope, "phase 5 implementation");

        close(&state, "wg-1", 1_700_000_900).expect("closing twice is not an error");
        assert_eq!(
            load(&state, "wg-1").expect("load").expect("present").closed_at,
            Some(1_700_000_500),
            "the first close time stands"
        );
    }

    /// A group written by a future zirv with extra fields, or by an older one
    /// with fewer, must not break `list` for every OTHER group.
    #[test]
    fn an_unparsable_group_file_is_skipped_not_fatal() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        create(&state, &sample_group("wg-good")).expect("create");
        std::fs::write(state.groups().join("wg-bad.json"), "{ not json").expect("write");
        assert_eq!(list(&state).len(), 1);
    }

    /// `zirv ctx group create` mints an id and prints it, because that id is
    /// what every `zirv agent --group` invocation must carry.
    #[test]
    fn group_create_prints_the_id_it_minted() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let mut out = Vec::new();
        let id = run_create(
            &state,
            &mut out,
            &CreateArgs {
                scope: "phase 5 implementation".to_string(),
                child_limit: 3,
                token_budget: Some(400_000),
                deadline_secs: None,
                completion_contract: "report by mail".to_string(),
                parent_session: Some("sess-parent".to_string()),
            },
            1_700_000_000,
        )
        .expect("create");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains(&id), "the minted id must be printed: {text}");
        assert!(load(&state, &id).expect("load").is_some());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin zirv group:: -- --test-threads=1`
Expected: FAIL — `unresolved module or unlinked crate group`.

- [ ] **Step 3: Write minimal implementation**

`src/commands/ctx/state.rs`:

```rust
    /// `<state>/groups` -- one JSON file per work group (issue #155). A
    /// sibling of `sessions()`, and deliberately NOT inside it: a session is
    /// a live process, a group outlives every process in it and is the record
    /// of what a batch of delegated work was launched under.
    pub fn groups(&self) -> PathBuf {
        self.0.join("groups")
    }
```

(add `groups()` to whatever `ensure()` creates, matching `sessions()`.)

`src/commands/ctx/group.rs` — `WorkGroup` derives `Debug, Clone, PartialEq, Eq, Serialize, Deserialize` with `#[serde(default)]` on `token_budget`, `deadline_secs` and `closed_at` so an older or newer file still parses. One file per group at `state.groups().join(format!("{id}.json"))` via `create_private_dir_all` + `write_private`. `load` returns `Ok(None)` for a missing file and `Ok(None)` for an unparsable one (with the file left in place — never delete an operator's state to make a read succeed). `list` walks the directory, skipping unparsable entries, sorted by `created_at` descending. `close` is a load-modify-write that leaves an already-set `closed_at` alone.

The CLI: `GroupArgs { #[command(subcommand)] command: GroupVerb }` with

```rust
pub enum GroupVerb {
    /// Open a work group: a scope, a child limit, and the contract every
    /// child must satisfy before the group can close.
    Create(CreateArgs),
    /// Show one group (or all open groups) with its live children.
    Status(StatusArgs),
    /// Close a group. Idempotent.
    Close(CloseArgs),
}
```

`CreateArgs` fields: `scope: String` (positional), `#[arg(long, default_value_t = 3)] child_limit: u32`, `#[arg(long)] token_budget: Option<u64>`, `#[arg(long)] deadline_secs: Option<u64>`, `#[arg(long, default_value = "report a compact structured result by mail to the requesting session")] completion_contract: String`, `#[arg(long)] parent_session: Option<String>`. `run_create` takes `state`, a writer and `now` as parameters (never reading the clock or resolving the state dir itself) so it is testable — the same seam `usage::run_with` already uses.

Register in `src/commands/ctx/mod.rs`: `mod group;`, `Group(group::GroupArgs)` on `CtxVerb` with the doc comment `/// Open, inspect or close a bounded group of delegated work.`, and `CtxVerb::Group(a) => group::run(a, &mut out),` in the dispatch match.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin zirv group:: ctx::tests:: -- --test-threads=1`
Expected: PASS. `zirv ctx --help` must list `group`; the existing `ctx --help` honesty test in `mod.rs` still passes.

- [ ] **Step 5: Commit**

```bash
git add src/commands/ctx/group.rs src/commands/ctx/mod.rs src/commands/ctx/state.rs
git commit -m "feat(ctx): add work groups with zirv ctx group create/status/close"
```

---

### Task 5.3: Spawn-request lineage and the depth cap

**Files:**
- Modify: `src/commands/ctx/dash/spawnreq.rs` (`SpawnRequest` at ~76)
- Modify: `src/commands/ctx/dash/mod.rs` (`fulfill_spawn_request`'s gate block at ~2705, `compose_worker_prompt` at ~2452)
- Modify: `src/commands/ctx/agent.rs` (`try_join_dashboard`'s request construction)
- Test: inline `#[cfg(test)] mod tests` in `src/commands/ctx/dash/spawnreq.rs` and `src/commands/ctx/dash/mod.rs`

**Interfaces:**
- Consumes: `PromptRole` (Task 5.1), `group::load` (Task 5.2), `SpawnRefusal::policy`.
- Produces:
  - `SpawnRequest` gains `#[serde(default)] pub role: Option<String>`, `#[serde(default)] pub parent_session: Option<String>`, `#[serde(default)] pub work_group_id: Option<String>`
  - `pub fn spawnreq::role_of(req: &SpawnRequest) -> PromptRole`
  - `pub fn dash::depth_refusal(parent_role: PromptRole, requested: PromptRole) -> Option<String>`

- [ ] **Step 1: Write the failing test**

```rust
    /// Issue #155, Phase 5(c): lineage travels with the request. Same-binary
    /// IPC on one machine, so `#[serde(default)]` is enough -- a request
    /// written by an older build deserialises to exactly today's behaviour.
    #[test]
    fn a_request_written_before_this_change_still_deserialises_to_todays_behaviour() {
        let old = r#"{"agent":"codex","prompt":"do the thing","cwd":".",
                      "requested_by":"sess-1"}"#;
        let req: SpawnRequest = serde_json::from_str(old).expect("older requests still parse");
        assert_eq!(req.role, None);
        assert_eq!(req.parent_session, None);
        assert_eq!(req.work_group_id, None);
        assert_eq!(
            role_of(&req),
            PromptRole::Worker,
            "an unstated role is a Worker -- the least-privileged reading"
        );
    }

    /// An unrecognised role string is a Worker too. A pane must never gain
    /// coordination privileges from a value nobody validated.
    #[test]
    fn an_unknown_role_string_reads_as_a_worker() {
        let mut req = sample_request();
        req.role = Some("orchestrator-plus".to_string());
        assert_eq!(role_of(&req), PromptRole::Worker);
        req.role = Some("sub-orchestrator".to_string());
        assert_eq!(role_of(&req), PromptRole::SubOrchestrator);
    }
```

and in `dash/mod.rs`:

```rust
    /// Issue #155, Phase 5(a): the depth cap is enforced HERE, at the
    /// authority side, not by prompt text. Orchestrator -> SubOrchestrator ->
    /// Worker is the whole tree; a SubOrchestrator asking for another
    /// coordinator is refused, and a Worker may spawn nothing at all.
    #[test]
    fn the_delegation_depth_cap_is_enforced_at_the_spawn_gate() {
        assert_eq!(
            depth_refusal(PromptRole::Orchestrator, PromptRole::SubOrchestrator),
            None
        );
        assert_eq!(depth_refusal(PromptRole::Orchestrator, PromptRole::Worker), None);
        assert_eq!(depth_refusal(PromptRole::SubOrchestrator, PromptRole::Worker), None);

        let refused = depth_refusal(PromptRole::SubOrchestrator, PromptRole::SubOrchestrator)
            .expect("a sub-orchestrator may not spawn another");
        assert!(refused.contains("depth"), "the reason must say why: {refused}");

        assert!(depth_refusal(PromptRole::Worker, PromptRole::Worker).is_some());
        assert!(
            depth_refusal(PromptRole::SubOrchestrator, PromptRole::Orchestrator).is_some(),
            "nothing may spawn a full Orchestrator seat"
        );
    }

    /// A refused depth is a POLICY refusal, never a retryable one: falling
    /// back to a headless run would route straight around the cap, the same
    /// reasoning the pane cap and the agent gate already apply.
    #[test]
    fn a_depth_refusal_is_not_retryable() {
        let refusal = SpawnRefusal::policy("delegation depth cap reached".to_string());
        assert!(!refusal.retryable());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin zirv dash::spawnreq::tests dash::tests::the_delegation_depth_cap -- --test-threads=1`
Expected: FAIL to compile — `struct SpawnRequest has no field named role`.

- [ ] **Step 3: Write minimal implementation**

`spawnreq.rs` — three fields with doc comments in the style of the existing `model`/`interactive` ones, each stating what an older request deserialises to and why that default is the safe reading:

```rust
    /// What the requester is asking this pane to BE (issue #155): absent, or
    /// unrecognised, means `PromptRole::Worker` -- the least-privileged
    /// reading, and exactly what every pre-2.35.0 request meant. Never
    /// trusted as authority on its own: `fulfill_spawn_request` re-derives
    /// the requesting session's own role and applies the depth cap itself.
    #[serde(default)]
    pub role: Option<String>,
    /// The session that asked for this spawn, for cost attribution and for
    /// the depth cap. `requested_by` already carries an address; this is the
    /// lineage link, and the two are deliberately separate because a
    /// retryable-refusal fallback can change who runs the work without
    /// changing who asked for it.
    #[serde(default)]
    pub parent_session: Option<String>,
    /// The `group::WorkGroup` this spawn belongs to, if any. `None` is a
    /// one-off delegation, which is every delegation before 2.35.0.
    #[serde(default)]
    pub work_group_id: Option<String>,
```

```rust
/// The role a request actually gets. Unstated or unrecognised is
/// `Worker`: a pane must never acquire coordination privileges from a string
/// nobody validated.
pub fn role_of(req: &SpawnRequest) -> PromptRole {
    match req.role.as_deref() {
        Some("sub-orchestrator") => PromptRole::SubOrchestrator,
        _ => PromptRole::Worker,
    }
}
```

`dash/mod.rs`:

```rust
/// Why this spawn is refused on delegation depth, or `None` to allow it.
///
/// The whole permitted tree is Orchestrator -> SubOrchestrator -> Worker.
/// Enforced here, at the authority side, because prompt text that asks a
/// coordinator not to spawn coordinators is a request, not a cap -- and an
/// unbounded delegation tree is precisely the cost failure this phase exists
/// to bound. A refusal is `SpawnRefusal::policy`, never `::channel`: a
/// headless fallback would route straight around the cap.
pub fn depth_refusal(parent_role: PromptRole, requested: PromptRole) -> Option<String> {
    match (parent_role, requested) {
        (_, PromptRole::Orchestrator) => Some(
            "a spawned pane is never a full orchestrator seat".to_string(),
        ),
        (PromptRole::Worker, _) => Some(
            "a worker may not delegate onward (delegation depth cap: 2)".to_string(),
        ),
        (PromptRole::SubOrchestrator, PromptRole::SubOrchestrator) => Some(
            "a sub-orchestrator may not spawn another (delegation depth cap: 2)".to_string(),
        ),
        _ => None,
    }
}
```

Wire it in `fulfill_spawn_request`'s gate block, immediately after the pane cap and before the agent gate: resolve the parent's role from `req.parent_session` (look the session up in the registry; a session zirv does not know is treated as `PromptRole::Orchestrator`, because an unknown parent is an operator invoking `zirv agent` from a plain terminal — the existing, allowed case), then `if let Some(reason) = depth_refusal(parent_role, spawnreq::role_of(req)) { return Err(SpawnRefusal::policy(reason)); }`.

`compose_worker_prompt` passes `spawnreq::role_of(req)` to `compile::compile` in place of its hardcoded `prompt::PromptRole::Worker`.

`agent.rs`'s `try_join_dashboard` populates the three new fields when it writes a request: `role` from a new `--role` flag (Task 5.4), `parent_session` from this process's own session id, `work_group_id` from `--group`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin zirv dash:: agent:: -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/commands/ctx/dash src/commands/ctx/agent.rs
git commit -m "feat(dash): carry delegation lineage on spawn requests and cap the depth at 2"
```

---

### Task 5.4: `zirv agent` budgets that checkpoint, never downgrade

**Files:**
- Modify: `src/commands/ctx/agent.rs` (`AgentArgs` at ~29, `run_with` at ~545, `try_join_dashboard`'s option-compatibility gate at ~300)
- Modify: `src/commands/ctx/exec.rs` (the supervisor poll loop around the `scorer.poll` site at ~1653)
- Test: inline `#[cfg(test)] mod tests` in `src/commands/ctx/agent.rs`

**Interfaces:**
- Consumes: `event::TranscriptUsage` (Task 2.1), `group::load` (Task 5.2), `exec::ExecArgs`, `AgentAdapter::transcript_usage`.
- Produces:
  - `AgentArgs` gains `#[arg(long)] pub group: Option<String>`, `#[arg(long)] pub budget_tokens: Option<u64>`, `#[arg(long)] pub max_tool_calls: Option<u32>`, `#[arg(long)] pub role: Option<String>`
  - `pub struct agent::WorkerBudget { pub tokens: Option<u64>, pub tool_calls: Option<u32> }`
  - `pub enum agent::BudgetState { Ok, SoftWarn { used: u64, limit: u64 }, HardStop { used: u64, limit: u64 } }`
  - `pub fn agent::budget_state(budget: &WorkerBudget, usage: &TranscriptUsage, tool_calls: u32) -> BudgetState`
  - `pub const agent::BUDGET_SOFT_FRACTION: f64 = 0.8;`

- [ ] **Step 1: Write the failing test**

```rust
    /// Issue #155, Phase 5(d): a budget bounds WORK. At 80% the worker is
    /// nudged to wrap up and checkpoint; at 100% it is checkpointed and
    /// stopped with a structured result demand. It is NEVER a signal to
    /// switch models -- a cheaper answer to the wrong question is not a
    /// saving, and automatic downshift is explicitly out of scope.
    #[test]
    fn a_token_budget_warns_at_eighty_percent_and_stops_at_the_limit() {
        let budget = WorkerBudget { tokens: Some(100_000), tool_calls: None };
        let at = |context: u64| TranscriptUsage {
            input_tokens: context,
            ..Default::default()
        };

        assert_eq!(budget_state(&budget, &at(79_999), 0), BudgetState::Ok);
        assert!(matches!(
            budget_state(&budget, &at(80_000), 0),
            BudgetState::SoftWarn { limit: 100_000, .. }
        ));
        assert!(matches!(
            budget_state(&budget, &at(100_000), 0),
            BudgetState::HardStop { .. }
        ));
        assert!(matches!(
            budget_state(&budget, &at(1_000_000), 0),
            BudgetState::HardStop { .. }
        ));
    }

    /// The budget counts what the run actually spends -- every input class
    /// plus output -- not just uncached input, which is near zero in a cached
    /// session and would make the budget never fire.
    #[test]
    fn a_token_budget_counts_every_class_the_run_spends() {
        let budget = WorkerBudget { tokens: Some(100_000), tool_calls: None };
        let cached = TranscriptUsage {
            input_tokens: 1_000,
            cache_creation_input_tokens: 9_000,
            cache_read_input_tokens: 89_000,
            output_tokens: 1_000,
        };
        assert!(
            matches!(budget_state(&budget, &cached, 0), BudgetState::HardStop { .. }),
            "100k spent across four classes is 100k spent"
        );
    }

    /// Tool calls are their own ceiling: a worker can burn a budget in cheap
    /// calls without moving the token count much, and a runaway loop is
    /// exactly what the rot engine's repetition signal already watches for.
    #[test]
    fn a_tool_call_ceiling_is_independent_of_the_token_ceiling() {
        let budget = WorkerBudget { tokens: None, tool_calls: Some(50) };
        let none = TranscriptUsage::default();
        assert_eq!(budget_state(&budget, &none, 39), BudgetState::Ok);
        assert!(matches!(budget_state(&budget, &none, 40), BudgetState::SoftWarn { .. }));
        assert!(matches!(budget_state(&budget, &none, 50), BudgetState::HardStop { .. }));
    }

    /// No budget is no change: every delegation before 2.35.0 ran unbounded
    /// and must continue to.
    #[test]
    fn no_budget_never_warns_and_never_stops() {
        let budget = WorkerBudget { tokens: None, tool_calls: None };
        let huge = TranscriptUsage { input_tokens: u64::MAX, ..Default::default() };
        assert_eq!(budget_state(&budget, &huge, u32::MAX), BudgetState::Ok);
    }

    /// A `--group` supplies defaults the flags may only TIGHTEN. A worker
    /// must not be able to talk its way past the group's own ceiling by
    /// passing a larger `--budget-tokens`.
    #[test]
    fn an_explicit_budget_may_only_tighten_the_groups_own() {
        let group_budget = Some(100_000);
        assert_eq!(resolve_budget_tokens(group_budget, Some(50_000)), Some(50_000));
        assert_eq!(resolve_budget_tokens(group_budget, Some(500_000)), Some(100_000));
        assert_eq!(resolve_budget_tokens(group_budget, None), Some(100_000));
        assert_eq!(resolve_budget_tokens(None, Some(50_000)), Some(50_000));
        assert_eq!(resolve_budget_tokens(None, None), None);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin zirv agent::tests::a_token_budget -- --test-threads=1`
Expected: FAIL to compile — `cannot find struct WorkerBudget in this scope`.

- [ ] **Step 3: Write minimal implementation**

Add the four flags to `AgentArgs` with doc comments; `--role` accepts `worker` (default) or `sub-orchestrator` and nothing else (reject anything else in `validate_flags`'s neighbourhood with a clear message naming the two accepted values).

```rust
/// The soft threshold, as a fraction of a budget. At or above it the worker
/// is nudged to wrap up and checkpoint while it still has room to write a
/// usable result; at the budget itself it is stopped.
pub const BUDGET_SOFT_FRACTION: f64 = 0.8;

/// What a delegated worker is allowed to spend. `None` on a field means no
/// ceiling for it -- which is every delegation before 2.35.0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WorkerBudget {
    pub tokens: Option<u64>,
    pub tool_calls: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetState { Ok, SoftWarn { used: u64, limit: u64 }, HardStop { used: u64, limit: u64 } }

/// Pure: no clock, no filesystem. The worst state across both ceilings wins
/// (HardStop > SoftWarn > Ok), the same "most restrictive answer" fold
/// `safety::evaluate_candidates` uses.
///
/// Spend is `context_total() + output_tokens`, not `input_tokens`: uncached
/// input is near zero in a cached session, so budgeting on it alone would
/// mean the budget effectively never fires.
pub fn budget_state(budget: &WorkerBudget, usage: &TranscriptUsage, tool_calls: u32) -> BudgetState {
    let spent = usage.context_total().saturating_add(usage.output_tokens);
    let mut worst = BudgetState::Ok;
    let mut consider = |used: u64, limit: u64| {
        if limit == 0 {
            return;
        }
        let soft = (limit as f64 * BUDGET_SOFT_FRACTION) as u64;
        let state = if used >= limit {
            BudgetState::HardStop { used, limit }
        } else if used >= soft {
            BudgetState::SoftWarn { used, limit }
        } else {
            BudgetState::Ok
        };
        if rank(state) > rank(worst) {
            worst = state;
        }
    };
    if let Some(limit) = budget.tokens {
        consider(spent, limit);
    }
    if let Some(limit) = budget.tool_calls {
        consider(u64::from(tool_calls), u64::from(limit));
    }
    worst
}

/// HardStop > SoftWarn > Ok, so the worst ceiling wins when both are set.
fn rank(state: BudgetState) -> u8 {
    match state {
        BudgetState::Ok => 0,
        BudgetState::SoftWarn { .. } => 1,
        BudgetState::HardStop { .. } => 2,
    }
}

/// A group's budget is a ceiling its children may only TIGHTEN. An explicit
/// `--budget-tokens` larger than the group's own is clamped, never honoured:
/// a child must not be able to raise the batch's own limit.
pub fn resolve_budget_tokens(group: Option<u64>, explicit: Option<u64>) -> Option<u64> {
    match (group, explicit) {
        (Some(group), Some(explicit)) => Some(group.min(explicit)),
        (Some(group), None) => Some(group),
        (None, explicit) => explicit,
    }
}
```

In `run_with`: resolve `--group` through `group::load` (an unknown group id is a hard error with a message naming `zirv ctx group create`; a CLOSED group is likewise refused), clamp both ceilings against the group's, and hand the resolved `WorkerBudget` to `exec::run_with` via two new `Option` fields on `ExecArgs` (`budget_tokens`, `max_tool_calls`, both `#[serde(default)]`/`Option<T>` so nothing else changes).

In `exec.rs`'s supervisor poll loop, beside the existing `scorer.poll(adapter, score_cfg)` call: read the worker's `transcript_usage` and its tool-call count from the same parsed events the scorer already produced (`NormalizedEvent::ToolCall` occurrences), evaluate `budget_state`, and:
- `SoftWarn` — inject a wrap-up nudge ONCE per run (latch it, exactly as `pace`'s `slow_latched` does): "you are at N% of your token budget; stop starting new work, checkpoint what you have, and report your result now."
- `HardStop` — inject a final structured-result demand, then terminate the run through the existing termination path with a distinct exit code `EXIT_BUDGET_EXHAUSTED` beside `EXIT_ROT_EXHAUSTED`/`EXIT_TIMEOUT`, and add its arm to `exec::describe_exit` and `agent::exit_note`.

Under no circumstances does any of this change `worker_launch_flags`' model resolution. Add a comment saying so at the `HardStop` arm.

`try_join_dashboard`'s option-compatibility gate must also decline a dashboard pane for `--budget-tokens`/`--max-tool-calls`, exactly as it already declines `--max-restarts`/`--timeout-secs`: a pane is not a supervised headless run and cannot honour them, and silently dropping an operator's ceiling is worse than not using the dashboard. `--group` and `--role` DO travel (they are carried in the `SpawnRequest` by Task 5.3), so they must not trigger the decline.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin zirv agent:: exec:: -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/commands/ctx/agent.rs src/commands/ctx/exec.rs
git commit -m "feat(ctx): bound a delegated worker with token and tool-call budgets"
```

---

### Task 5.5: Heavy-**operation** permits replace workload-blind session counting

`sessions::count_heavy_workers_among` counts live records whose verb is `Exec | Dash`. An idle `Exec` worker consumes the whole default budget of 1; a `Chat` orchestrator running a full nextest sweep consumes none. With the default at 1, one parked worker blocks every delegation, so the orchestrator does the work itself on the expensive seat — the exact spend pattern this issue exists to remove.

**Files:**
- Create: `src/commands/ctx/permit.rs`
- Modify: `src/commands/ctx/config.rs` (`SuperviseConfig` at ~100, its `Default`, `ENV_MAP` at ~988, `REPO_FORBIDDEN` at ~1617, `ALL_CONFIG_KEYS` at ~4188, `CtxConfig::load`'s pre-deserialise table rewrite)
- Modify: `src/commands/ctx/sessions.rs` (deprecate `count_heavy_workers`/`count_heavy_workers_among`)
- Modify: `src/commands/ctx/exec.rs` (the gate at ~882), `src/commands/ctx/dash/mod.rs` (the gate at ~2727), `src/commands/ctx/status.rs` (the line at ~321)
- Modify: `src/script_runner/command.rs` (`Command::invoke` at ~92)
- Modify: `.zirv/ctx.toml`, `README.md`, `docs/obsidian/Concepts/Untrusted Configuration.md`
- Test: inline `#[cfg(test)] mod tests` in `src/commands/ctx/permit.rs`, `src/commands/ctx/config.rs`, `src/script_runner/command.rs`

**Interfaces:**
- Consumes: `state::{StateDir, create_private_dir_all, write_private}`, `sessions::pid_is_live` (or whatever liveness probe `sessions::list` already uses — reuse it, do not add a second).
- Produces:
  - `SuperviseConfig::max_heavy_operations: usize` (default 1), with `max_heavy_workers` accepted as a deprecated alias
  - `SuperviseConfig::heavy_command_patterns: Vec<String>` (default: the built-in set)
  - `pub fn permit::is_heavy(command: &str, extra_patterns: &[String]) -> bool` — PURE
  - `pub struct permit::HeavyPermit` (RAII; releases on `Drop`)
  - `pub fn permit::acquire(state: &StateDir, limit: usize, label: &str) -> Option<HeavyPermit>`
  - `pub fn permit::live_count(state: &StateDir) -> usize`

- [ ] **Step 1: Write the failing test**

```rust
    /// Issue #155, Phase 5(e): the budget must count WORK, not sessions. The
    /// old rule counted `Verb::Exec | Verb::Dash` records, so an idle worker
    /// consumed the whole default budget of 1 -- which meant one parked
    /// delegation blocked every subsequent one, and the orchestrator did the
    /// work itself on the expensive seat.
    #[test]
    fn heavy_classification_is_about_the_command_not_the_session() {
        let none: Vec<String> = Vec::new();
        for heavy in [
            "cargo build",
            "cargo build --release",
            "cargo test --verbose -- --test-threads=1",
            "cargo nextest run --no-fail-fast",
            "cargo clippy --all-targets -- -D warnings",
            "cargo package",
            "cargo publish --dry-run",
        ] {
            assert!(is_heavy(heavy, &none), "{heavy} must hold a permit");
        }
        for light in [
            "git status",
            "cargo --version",
            "cargo fmt -- --check",
            "ls",
            "rg TODO src/",
            "echo cargo build",
        ] {
            assert!(!is_heavy(light, &none), "{light} must not hold a permit");
        }
    }

    /// Operator patterns ADD to the built-in set; they never replace it. A
    /// repo layer may only add, which is narrowing.
    #[test]
    fn configured_patterns_extend_the_builtin_set() {
        let extra = vec!["npm run build*".to_string()];
        assert!(is_heavy("npm run build --workspaces", &extra));
        assert!(is_heavy("cargo build", &extra), "built-ins still apply");
        assert!(!is_heavy("npm run lint", &extra));
    }

    /// The permit itself: bounded, released on drop, and never held by an
    /// idle process.
    #[test]
    fn a_permit_is_bounded_and_released_on_drop() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());

        let first = acquire(&state, 1, "cargo build").expect("the first permit is granted");
        assert_eq!(live_count(&state), 1);
        assert!(
            acquire(&state, 1, "cargo nextest run").is_none(),
            "the budget of 1 must refuse a second concurrent heavy operation"
        );

        drop(first);
        assert_eq!(live_count(&state), 0);
        assert!(acquire(&state, 1, "cargo build").is_some(), "the slot is free again");
    }

    /// A permit whose owning process is gone must not wedge the budget
    /// forever -- the same dead-owner sweep `sessions::list` already performs
    /// for session records, and `dash`'s `owner.pid` sweep for request dirs.
    #[test]
    fn a_permit_left_by_a_dead_process_is_swept() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        write_orphan_permit(&state, "cargo build", DEFINITELY_DEAD_PID);
        assert_eq!(live_count(&state), 0, "a dead owner's permit does not count");
        assert!(acquire(&state, 1, "cargo build").is_some());
    }
```

and in `config.rs`:

```rust
    /// Issue #155, Phase 5(e): `supervise.max_heavy_workers` is renamed to
    /// `max_heavy_operations`. The old spelling must still PARSE, not merely
    /// be documented: `CtxConfig`'s structs are `deny_unknown_fields`, an
    /// installed older binary hard-errors on an unknown key, and an
    /// operator's existing `~/.zirv/ctx.toml` has to keep working across the
    /// upgrade in both directions.
    #[test]
    fn the_deprecated_max_heavy_workers_alias_still_sets_the_new_key() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv").join(CTX_CONFIG_FILE),
            "[supervise]\nmax_heavy_workers = 2\n",
        )
        .expect("write");

        let repo = tempfile::tempdir().expect("repo");
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");
        assert_eq!(cfg.supervise.max_heavy_operations, 2);
    }

    /// The new spelling wins when both are present -- an operator mid-
    /// migration must not get the old value silently.
    #[test]
    fn the_new_key_wins_over_the_deprecated_alias() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv").join(CTX_CONFIG_FILE),
            "[supervise]\nmax_heavy_workers = 2\nmax_heavy_operations = 4\n",
        )
        .expect("write");

        let repo = tempfile::tempdir().expect("repo");
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");
        assert_eq!(cfg.supervise.max_heavy_operations, 4);
    }

    /// Both spellings stay `REPO_FORBIDDEN`: a checked-out repo raising the
    /// machine-wide concurrency budget is the exact case issue #133's BSOD
    /// incident created it for, and a renamed key must not become a hole.
    #[test]
    fn neither_spelling_may_come_from_a_repo_layer() {
        for key in ["max_heavy_operations", "max_heavy_workers"] {
            let repo = tempfile::tempdir().expect("repo");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(
                repo.path().join(".zirv").join(CTX_CONFIG_FILE),
                format!("[supervise]\n{key} = 8\n"),
            )
            .expect("write");
            let empty: HashMap<String, String> = HashMap::new();
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repo layer must be rejected");
            assert!(err.to_string().contains(key), "got {err}");
        }
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin zirv permit:: config::tests::the_deprecated_max_heavy_workers -- --test-threads=1`
Expected: FAIL — `unresolved module or unlinked crate permit`.

- [ ] **Step 3: Write minimal implementation**

`src/commands/ctx/permit.rs`:

```rust
//! The machine-wide heavy-OPERATION budget (issue #155, replacing issue
//! #133's heavy-*worker* count).
//!
//! #133's incident was two concurrent cold `cargo build` + full-nextest
//! workloads bugchecking the host four times in twelve minutes. The budget
//! that answered it counted live `Verb::Exec | Verb::Dash` session records,
//! which is workload-blind in both directions: an idle worker sitting at a
//! prompt consumed the whole default budget of 1, while a `Verb::Chat`
//! orchestrator running a full nextest sweep consumed none. With the default
//! at 1, one parked delegation blocked every subsequent one -- so the
//! orchestrator did the work itself on the expensive seat, which is the spend
//! pattern issue #155 exists to remove.
//!
//! A permit is held for the DURATION OF AN ACTUAL HEAVY COMMAND and released
//! when the child exits, so an idle coordinator holds nothing and a busy one
//! holds exactly one.
//!
//! [`is_heavy`] is pure -- no fs, clock or env -- the same discipline
//! `safety::evaluate` holds, so classification is testable without touching
//! the machine. All I/O lives in [`acquire`]/[`live_count`].
//!
//! Cross-process atomicity is deliberately NOT closed here: the count-then-
//! write window is the same TOCTOU today's heavy-worker gate documents, the
//! budget exists to keep concurrency low rather than enforce an exact
//! ceiling, and closing it needs a cross-process lock this state directory
//! has never had.
```

`is_heavy` matches the substituted command against `BUILTIN_HEAVY_PATTERNS` plus `extra_patterns`, reusing `safety::glob_match` and `safety::normalize_segments` so a heavy command hidden behind `sh -c` or a `&&` chain is still classified — one matcher in this codebase, not two. `BUILTIN_HEAVY_PATTERNS` is `["cargo build*", "cargo test*", "cargo nextest*", "cargo clippy*", "cargo package*", "cargo publish*"]`. `cargo fmt` and `cargo --version` are deliberately absent: they are neither long nor resource-hungry, and a permit on them would reintroduce exactly the "idle thing holds the budget" failure.

A permit is one file under `state.root().join("permits")` named `{pid}-{uuid}.json` holding `{ pid, label, acquired_at }`. `live_count` walks the directory, deletes any entry whose `pid` is not live (reusing `sessions`' existing liveness probe — do not add a second), and counts the rest. `acquire` calls `live_count`, returns `None` when it is `>= limit`, otherwise writes its own file and returns a `HeavyPermit` whose `Drop` removes it. `Drop` must be infallible and silent: this binary's release profile is `panic = "abort"`, and a permit that fails to clean up is swept by the next `live_count` anyway.

`src/commands/ctx/config.rs`:

```rust
    /// Issue #155: how many HEAVY OPERATIONS may run concurrently on this
    /// machine -- classified commands (`cargo build`/`test`/`nextest`/
    /// `clippy`/`package`/`publish`, plus `heavy_command_patterns`), each
    /// holding a permit for the duration of the child process. Replaces
    /// `max_heavy_workers`, which counted live `Verb::Exec | Verb::Dash`
    /// session records and so was blind to what those sessions were doing.
    ///
    /// `max_heavy_workers` is still accepted as a DEPRECATED ALIAS, rewritten
    /// onto this key before deserialisation: these structs are
    /// `deny_unknown_fields`, so an operator's existing `~/.zirv/ctx.toml`
    /// would otherwise hard-fail on upgrade. The new key wins when both are
    /// present.
    ///
    /// `REPO_FORBIDDEN` under BOTH spellings, unchanged from #133: a
    /// checked-out repo raising the machine-wide concurrency budget is the
    /// exact case the cap exists for.
    pub max_heavy_operations: usize,
    /// Extra command patterns an operator classifies as heavy on their own
    /// machine, ADDED to the built-in set (`permit::BUILTIN_HEAVY_PATTERNS`),
    /// never replacing it. A repo layer may add entries -- adding is
    /// narrowing -- but the built-ins can never be removed by any layer.
    pub heavy_command_patterns: Vec<String>,
```

The alias rewrite goes in `CtxConfig::load`, in the same pre-deserialise phase as the `[policy]`/`[safety]` lifts and after `reject_untrusted_keys` has run (so a repo layer naming EITHER spelling is still rejected loudly): take `supervise.max_heavy_workers` out of the merged table; if `supervise.max_heavy_operations` is absent, insert the taken value under the new key. Add `(&["supervise", "max_heavy_workers"], "ZIRV_CTX_SUPERVISE_MAX_HEAVY_WORKERS")` **and** `(&["supervise", "max_heavy_operations"], "ZIRV_CTX_SUPERVISE_MAX_HEAVY_OPERATIONS")` to `REPO_FORBIDDEN`, both env vars to `ENV_MAP`, and both keys to `ALL_CONFIG_KEYS` and `.zirv/ctx.toml`. Both need a row in `README.md` and `Concepts/Untrusted Configuration.md`.

Gates and display: `exec.rs`'s and `dash/mod.rs`'s heavy checks stop calling `sessions::count_heavy_workers*` and are DELETED — a session registration is no longer a heavy event at all. `sessions::count_heavy_workers`/`count_heavy_workers_among` are removed along with their tests (the behaviour they encoded is the bug). `status.rs`'s line becomes `heavy operations: {permit::live_count(&state)} of {cfg.supervise.max_heavy_operations} slots in use`.

`src/script_runner/command.rs` — in `Command::invoke`, after the shell is built and before the child is spawned:

```rust
        // Issue #155: hold a machine-wide permit for the duration of a
        // classified heavy command. Best effort by design -- a state
        // directory that cannot be resolved must never stop a script from
        // running -- and the guard's `Drop` releases the slot when the child
        // exits, however it exits.
        let _permit = heavy_permit_for(command);
```

with a small private helper resolving the state dir and config from the process environment, returning `Option<HeavyPermit>`. When a permit is genuinely unavailable (the budget is full), the command WAITS rather than failing: poll `acquire` on a bounded interval up to a generous cap, print one line to stderr saying what it is waiting for, and proceed anyway if the cap is reached. Refuse-not-queue is right for a *spawn* (issue #133's own reasoning) but wrong for a command an operator already typed — failing their `zirv test` because a background build is running would be worse than the wait.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin zirv permit:: config:: sessions:: status:: exec:: dash:: script_runner:: -- --test-threads=1`
Expected: PASS.

Then verify by hand: start `zirv ctx agent claude "sleep quietly"`, confirm `zirv ctx status` reports `heavy operations: 0 of 1 slots in use` while it idles, and that a second delegation is accepted.

- [ ] **Step 5: Commit**

```bash
git add src/commands/ctx/permit.rs src/commands/ctx/config.rs src/commands/ctx/sessions.rs src/commands/ctx/exec.rs src/commands/ctx/dash src/commands/ctx/status.rs src/script_runner/command.rs .zirv/ctx.toml README.md docs
git commit -m "feat(ctx): budget heavy OPERATIONS with permits instead of counting idle sessions"
```

---

### Task 5.6: `zirv ctx status` renders the group tree with per-child spend

**Files:**
- Modify: `src/commands/ctx/status.rs` (the sessions section around ~321)
- Test: inline `#[cfg(test)] mod tests` in `src/commands/ctx/status.rs`

**Interfaces:**
- Consumes: `group::list` (Task 5.2), `log::tail_delegations` (Task 2.3), `sessions::list`.
- Produces: `pub fn status::group_tree_lines(groups: &[group::WorkGroup], delegations: &[log::DelegationRow], records: &[(sessions::Record, sessions::Liveness)]) -> Vec<String>` — PURE, so the rendering is testable without a state directory
  - plus `pub struct log::DelegationRow` (a `Deserialize` mirror of `log::Delegation`, since `Delegation<'a>` is `Serialize`-only)

- [ ] **Step 1: Write the failing test**

```rust
    /// Issue #155, Phase 5(f): an orchestrator can see what its own
    /// delegation tree has cost, per child, in raw classes -- which is the
    /// question "was delegating cheaper than doing it here" reduces to.
    #[test]
    fn the_group_tree_shows_each_child_with_its_own_spend() {
        let groups = vec![sample_group("wg-1", "phase 5 implementation")];
        let delegations = vec![
            delegation_row("wg-1", "sess-a", "codex", 1_000, 91_000, 500),
            delegation_row("wg-1", "sess-b", "claude", 2_000, 40_000, 900),
        ];
        let lines = group_tree_lines(&groups, &delegations, &[]);
        let text = lines.join("\n");

        assert!(text.contains("wg-1"), "got {text}");
        assert!(text.contains("phase 5 implementation"), "the scope: {text}");
        assert!(text.contains("sess-a"), "each child: {text}");
        assert!(text.contains("sess-b"), "each child: {text}");
        assert!(text.contains("codex"), "and which harness ran it: {text}");
        assert!(text.contains("91000"), "raw cache-read spend: {text}");
    }

    /// A delegation with no group is not lost -- it is listed under a plain
    /// "ungrouped" heading, because a one-off delegation is still spend.
    #[test]
    fn ungrouped_delegations_are_still_listed() {
        let delegations = vec![ungrouped_delegation_row("sess-c", "codex")];
        let text = group_tree_lines(&[], &delegations, &[]).join("\n");
        assert!(text.contains("ungrouped"), "got {text}");
        assert!(text.contains("sess-c"), "got {text}");
    }

    /// Nothing to show is nothing shown -- no empty heading on a machine that
    /// has never delegated.
    #[test]
    fn no_groups_and_no_delegations_render_nothing() {
        assert!(group_tree_lines(&[], &[], &[]).is_empty());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin zirv status::tests::the_group_tree -- --test-threads=1`
Expected: FAIL to compile — `cannot find function group_tree_lines in this scope`.

- [ ] **Step 3: Write minimal implementation**

Add `log::DelegationRow` — the same fields as `log::Delegation` but owned `String`s and `#[derive(Deserialize)]` — plus `pub fn log::read_delegations(state: &StateDir, count: usize) -> Vec<DelegationRow>` which tails and parses, skipping unparsable lines.

`group_tree_lines` is pure and does the rendering: one block per open group (`work_group_id`, `scope`, `child_limit`, and the budget/deadline when set), then one indented line per delegation belonging to it — session short id, agent, model, the four raw classes, wall time, outcome — then a per-group total. Delegations with `work_group_id: None` go under a final `ungrouped` heading. A live session record matching a delegation's session marks that child `running`; otherwise it shows its recorded outcome. Empty input renders nothing at all.

Wire it into `status::run_with`'s existing output right after the heavy-operations line, reading `group::list(&state)` and `log::read_delegations(&state, 200)`. Degrade silently on any read failure, exactly as the heavy-operations line already does.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin zirv status:: log:: group:: -- --test-threads=1`
Expected: PASS. Then run `zirv ctx status` after a real grouped delegation.

- [ ] **Step 5: Commit**

```bash
git add src/commands/ctx/status.rs src/commands/ctx/log.rs
git commit -m "feat(ctx): render the work-group tree with per-child token spend in status"
```

---

### Task 5.7: Version bump and vault updates for Phase 5

**Files:**
- Modify: `Cargo.toml` (`2.35.0`), `Cargo.lock`
- Modify: `docs/obsidian/Modules/{Ctx Subsystem,Ctx Supervisors,Built-in Commands,Command Safety}.md`, `docs/obsidian/Concepts/Untrusted Configuration.md`, `README.md`, `docs/obsidian/Development/{Decision Log,Work Journal,Active Work,Known Issues}.md`

- [ ] **Step 1: Bump to `2.35.0`**, `cargo build`, `rg '2\.34\.0' src/ docs/`.
- [ ] **Step 2: Update the vault**
- `Modules/Ctx Supervisors.md` — heavy OPERATION permits replace heavy-worker session counting; `supervise.max_heavy_operations` with the `max_heavy_workers` alias; worker budgets (`--budget-tokens`, `--max-tool-calls`) and the 80%/100% behaviour; `EXIT_BUDGET_EXHAUSTED`.
- `Modules/Ctx Subsystem.md` — `PromptRole::SubOrchestrator`, work groups, the depth cap enforced at the spawn gate, the new `SpawnRequest` fields.
- `Modules/Built-in Commands.md` — `zirv ctx group create|status|close`; `zirv ctx agent --group/--budget-tokens/--max-tool-calls/--role`; the status group tree.
- `Modules/Command Safety.md` — `permit::is_heavy` reuses `safety::glob_match`/`normalize_segments`, so a heavy command behind `sh -c` is still classified.
- `Concepts/Untrusted Configuration.md` + `README.md` — rows for both `supervise` spellings and `supervise.heavy_command_patterns` (add-only from a repo layer).
- `Development/Decision Log.md` — why the depth cap is enforced at the spawn gate rather than by prompt text; why budgets checkpoint instead of downgrading models; why a script command WAITS for a permit while a spawn REFUSES.
- `Development/Known Issues.md` — the permit gate inherits the documented cross-process count-then-write TOCTOU.
- `Development/{Work Journal,Active Work}.md`.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock docs README.md
git commit -m "chore: bump to 2.35.0 and document work groups, budgets and operation permits"
```

---

### Task 5.8: Phase 5 verification gates, Linux/Docker pass, cross-review, and PR

- [ ] **Step 1: Capture the `main` baseline** (as Task 1.5 Step 1).

- [ ] **Step 2: Run all five gates in the FOREGROUND**, diff failure NAMES.

- [ ] **Step 3: Linux/Docker verification — REQUIRED for this phase**

This phase touches `dash`, `agent`, `exec` and adapter-adjacent argv, and `wrap.rs` holds roughly 30 `#[cfg(unix)]` real-PTY tests that never compile or run on Windows.

```bash
git -c core.autocrlf=false archive HEAD -o /tmp/zirv.tar
# In rust:1-bookworm, as a NON-root user, with /tmp/zirv.tar extracted:
cargo test --bin zirv wrap:: -- --test-threads=1
cargo clippy --all-targets -- -D warnings
```

`#[cfg(unix)]` blocks never lint on Windows, so the clippy run there is not optional. Plain `git archive` emits CRLF and corrupts `tests/fixtures/stub-tui.sh` — the `-c core.autocrlf=false` is load-bearing.

- [ ] **Step 4: Measure**

- Delegations accepted while N workers idle, before and after (expected: blocked at N≥1 → unblocked).
- Permits held during a `zirv test` run (expected: exactly 1) and during an idle delegation (expected: 0).
- `zirv ctx status` group tree with per-child spend on a real grouped batch.
- Tokens per completed task for one representative batch, versus the Phase 2 baseline.

- [ ] **Step 5: Codex cross-review**

```bash
zirv agent codex "Review the diff on branch feat/token-cost-p5-work-groups against main. Focus on: (1) whether depth_refusal can be bypassed by a request whose parent_session names a session zirv cannot find; (2) whether a HeavyPermit can leak and permanently wedge the budget -- consider panic=abort, a killed process, and a state dir on a network share; (3) whether the max_heavy_workers alias rewrite runs before or after reject_untrusted_keys, and whether a repo layer can therefore slip either spelling through; (4) whether budget_state can ever stop a worker before it has produced a usable result. Reply with confirmed, concrete findings only." -- --model gpt-5-codex
```

- [ ] **Step 6: Open the PR** with the measurements from Step 4 and an explicit statement of the Docker run's result.

**Phase 5 acceptance criteria (all measurable):**
- With `max_heavy_operations = 1`, three idle delegated workers block no spawn; two concurrent `cargo nextest` runs cannot both hold a permit.
- A spawn requested by a SubOrchestrator parent for another SubOrchestrator is refused with a policy (non-retryable) refusal naming the depth cap.
- `max_heavy_workers = 2` in an existing `~/.zirv/ctx.toml` still parses and still yields `max_heavy_operations == 2`; either spelling from a repo layer is rejected loudly.
- A `--budget-tokens` worker warns at 80% and stops at 100% with `EXIT_BUDGET_EXHAUSTED`, and the model it launched with is unchanged in both cases.
- `zirv ctx status` shows a group with per-child raw token spend.
- The Linux/Docker `wrap::` run and clippy pass.

---

# Phase 6 — model-aware rotation and quota-aware scheduling (PR 6, version `2.36.0`, branch `feat/token-cost-p6-model-aware`)

Spec section: "Phase 6 — model-aware rotation and quota-aware scheduling". **Touches `pace`, so it needs the Linux/Docker verification pass.**

### Task 6.1: Context capacity becomes a capability

`ScoreConfig::token_floor = 100_000` / `token_ceiling = 160_000` are absolute. On a 1M-token seat the ceiling fires at 16% of capacity and restarts a session with 840k tokens of headroom — throwing away a warm cache to rebuild one. No adapter reports capacity at all: `Capabilities` carries `marker_signal`, `token_usage`, `turn_signal`, `system_prompt`, `events`, `defer_injection_submit`, and nothing about size.

**Files:**
- Modify: `src/commands/ctx/event.rs` (`Capabilities` at ~68)
- Modify: `src/commands/ctx/adapters/mod.rs` (`AgentAdapter` trait, near `transcript_usage` at ~1016)
- Modify: `src/commands/ctx/adapters/claude.rs` (`capabilities` at ~1249), `src/commands/ctx/adapters/codex.rs` (`capabilities` at ~1188)
- Test: inline `#[cfg(test)] mod tests` in `src/commands/ctx/event.rs`, `src/commands/ctx/adapters/claude.rs`, `src/commands/ctx/adapters/codex.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces:
  - `Capabilities` gains `pub context_window_tokens: Option<u64>` (stays `Debug + Clone + Copy + PartialEq + Eq + Default`)
  - `fn AgentAdapter::context_window_tokens(&self, model: Option<&str>) -> Option<u64>` (default `None`)
  - `fn AgentAdapter::capabilities_for_model(&self, model: Option<&str>) -> Capabilities` (default: `capabilities()` with the field replaced)
  - `pub const adapters::claude::DEFAULT_CONTEXT_WINDOW_TOKENS: u64 = 200_000;`

- [ ] **Step 1: Write the failing test**

```rust
    /// Issue #155, Phase 6(a): capacity is a CAPABILITY, delivered inside the
    /// struct `rot.rs` already receives. That is what lets rotation
    /// thresholds become ratios of a real window without adding any fs,
    /// clock or env access to a module that must stay pure.
    #[test]
    fn capabilities_default_to_an_unknown_context_window() {
        assert_eq!(Capabilities::default().context_window_tokens, None);
    }
```

and in `adapters/claude.rs`:

```rust
    /// Claude reports a per-model capacity, with a CONSERVATIVE default for a
    /// model id it does not recognise. Conservative on purpose: an
    /// overstated capacity raises the restart ceiling past what the seat can
    /// actually hold, and a session that overruns its window is a far worse
    /// outcome than one rotated slightly early.
    #[test]
    fn claude_reports_a_conservative_context_window_for_an_unknown_model() {
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(
            adapter.context_window_tokens(Some("some-model-zirv-has-never-seen")),
            Some(DEFAULT_CONTEXT_WINDOW_TOKENS)
        );
        assert_eq!(
            adapter.context_window_tokens(None),
            Some(DEFAULT_CONTEXT_WINDOW_TOKENS),
            "an unstated model is the same conservative answer"
        );
        assert_eq!(
            adapter.capabilities().context_window_tokens,
            Some(DEFAULT_CONTEXT_WINDOW_TOKENS),
            "every existing capabilities() caller gets a capacity with no new plumbing"
        );
    }

    /// A recognised long-window model id reports its own capacity, and the
    /// `[1m]` suffix form is recognised too -- that is how a long-window seat
    /// is actually spelled in this environment.
    #[test]
    fn claude_recognises_a_long_window_model_id() {
        let adapter = ClaudeAdapter::new(None);
        let long = adapter
            .context_window_tokens(Some("claude-opus-5[1m]"))
            .expect("a capacity");
        assert!(
            long > DEFAULT_CONTEXT_WINDOW_TOKENS,
            "a 1M seat must not be capped at the conservative default"
        );
        assert_eq!(
            adapter.capabilities_for_model(Some("claude-opus-5[1m]")).context_window_tokens,
            Some(long)
        );
    }
```

and in `adapters/codex.rs`:

```rust
    /// Codex reports NO capacity: none is verified for it, and a guessed
    /// capacity is worse than falling back to the absolute defaults, which
    /// are at least a known quantity. Never fake parity.
    #[test]
    fn codex_reports_no_context_window_because_none_is_verified() {
        assert_eq!(CodexAdapter::new(None).context_window_tokens(None), None);
        assert_eq!(CodexAdapter::new(None).capabilities().context_window_tokens, None);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin zirv event::tests::capabilities_default adapters::claude::tests::claude_reports -- --test-threads=1`
Expected: FAIL to compile — `struct Capabilities has no field named context_window_tokens`.

- [ ] **Step 3: Write minimal implementation**

`event.rs`:

```rust
    /// The model's usable context window, when the adapter can state one
    /// (issue #155). `None` means "unknown", which `rot::token_gates` reads
    /// as "use the absolute `score.token_floor`/`token_ceiling` defaults" --
    /// never as a guess. Delivered inside `Capabilities` deliberately: this
    /// struct is already an input to `rot::score_events` and
    /// `RotState::score`, so capacity reaches the rot engine without adding
    /// a single fs, clock or env read to a module that must stay pure.
    pub context_window_tokens: Option<u64>,
```

`adapters/mod.rs`:

```rust
    /// This adapter's usable context window for `model`, when it can state
    /// one. `None` -- the default -- means no verified capacity, which
    /// leaves rotation on its absolute thresholds. Never guess: an
    /// overstated capacity raises the restart ceiling past what the seat
    /// holds, and overrunning a window is worse than rotating early.
    fn context_window_tokens(&self, _model: Option<&str>) -> Option<u64> {
        None
    }

    /// [`capabilities`](Self::capabilities) with the context window resolved
    /// for a KNOWN model. Callers that have a model string to hand use this;
    /// everything else keeps calling `capabilities()`, which carries the
    /// adapter's own conservative default.
    fn capabilities_for_model(&self, model: Option<&str>) -> Capabilities {
        Capabilities {
            context_window_tokens: self.context_window_tokens(model),
            ..self.capabilities()
        }
    }
```

`adapters/claude.rs` — a `DEFAULT_CONTEXT_WINDOW_TOKENS` of `200_000` and a small ordered match over lowercased model ids. Recognise the long-window form by looking for a `[1m]` / `-1m` marker in the id (that is how a long-window seat is spelled here) and report `1_000_000` for it; everything else, including `None`, is the conservative default. Fill `capabilities()`'s new field with `self.context_window_tokens(None)`.

`adapters/codex.rs` — leave `context_window_tokens` at the trait default and set `context_window_tokens: None` explicitly in its `Capabilities` literal, with a comment stating that no capacity is verified for codex and that a guess would be worse than the absolute fallback.

`cargo build` will surface every `Capabilities { … }` literal that must gain the field; prefer `..Default::default()` in tests and an explicit value in the two adapters.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin zirv event:: adapters:: score:: rot:: -- --test-threads=1`
Expected: PASS — `rot.rs` does not read the field yet, so no verdict changes in this task.

- [ ] **Step 5: Commit**

```bash
git checkout -b feat/token-cost-p6-model-aware
git add src/commands/ctx/event.rs src/commands/ctx/adapters
git commit -m "feat(ctx): let an adapter report its model's context window as a capability"
```

---

### Task 6.2: Rotation thresholds become ratios of real capacity

**Files:**
- Modify: `src/commands/ctx/config.rs` (`ScoreConfig` at ~26 and its `Default`, `ENV_MAP`, `REPO_FORBIDDEN`, `ALL_CONFIG_KEYS`)
- Modify: `src/commands/ctx/rot.rs` (`verdict_for` at ~375, `score_from` at ~405, `score_events` at ~399, `RotState::score` at ~290)
- Modify: `.zirv/ctx.toml`, `README.md`, `docs/obsidian/Concepts/Untrusted Configuration.md`
- Test: inline `#[cfg(test)] mod tests` in `src/commands/ctx/rot.rs`

**Interfaces:**
- Consumes: `Capabilities::context_window_tokens` (Task 6.1).
- Produces:
  - `ScoreConfig` gains `pub token_floor_ratio: f64` (0.5), `pub token_ceiling_ratio: f64` (0.8), `pub model_context_tokens: Option<u64>`, and `token_floor`/`token_ceiling` become `pub Option<u64>` (both default `None`)
  - `pub fn rot::token_gates(cfg: &ScoreConfig, caps: Capabilities) -> (u64, u64)` — PURE
  - `pub fn rot::verdict_for(score: u32, tokens: u64, cfg: &ScoreConfig, caps: Capabilities) -> Verdict`
  - `pub fn rot::score_from(signals: Signals, tokens: u64, cfg: &ScoreConfig, caps: Capabilities) -> Score`
  - `pub const rot::FALLBACK_TOKEN_FLOOR: u64 = 100_000;` / `pub const rot::FALLBACK_TOKEN_CEILING: u64 = 160_000;`

- [ ] **Step 1: Write the failing test**

```rust
    /// Issue #155, Phase 6(b): with no capacity known, the gates are EXACTLY
    /// today's absolute defaults. This is the compatibility floor: codex
    /// reports no capacity, and its rotation behaviour must not move at all.
    #[test]
    fn an_unknown_capacity_keeps_todays_absolute_thresholds() {
        let cfg = ScoreConfig::default();
        let caps = Capabilities::default();
        assert_eq!(token_gates(&cfg, caps), (FALLBACK_TOKEN_FLOOR, FALLBACK_TOKEN_CEILING));
        assert_eq!(token_gates(&cfg, caps), (100_000, 160_000));
    }

    /// A known capacity makes the gates RATIOS of it. On a 1M seat the old
    /// absolute ceiling fired at 16% of capacity and restarted a session with
    /// 840k tokens of headroom -- discarding a warm cache to rebuild one,
    /// which is the most expensive possible response to a size signal.
    #[test]
    fn a_known_capacity_scales_the_gates_to_it() {
        let cfg = ScoreConfig::default();
        let million = Capabilities {
            context_window_tokens: Some(1_000_000),
            ..Default::default()
        };
        assert_eq!(token_gates(&cfg, million), (500_000, 800_000));

        let small = Capabilities {
            context_window_tokens: Some(200_000),
            ..Default::default()
        };
        assert_eq!(token_gates(&cfg, small), (100_000, 160_000), "the shipped ratios reproduce the old absolutes on a 200k seat");
    }

    /// An explicit absolute wins outright: an operator who pins a number gets
    /// that number, capacity or not. Where ordering has to be repaired, the
    /// DERIVED side moves -- zirv never silently rewrites a number the
    /// operator typed.
    #[test]
    fn an_explicit_absolute_overrides_the_ratio() {
        let million = Capabilities {
            context_window_tokens: Some(1_000_000),
            ..Default::default()
        };

        let ceiling_only = ScoreConfig { token_ceiling: Some(900_000), ..ScoreConfig::default() };
        assert_eq!(token_gates(&ceiling_only, million), (500_000, 900_000));

        let floor_only = ScoreConfig { token_floor: Some(120_000), ..ScoreConfig::default() };
        assert_eq!(token_gates(&floor_only, million), (120_000, 800_000));

        let both = ScoreConfig {
            token_floor: Some(10),
            token_ceiling: Some(20),
            ..ScoreConfig::default()
        };
        assert_eq!(token_gates(&both, million), (10, 20), "a fully pinned pair is used verbatim");
    }

    /// The operator's own capacity override beats the adapter's reported
    /// one: an adapter's conservative default is a guess about the seat, and
    /// the operator knows their seat.
    #[test]
    fn the_configured_capacity_overrides_the_adapters_reported_one() {
        let cfg = ScoreConfig { model_context_tokens: Some(1_000_000), ..ScoreConfig::default() };
        let conservative = Capabilities {
            context_window_tokens: Some(200_000),
            ..Default::default()
        };
        assert_eq!(token_gates(&cfg, conservative), (500_000, 800_000));
    }

    /// The gates must never invert or collapse, whatever ratios are
    /// configured: a ceiling at or below the floor would make `verdict_for`'s
    /// two-stage gate meaningless.
    #[test]
    fn the_gates_are_always_ordered_and_nonzero() {
        let inverted = ScoreConfig {
            token_floor_ratio: 0.9,
            token_ceiling_ratio: 0.1,
            ..ScoreConfig::default()
        };
        let caps = Capabilities { context_window_tokens: Some(200_000), ..Default::default() };
        let (floor, ceiling) = token_gates(&inverted, caps);
        assert!(floor < ceiling, "got ({floor}, {ceiling})");

        let zeroed = ScoreConfig {
            token_floor_ratio: 0.0,
            token_ceiling_ratio: 0.0,
            ..ScoreConfig::default()
        };
        let (floor, ceiling) = token_gates(&zeroed, caps);
        assert!(floor > 0 && ceiling > floor, "got ({floor}, {ceiling})");
    }

    /// And the gate still behaves the same way around those thresholds --
    /// this is a change of WHERE the gate sits, never of what it does.
    #[test]
    fn the_verdict_gate_behaves_identically_at_the_scaled_thresholds() {
        let cfg = ScoreConfig::default();
        let million = Capabilities {
            context_window_tokens: Some(1_000_000),
            ..Default::default()
        };
        assert_eq!(verdict_for(100, 499_999, &cfg, million), Verdict::Healthy);
        assert_eq!(verdict_for(100, 500_000, &cfg, million), Verdict::Restart);
        assert_eq!(verdict_for(0, 800_000, &cfg, million), Verdict::Compact);
        assert_eq!(verdict_for(60, 800_000, &cfg, million), Verdict::Restart);
        assert_eq!(verdict_for(59, 850_000, &cfg, million), Verdict::Compact);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin zirv rot::tests::an_unknown_capacity -- --test-threads=1`
Expected: FAIL to compile — `cannot find function token_gates in this scope`.

- [ ] **Step 3: Write minimal implementation**

`config.rs` — change the two fields to `Option<u64>` (both defaulting `None`) and add the three new keys with doc comments explaining the precedence: an explicit absolute wins; otherwise `model_context_tokens` if set, else the adapter's reported capacity, scaled by the ratios; else the absolute fallbacks. All five keys join `REPO_FORBIDDEN` (a repo checkout must not be able to move when the operator's sessions rotate), `ENV_MAP` (`ZIRV_CTX_SCORE_TOKEN_FLOOR_RATIO`, `ZIRV_CTX_SCORE_TOKEN_CEILING_RATIO`, `ZIRV_CTX_SCORE_MODEL_CONTEXT_TOKENS`; `token_floor`/`token_ceiling` keep whatever env vars they already have), `ALL_CONFIG_KEYS`, `.zirv/ctx.toml`, and both trust tables.

`rot.rs`:

```rust
/// The absolute thresholds zirv shipped before capacity was knowable. Still
/// the answer whenever no capacity is available from anywhere -- codex today,
/// and any adapter that cannot honestly state one.
pub const FALLBACK_TOKEN_FLOOR: u64 = 100_000;
pub const FALLBACK_TOKEN_CEILING: u64 = 160_000;

/// The `(floor, ceiling)` this session's token gate actually uses.
///
/// Precedence, per field: an explicit `score.token_floor`/`token_ceiling`
/// wins outright -- an operator who pins a number gets that number. Otherwise
/// the ratio is applied to the resolved capacity: `score.model_context_tokens`
/// if the operator set one (they know their seat; the adapter's default is a
/// guess about it), else `caps.context_window_tokens`. With no capacity from
/// anywhere, the absolute fallbacks apply, unchanged.
///
/// PURE, like everything else in this module: capacity arrives inside
/// `Capabilities`, which `score_events` and `RotState::score` already
/// receive, so no fs, clock, env or net access is added here.
///
/// The result is always ordered and non-zero: a ceiling at or below the floor
/// would make `verdict_for`'s two-stage gate meaningless, and a
/// misconfigured pair of ratios must degrade, never break rotation.
pub fn token_gates(cfg: &ScoreConfig, caps: Capabilities) -> (u64, u64) {
    let capacity = cfg.model_context_tokens.or(caps.context_window_tokens);
    let scaled = |ratio: f64, fallback: u64| -> u64 {
        match capacity {
            Some(capacity) if ratio > 0.0 => {
                ((capacity as f64) * ratio.clamp(0.0, 1.0)).round() as u64
            }
            _ => fallback,
        }
    };
    let mut floor = cfg
        .token_floor
        .unwrap_or_else(|| scaled(cfg.token_floor_ratio, FALLBACK_TOKEN_FLOOR))
        .max(1);
    let mut ceiling = cfg
        .token_ceiling
        .unwrap_or_else(|| scaled(cfg.token_ceiling_ratio, FALLBACK_TOKEN_CEILING));
    // Never inverted, never collapsed: `verdict_for`'s two-stage gate is
    // meaningless if the ceiling is at or below the floor, and a
    // misconfigured pair of ratios must degrade rather than break rotation.
    // Only a DERIVED value is moved -- a number the operator typed is never
    // silently rewritten, so a fully pinned inverted pair stands as written.
    if ceiling <= floor {
        match (cfg.token_floor, cfg.token_ceiling) {
            (None, Some(_)) => floor = ceiling.saturating_sub(1).max(1),
            (Some(_), None) | (None, None) => ceiling = floor.saturating_add(1),
            (Some(_), Some(_)) => {}
        }
    }
    (floor, ceiling)
}
```

Change `verdict_for` to take `caps: Capabilities` and read `let (floor, ceiling) = token_gates(cfg, caps);` in place of `cfg.token_floor`/`cfg.token_ceiling`; its body is otherwise byte-for-byte unchanged. Thread `caps` through `score_from` (which `RotState::score` and `score_events` both already have in hand — neither gains a parameter of its own). `optimize.rs:1238`'s `rot::score_events(&events, caps, cfg)` needs no change.

Every existing `verdict_for(score, tokens, &cfg)` assertion in `rot.rs`'s `mod tests` gains `Capabilities::default()` as its fourth argument, which preserves its current expectation exactly — that is the compatibility proof, so do NOT change any of their expected values.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin zirv rot:: score:: config:: optimize:: -- --test-threads=1`
Expected: PASS. Confirm `rot.rs` still contains no `std::fs`, `std::env`, `SystemTime` or network use — `rg 'std::(fs|env|net)|SystemTime|Instant' src/commands/ctx/rot.rs` must be empty.

- [ ] **Step 5: Commit**

```bash
git add src/commands/ctx/rot.rs src/commands/ctx/config.rs .zirv/ctx.toml README.md docs
git commit -m "feat(rot): scale rotation thresholds to the model's real context window"
```

---

### Task 6.3: Quota pressure gates spawns — never rotation

`rot.rs`/`score.rs` never read `window`/`pace` data, so a session 97% through its five-hour window is scheduled exactly like one at 3%. The fix is on the SCHEDULING side only: zirv must never restart a session because it is expensive, since a restart discards a warm cache and re-reads the whole context.

**Files:**
- Modify: `src/commands/ctx/pace.rs` (near `decide` at ~179, reusing the private `binding`/`worst` helpers)
- Modify: `src/commands/ctx/config.rs` (`PaceConfig` at ~175 and its `Default`, plus `ENV_MAP`/`REPO_FORBIDDEN`/`ALL_CONFIG_KEYS`)
- Modify: `src/commands/ctx/agent.rs` (`run_with`, before the dashboard join attempt)
- Modify: `src/commands/ctx/dash/mod.rs` (`fulfill_spawn_request`'s gate block)
- Modify: `.zirv/ctx.toml`, `README.md`, `docs/obsidian/Concepts/Untrusted Configuration.md`
- Test: inline `#[cfg(test)] mod tests` in `src/commands/ctx/pace.rs` and `src/commands/ctx/agent.rs`

**Interfaces:**
- Consumes: `pace::{binding, worst, current_windows}`, `window::{UsageWindows, Window}`.
- Produces:
  - `PaceConfig` gains `pub spawn_soft_pct: f64` (80.0) and `pub spawn_hard_pct: f64` (95.0)
  - `pub enum pace::SpawnGate { Proceed, Warn { window: &'static str, percent: f64, source: Source }, Refuse { window: &'static str, percent: f64, source: Source } }`, deriving `Debug + Clone + Copy + PartialEq` (no `Eq` — it carries an `f64`, exactly like `PaceDecision` beside it)
  - `pub fn pace::spawn_gate(collector: &UsageWindows, estimator: Option<&UsageWindows>, now: u64, cfg: &PaceConfig) -> SpawnGate` — PURE
  - `pub fn pace::describe_spawn_gate(gate: &SpawnGate) -> Option<String>`
  - `AgentArgs` gains `#[arg(long, default_value_t = false)] pub force: bool`

- [ ] **Step 1: Write the failing test**

```rust
    /// Issue #155, Phase 6(c): quota pressure gates SCHEDULING, never
    /// rotation. Restarting a session because it is expensive throws away a
    /// warm cache and re-reads the whole context -- the most expensive
    /// possible response to a cost signal. Declining to start NEW work is the
    /// cheap one.
    #[test]
    fn the_spawn_gate_warns_at_the_soft_band_and_refuses_at_the_hard_one() {
        let cfg = PaceConfig::default();
        assert_eq!(cfg.spawn_soft_pct, 80.0);
        assert_eq!(cfg.spawn_hard_pct, 95.0);
        let at = |pct| collector_at(pct, 1_000);   // fresh five-hour reading

        assert_eq!(spawn_gate(&at(10.0), None, 0, &cfg), SpawnGate::Proceed);
        assert_eq!(spawn_gate(&at(79.9), None, 0, &cfg), SpawnGate::Proceed);
        assert!(matches!(spawn_gate(&at(80.0), None, 0, &cfg), SpawnGate::Warn { .. }));
        assert!(matches!(spawn_gate(&at(94.9), None, 0, &cfg), SpawnGate::Warn { .. }));
        assert!(matches!(spawn_gate(&at(95.0), None, 0, &cfg), SpawnGate::Refuse { .. }));
        assert!(matches!(spawn_gate(&at(100.0), None, 0, &cfg), SpawnGate::Refuse { .. }));
    }

    /// No reading at all is `Proceed`. Blindness must never become a refusal:
    /// this gate would then block every delegation on a machine that has
    /// never wired a statusline tee, which is a common, legitimate setup.
    #[test]
    fn no_usage_reading_proceeds_rather_than_refusing() {
        assert_eq!(
            spawn_gate(&UsageWindows::default(), None, 0, &PaceConfig::default()),
            SpawnGate::Proceed
        );
    }

    /// Disabled pacing disables this gate too -- one switch, as the operator
    /// expects, and the same first thing `decide` checks.
    #[test]
    fn disabled_pacing_disables_the_spawn_gate() {
        let cfg = PaceConfig { enabled: false, ..PaceConfig::default() };
        assert_eq!(spawn_gate(&collector_at(99.0, 1_000), None, 0, &cfg), SpawnGate::Proceed);
    }

    /// The estimator is consulted only when the collector has nothing
    /// binding, exactly as `decide` already layers its two sources -- a
    /// fresher lower-priority layer never overrides a fresh collector.
    #[test]
    fn the_estimator_is_the_fallback_source_not_an_override() {
        let cfg = PaceConfig::default();
        let estimator = collector_at(99.0, 1_000);
        assert!(matches!(
            spawn_gate(&UsageWindows::default(), Some(&estimator), 0, &cfg),
            SpawnGate::Refuse { source: Source::Estimator, .. }
        ));
        assert_eq!(
            spawn_gate(&collector_at(5.0, 1_000), Some(&estimator), 0, &cfg),
            SpawnGate::Proceed
        );
    }
```

and in `agent.rs`:

```rust
    /// `--force` is the operator saying they accept the spend. Only a Refuse
    /// is overridable; a Warn was never blocking, and a Proceed has nothing
    /// to override.
    #[test]
    fn only_a_refusal_is_overridable_and_only_by_force() {
        let refuse = pace::SpawnGate::Refuse {
            window: "five_hour",
            percent: 97.0,
            source: pace::Source::Collector,
        };
        assert!(spawn_blocked(&refuse, false));
        assert!(!spawn_blocked(&refuse, true), "--force proceeds");

        let warn = pace::SpawnGate::Warn {
            window: "five_hour",
            percent: 85.0,
            source: pace::Source::Collector,
        };
        assert!(!spawn_blocked(&warn, false), "a warning never blocks");
        assert!(!spawn_blocked(&pace::SpawnGate::Proceed, false));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin zirv pace::tests::the_spawn_gate agent::tests::only_a_refusal -- --test-threads=1`
Expected: FAIL to compile — `cannot find function spawn_gate in this scope`.

- [ ] **Step 3: Write minimal implementation**

`config.rs` — two `f64` keys on `PaceConfig` with doc comments stating plainly that they gate SPAWNS only and never rotation, and that a restart on a cost signal would be the most expensive possible reaction. Both `REPO_FORBIDDEN` (a repo checkout must not be able to change when the operator's account stops accepting new work), both in `ENV_MAP` (`ZIRV_CTX_PACE_SPAWN_SOFT_PCT`, `ZIRV_CTX_PACE_SPAWN_HARD_PCT`), `ALL_CONFIG_KEYS`, `.zirv/ctx.toml` and both trust tables.

`pace.rs` — `spawn_gate` reuses the module's existing `binding` and `worst` helpers and the same collector-then-estimator layering `decide` applies, so this gate and the pacing verdict can never disagree about which window is binding or which source is authoritative. `!cfg.enabled` is `Proceed`, and so is no binding window at all. `describe_spawn_gate` returns the operator-facing line (`None` for `Proceed`).

`agent.rs` — add `--force`, and in `run_with`, BEFORE `try_join_dashboard` (so the gate applies to a pane spawn and a headless run alike):

```rust
    let (collector, estimator) = pace::current_windows(&state, &cfg.pace, now, provider);
    let gate = pace::spawn_gate(&collector, estimator.as_ref(), now, &cfg.pace);
    if let Some(note) = pace::describe_spawn_gate(&gate) {
        eprintln!("zirv ctx agent: {note}");
    }
    if spawn_blocked(&gate, args.force) {
        return Err(
            "refusing to start new delegated work at this usage level; wait for the window to \
             reset, or pass --force to spend anyway"
                .into(),
        );
    }
```

with the small pure helper the test pins:

```rust
/// Whether this spawn must not start. Only a `Refuse` blocks, and only
/// without `--force`: a `Warn` is information, not a gate. Deliberately NOT
/// a rot signal -- see `pace::spawn_gate`'s own doc comment.
fn spawn_blocked(gate: &pace::SpawnGate, force: bool) -> bool {
    matches!(gate, pace::SpawnGate::Refuse { .. }) && !force
}
```

`dash/mod.rs` — the same gate in `fulfill_spawn_request`, after the depth cap: a `Refuse` becomes `SpawnRefusal::policy` (never `::channel`, since a headless fallback would route straight around the gate), a `Warn` is printed and allowed. The dashboard has no `--force`, so an operator who wants to override there raises `pace.spawn_hard_pct` or uses `zirv ctx agent --force` directly; say so in the refusal text.

Finally, and non-negotiably: nothing in `rot.rs` or `score.rs` reads `pace` or `window`. Add a test in `rot.rs` asserting the module source contains no `pace`/`window` reference if one does not already exist, or state the invariant in `rot.rs`'s module doc.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin zirv pace:: agent:: dash:: config:: rot:: -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/commands/ctx/pace.rs src/commands/ctx/config.rs src/commands/ctx/agent.rs src/commands/ctx/dash .zirv/ctx.toml README.md docs
git commit -m "feat(pace): gate new delegated work on quota pressure, never rotation"
```

---

### Task 6.4: The benchmarks document

Every phase claims a before/after number. Without a written procedure, "before/after" is an opinion — and issue #155's acceptance criterion is explicitly a measured one.

**Files:**
- Create: `docs/benchmarks/token-cost.md`
- Modify: `docs/obsidian/Modules/Usage and Pacing.md` (a link to it)
- Test: none — this is a document. Its correctness is that a reader can follow it and get the numbers.

**Interfaces:**
- Consumes: `zirv ctx usage --sessions` (Task 2.4), `delegations.jsonl` (Task 2.3), `TelemetryKind::ReviewRun` events (Task 2.2), `zirv ctx status` (Task 5.6).
- Produces: `docs/benchmarks/token-cost.md`.

- [ ] **Step 1: Write the document**

It must define, concretely enough that two people get the same number:

1. **Tokens per completed task.** The exact command sequence (`zirv ctx usage --sessions`, plus reading `<state>/logs/delegations.jsonl`), what counts as "one completed task" (one workflow id from `zirv workflow`, or one work group id), and the arithmetic: sum the four raw classes across every session and delegation attributed to it.
2. **Cache-hit ratio.** `cache_read_input_tokens / (input_tokens + cache_creation_input_tokens + cache_read_input_tokens)`, per session and aggregated, with the standing caveat that class weighting against the vendor's own limiter is undocumented so this is an approximation of *cost*, though an exact measure of *cache behaviour*.
3. **Review count per change.** Independent reviewer launches (`TelemetryKind::ReviewRun` events per `workflow_id`) and total review-diff bytes shipped (`ReviewPackage::diff.len()` summed over rounds).
4. **The controlled-comparison protocol.** Same repository, same task text, same seat model, sessions run back to back, `zirv ctx usage --sessions` captured immediately before and after each run. State plainly that these are single-run observations on one machine, not a benchmark suite — an honest small number beats a fabricated rigorous one.
5. **A results table**, one row per phase, filled in as each PR lands, with the numbers actually observed and the machine they were observed on.

- [ ] **Step 2: Verify the procedure by following it**

Run the whole procedure end to end on this machine and fill in the Phase 1–6 rows with real observations. Any step that cannot be followed as written is a defect in the document, not an excuse to leave the row blank.

- [ ] **Step 3: Commit**

```bash
git add docs/benchmarks/token-cost.md docs/obsidian
git commit -m "docs: define the token-cost measurement procedure and record phase results"
```

---

### Task 6.5: Version bump and vault updates for Phase 6

**Files:**
- Modify: `Cargo.toml` (`2.36.0`), `Cargo.lock`
- Modify: `docs/obsidian/Modules/{Rot Engine,Ctx Adapters,Usage and Pacing,Built-in Commands}.md`, `docs/obsidian/Concepts/Untrusted Configuration.md`, `README.md`, `docs/obsidian/Development/{Decision Log,Work Journal,Active Work}.md`

- [ ] **Step 1: Bump to `2.36.0`**, `cargo build`, `rg '2\.35\.0' src/ docs/`.
- [ ] **Step 2: Update the vault**
- `Modules/Rot Engine.md` — `token_gates` and its precedence; `token_floor`/`token_ceiling` are now `Option<u64>` overrides; the ratio defaults; and, restated, that `rot.rs` is still pure because capacity arrives via `Capabilities`.
- `Modules/Ctx Adapters.md` — `context_window_tokens` / `capabilities_for_model`; claude's conservative default and its long-window recognition; codex reports none, deliberately.
- `Modules/Usage and Pacing.md` — the spawn gate, its two thresholds, `--force`, and the hard rule that quota pressure never drives rotation. Link the benchmarks doc.
- `Modules/Built-in Commands.md` — `zirv ctx agent --force`.
- `Concepts/Untrusted Configuration.md` + `README.md` — rows for `score.token_floor_ratio`, `score.token_ceiling_ratio`, `score.model_context_tokens`, `pace.spawn_soft_pct`, `pace.spawn_hard_pct`.
- `Development/Decision Log.md` — why cost pressure gates scheduling and never rotation; why an adapter's capacity default is conservative; why codex reports nothing rather than a guess.
- `Development/{Work Journal,Active Work}.md` — close out issue #155 with the final measured numbers.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock docs README.md
git commit -m "chore: bump to 2.36.0 and document model-aware rotation and spawn gating"
```

---

### Task 6.6: Phase 6 verification gates, Linux/Docker pass, cross-review, and PR

- [ ] **Step 1: Capture the `main` baseline** (as Task 1.5 Step 1).
- [ ] **Step 2: Run all five gates in the FOREGROUND**, diff failure NAMES.
- [ ] **Step 3: Linux/Docker verification — REQUIRED** (this phase touches `pace`):

```bash
git -c core.autocrlf=false archive HEAD -o /tmp/zirv.tar
# rust:1-bookworm, NON-root:
cargo test --bin zirv wrap:: -- --test-threads=1
cargo clippy --all-targets -- -D warnings
```

- [ ] **Step 4: Measure and fill the benchmarks table**

Run the full `docs/benchmarks/token-cost.md` procedure and record the end-to-end result versus the Phase 2 baseline: tokens per completed task, cache-hit ratio, review count per change. This is the number issue #155 is closed on.

- [ ] **Step 5: Codex cross-review**

```bash
zirv agent codex "Review the diff on branch feat/token-cost-p6-model-aware against main. Focus on: (1) whether token_gates can ever return a floor >= ceiling, or a ceiling above the model's actual capacity; (2) whether rot.rs still reads no fs/clock/env/net and no pace or window state; (3) whether the spawn gate can refuse on a stale or blind usage reading, which would block delegation on a machine with no statusline tee. Reply with confirmed, concrete findings only." -- --model gpt-5-codex
```

- [ ] **Step 6: Open the PR**, closing issue #155, with the full benchmarks table in the body and an explicit statement of the Docker run's result.

**Phase 6 acceptance criteria (all measurable):**
- A 1M-capacity seat resolves `(500_000, 800_000)`; a 200k seat resolves `(100_000, 160_000)` — identical to today; an unknown capacity resolves the absolute fallbacks unchanged.
- An explicit `score.token_ceiling` wins over the ratio in every case.
- A session at 96% of its five-hour window refuses a new delegation without `--force` and its rotation verdict is unchanged by that fact.
- `rot.rs` contains no fs/clock/env/net access and no `pace`/`window` reference.
- `docs/benchmarks/token-cost.md` carries a filled results row for every phase.

---

## Self-Review

Checked after writing, against the locked decisions in the brief and issue #155.

**Every locked decision maps to a task:**

| Locked decision | Task |
| --- | --- |
| 1(a) truncation warning: decision entry + stderr | 1.1 |
| 1(b) trim this repo's `common.md` under 4096 | 1.2 |
| 1(c) merge the two memory layers, dedupe by name, fix `describe()` | 1.3 |
| 1(d) move memory-retrieval after canonical context; `v7`→`v8` | 1.3 |
| 2 widen `TranscriptUsage`, stop pre-summing, keep `context_total()` | 2.1 |
| 2 widen `TelemetryEvent` + lineage + sidechain bucket | 2.2 |
| 2 per-delegation checkpoint records | 2.3 |
| 2 `zirv ctx usage --sessions` | 2.4 |
| 3 provenance hash in `render_generated` | 3.1 |
| 3 skip on match, `context-dedup-skip`, `context.dedupe_native` | 3.2 |
| 4(a) review-gate guard text in both prompt layers | 4.1 |
| 4(b) delta re-review from the last reviewed sha | 4.2 |
| 4(c) enforce stop-on-no-new-findings in code | 4.3 |
| 5(a) `PromptRole::SubOrchestrator` + depth cap at spawn time | 5.1, 5.3 |
| 5(b) `WorkGroup` + `zirv ctx group create/status/close` | 5.2 |
| 5(c) `SpawnRequest` role/parent/group fields | 5.3 |
| 5(d) `--group`/`--budget-tokens`/`--max-tool-calls`, soft/hard thresholds, no model downshift | 5.4 |
| 5(e) heavy-OPERATION permits, `max_heavy_operations` + parsing alias | 5.5 |
| 5(f) `zirv ctx status` group tree with per-child spend | 5.6 |
| 6(a) `Capabilities::context_window_tokens` + adapter capacity | 6.1 |
| 6(b) ratio gates, absolutes as overrides, `rot.rs` stays pure | 6.2 |
| 6(c) quota pressure gates scheduling, `--force`, never auto-restart | 6.3 |
| 6(d) benchmarks doc | 6.4 |

**Type and signature consistency, checked across tasks:** `TranscriptUsage`'s four field names are identical in 2.1, 2.2, 2.3, 2.4, 5.4 and the spec. `context_total()` is the only combining helper and is used identically in 2.1 and 5.4. `PromptRole::SubOrchestrator` is spelled the same in 5.1, 5.3 and `spawnreq::role_of`'s `"sub-orchestrator"` string. `log::Delegation` (Serialize, borrowed) and `log::DelegationRow` (Deserialize, owned) are introduced in 2.3 and 5.6 respectively and do not collide. `compile::compile`'s `log_truncation` parameter added in 1.1 is consumed again in 3.2. `finding_key` is the single finding identity in 4.3, matching the existing `has_repeated_meaningful_finding`. `Capabilities` gains its field in 6.1 and is read in 6.2 only.

**Deviations from the brief, and why:**

1. **`ctx.model_context_tokens` → `score.model_context_tokens` (Task 6.2).** There is no `[ctx]` table in this config model; the key is consumed only by `rot::token_gates` alongside `token_floor`/`token_ceiling`, which live in `[score]`. Same behaviour, correct home.
2. **Phase 6 does not thread a live model string into rotation (Task 6.1).** `IncrementalScorer::poll` calls `adapter.capabilities()` internally and no launch seam passes a model down to it. Rather than refactor five supervisor call sites for an unmeasured gain, `capabilities()` carries the adapter's conservative default and `score.model_context_tokens` covers the operator case; `capabilities_for_model` exists for a future caller. Recorded as out of scope in the spec.
3. **Truncation logging is gated by an explicit `log_truncation` parameter rather than being unconditional (Task 1.1).** `zirv context status` compiles once per registered adapter purely to report truncation; unconditional logging would spam the decision log on every status invocation. Making each call site answer is the same compiler-enforced pattern the cross-harness-permissions plan used for `LaunchMode`.
4. **`context.dedupe_native` is not `REPO_FORBIDDEN` (Task 3.2).** A repo layer can only set it `false`, and `false` injects MORE context — narrowing, which this trust model allows. It is folded with the same stricter-layer-wins rule `pace.enabled` uses, so a repo `true` cannot re-enable a skip the operator disabled. Flagged because the brief said "per REPO_FORBIDDEN conventions", and the convention this key actually matches is the narrowing fold, not the hard rejection.
5. **`permit::is_heavy` classifies commands at `script_runner::Command::invoke` (Task 5.5).** The brief says "classify actual commands … and hold a permit for the command's duration", without naming a seam. `Command::invoke` is the single place a zirv script actually spawns a shell child; the safety hook cannot hold a permit because it is a short-lived process that returns immediately.
6. **A script command WAITS for a permit; a spawn REFUSES (Task 5.5).** Issue #133's refuse-not-queue reasoning applies to a *spawn* (an unbounded new workload). Failing a `zirv test` the operator already typed, because a background build holds the slot, would be worse than a bounded wait.
7. **Each phase closes with two tasks (version+vault, then gates+review+PR) rather than one.** The brief asked for "its version bump and its verification/review gate as explicit final tasks" — plural — and they have genuinely different failure modes, so they get separate review points.
