# Exhaustive `.zirv/ctx.toml` and `.zirv/.settings.toml` reference configs

## Deliverables

1. `.zirv/ctx.toml` -- rewritten (it already existed, with only `[chat] model = "fable"` set; see "Non-obvious decision" below) to enumerate every `CtxConfig` key at its default, with REPO_FORBIDDEN keys shown commented-out.
2. `.zirv/.settings.toml` -- new file, `[agents.claude]`/`[agents.codex]` both `enabled = true` (the default), with a header explaining the repo-can-only-narrow fold.
3. Two new tests in `src/commands/ctx/config.rs`'s `tests` module:
   - `the_repo_own_ctx_toml_parses_and_matches_defaults`
   - `the_repo_own_settings_toml_parses_without_error`

## Key counts

### `.zirv/ctx.toml` -- by `CtxConfig` field tree

| Table | Keys (incl. commented) | Repo-forbidden (commented) |
|---|---|---|
| top-level (`agent`, `agent_bin`) | 2 | 2 |
| `chat` | 1 (`model`) | 0 |
| `score` | 12 | 0 |
| `wrap` | 2 | 0 |
| `supervise` | 7 | 1 (`on_failure`) |
| `handoff` | 3 | 1 (`model`) |
| `pace` (incl. `use_credits`) | 15 | 5 (`poll_enabled`, `poll_min_interval_secs`, `use_credits.claude`, `use_credits.codex` -- one `REPO_FORBIDDEN` entry `["pace","use_credits"]` covers the whole sub-table) |
| `optimize` | 7 | 1 (`model`) |
| `prompt` | 4 | 4 (all) |
| `mail` | 4 | 2 (`enabled`, `max_delivered_bytes`) |
| `memory` | 5 | 5 (all) |
| `chrome` | 3 | 1 (`events`) |
| `dash` | 5 | 5 (all) |
| **Total** | **70 keys** | **25 keys**, matching `REPO_FORBIDDEN`'s 25 entries in config.rs exactly |

`agents` (the `#[serde(skip)]` field) is out of scope for `ctx.toml` by design -- it lives in `.settings.toml` and a same-named `[agents]` table inside `ctx.toml` is rejected (`agents_in_ctx_toml_is_rejected_so_the_two_files_stay_distinct`, pre-existing test, still passes).

### `.zirv/.settings.toml`

2 agents (`claude`, `codex`, from `adapters::ADAPTERS` in `src/commands/ctx/adapters/mod.rs`), each with 1 key (`enabled = true`).

## Non-obvious decisions / defaults read from code

- **`.zirv/ctx.toml` already existed** on this branch (committed in `a5bb389`/`afd0ce4`) with real, deliberate content: `[chat] model = "fable"`, documented in-file as "User decision 2026-08-13: the orchestrator runs Fable". `chat.model` is the one config key that is *not* `REPO_FORBIDDEN` (see `ChatConfig`'s doc comment in config.rs: it only shapes an interactive session the operator launched, disclosed on screen). Overwriting this to `None`/commented to satisfy a literal "every key at default" would have silently reverted a real, already-shipped decision. I kept `model = "fable"` and adjusted the parse test's expected value accordingly (`expected.chat.model = Some("fable".to_string())`) rather than reverting the file or the decision. Flagged here explicitly since it's a deviation from the literal task wording ("every key including keys at default value... every value IS the default").
- **`agents` field can never structurally equal `CtxConfig::default().agents`.** `CtxConfig::load` (config.rs line ~980, now ~1050) always ends with `cfg.agents = crate::settings::AgentGate::load(repo, env)?`, which populates real per-adapter `AgentState` entries (claude, codex) -- never `AgentGate::default()`'s empty `states` map, even with zero settings files present. So a naive `assert_eq!(cfg, CtxConfig::default())` would fail on the `agents` field for *any* repo, regardless of file content. The test copies `cfg.agents.clone()` onto the `expected` value before comparing, and separately asserts the *behavioral* default (`cfg.agents.is_enabled("claude") && is_enabled("codex")`) to still pin that `.settings.toml`'s `enabled = true` lines are true no-ops per the fold.
- `ScoreConfig::default()` (`config.rs::ScoreConfig::default`): `window=10, min_turns=10, token_floor=100_000, token_ceiling=160_000, weight_tool_failure=40.0, weight_repetition=30.0, weight_marker=30.0, repetition_threshold=3, advise_at=40, compact_at=60, restart_at=80, marker="[zirv]"` (the `DEFAULT_MARKER` const).
- `WrapConfig::default`: `debounce_ms=3000, inject_timeout_ms=20_000`.
- `SuperviseConfig::default`: `max_restarts=2, poll_ms=2000, interval_secs=900, max_cycle_secs=3600, max_failures=5, backoff_base_secs=60, on_failure=None, max_nudges=3`.
- `HandoffConfig::default`: `model=None, tail_items=5, timeout_secs=30`.
- `PaceConfig::default`: `enabled=true, max_percent=99.0, collector_max_age_secs=900, estimator=true, five_hour_budget_tokens=0, seven_day_budget_tokens=0, count_cache_reads=false, jitter_secs=30, fallback_delay_secs=900, wait_slack_secs=3600, max_wait_secs=None, soft_percent=80.0, poll_enabled=true, poll_min_interval_secs=60, use_credits={claude:false, codex:false}`.
- `OptimizeConfig::default`: `enabled=true, sessions_sampled=10, max_surface_bytes=200_000, model="", recommend_tool_failure_rate=0.25, recommend_corrections=3, recommend_cooldown_secs=86_400`.
- `PromptConfig::default`: `enabled=true, repo_layer=true, max_repo_bytes=4096, harnesses=true`.
- `MailConfig::default`: `enabled=true, max_message_bytes=4096, max_delivered_bytes=4096, keep=50`.
- `MemoryConfig::default`: `enabled=true, harvest=false, max_entries=50, max_entry_bytes=512, max_injected_bytes=2048`.
- `ChromeConfig::default`: `banner=true, bar=true, events=true`.
- `DashConfig::default`: `enabled=true, sidebar_cols=24, roster_max_age_secs=604_800, max_panes=9, mouse=true`.
- `ChatConfig` derives `Default` (`model: None`).
- `REPO_FORBIDDEN` table in config.rs enumerated and cross-checked: 25 entries, all represented as commented lines under their table, matching the task's own listed set plus confirming `pace.use_credits` is one entry covering the whole sub-table (`value_at` matches a table node the same way it matches a leaf).
- Confirmed via `settings.rs`/`adapters/mod.rs`: only two known agents, `claude` and `codex` (`pub const ADAPTERS: &[(&str, AdapterCtor)] = &[("claude", make_claude), ("codex", make_codex)]`).

## Test evidence

```
cargo test --quiet -- --test-threads=1 config settings
  test result: ok. 110 passed; 0 failed; 0 ignored; 0 measured; 1346 filtered out

cargo test --quiet -- --test-threads=1 ctx
  test result: FAILED. 1277 passed; 48 failed; 0 ignored; 0 measured; 131 filtered out
  (all 48 failures are the pre-existing os-193 spawn family / socket-path-length
  family -- exec, wrap, handoff, hook, memory, optimize, run_loop, agent,
  supervise, plus state::tests::socket_paths_stay_short_enough_for_macos --
  none touch config.rs or settings.rs; none are new)

cargo test --quiet -- --test-threads=1 utils optimize
  test result: FAILED. 104 passed; 3 failed
  (the 3 failures -- hook::a_failure_heavy_session_queues_an_optimize_recommendation,
  hook::a_healthy_correction_heavy_session_prints_only_the_optimize_hint,
  optimize::the_verb_prints_a_report_and_stores_a_copy -- are all already in the
  48-failure list above; utils.rs tests themselves all passed, confirming adding
  .zirv/.settings.toml doesn't affect script listing since .zirv is already
  excluded from those tests, which use isolated temp dirs, not the real repo tree)

cargo fmt -- --check
  clean (no output)

cargo clippy --all-targets -- -D warnings
  clean -- one lint hit and fixed: clippy::field_reassign_with_default on the
  new test (mutating `expected.agents`/`expected.chat.model` after
  CtxConfig::default()); switched to struct-update syntax
  (CtxConfig { agents: ..., chat: ChatConfig { model: ... }, ..Default::default() })
```

## Self-review

- Verified `optimize.rs`'s "the analysed tree is unchanged after a run" test (`the_verb_never_modifies_an_analysed_file`) uses a synthetic `fixture_tree()`, not the real repo `.zirv`, so it is unaffected by these new files.
- Verified `utils.rs`'s script-listing tests build their own temp `.zirv` dirs, not the real repo tree, so listing behavior for the real repo was not exercised by any existing test either way; `RESERVED_ZIRV_FILES` already covers `ctx.toml` and `.settings.toml` by name (`src/utils.rs:39`), so both stay excluded from script listing regardless.
- Verified `agents_in_ctx_toml_is_rejected_so_the_two_files_stay_distinct` (pre-existing test) still passes -- `.zirv/ctx.toml` never gained an `[agents]` table.
- Cross-checked every key in the new `ctx.toml` against `ENV_MAP` and `REPO_FORBIDDEN` in config.rs by table, not just by memory, to avoid missing or mis-placing a forbidden marker.
- Did not touch `docs/obsidian/` -- this is a reference-config/test change, not a behavior/contract/architecture change per CLAUDE.md's doc-update trigger table, so no vault update was made.

## Concerns

- The literal instruction "every value IS the default" is not quite true of the committed file for one key (`chat.model = "fable"`), by deliberate choice -- see "Non-obvious decisions" above. If the intent was actually to revert that decision back to the true default (`None`), that's a one-line change (drop the `model = "fable"` line, revert `expected.chat.model` to `None` in the test) but it would silently undo a previously shipped, documented operator choice, so I left it as a flagged decision rather than making that call unilaterally.
- Did not message `main` about the `chat.model` conflict before proceeding (Auto Mode bias toward continuing); flagging here instead for visibility.

---

## Revision (2026-08-17): uncomment-to-override rework

The coordinator flagged a real bug in the first version: the repo `ctx.toml` layer table-merges ON TOP of the operator's own `~/.zirv/ctx.toml` in `CtxConfig::load`, so an *active* default-valued key committed in the repo file would silently clobber any operator global customization of that same key -- the file being "exhaustive" made this worse, not better, since it maximized the number of keys capable of doing that. Direction: both files must be freely overwriteable per-repo, sample-config style -- every key commented out except the one pre-existing real decision (`chat.model = "fable"`).

### Changes

1. **`.zirv/ctx.toml`** -- every key converted to a commented-out line at its built-in default (`# key = value  # comment`), including every table header except `[chat]` (the only section with an active key). Header rewritten to explain the uncomment-to-override model and name the clobbering bug as the reason a commented key matters. Removed the sentence claiming the test pins values against drift (the test's role changed, see below). REPO-FORBIDDEN keys keep their marker and default value in the comment; uncommenting one still has no effect (rejected by `reject_untrusted_keys`).
2. **`.zirv/.settings.toml`** -- both `[agents.claude]`/`[agents.codex]` blocks commented out too, for symmetry with ctx.toml. Noted explicitly that this file's fold is *not* a deep merge (unlike ctx.toml) so an active `enabled = true` was already an inert no-op -- the comment-out here is for consistency/clarity, not fixing a second instance of the same bug.
3. **Test rework in `src/commands/ctx/config.rs`** -- replaced `the_repo_own_ctx_toml_parses_and_matches_defaults` with `the_repo_ctx_toml_parses_and_stays_exhaustive`, asserting two things instead of one:
   - (a) the file still parses through the real repo-layer path and `chat.model = Some("fable")` is the *only* non-default value in the resulting config (same struct-update-syntax comparison as before, `agents` still copied over since it's `#[serde(skip)]` and always freshly populated);
   - (b) exhaustiveness: a new hand-maintained `ALL_CONFIG_KEYS: &[(&str, &str)]` constant (72 `(table, key)` pairs, one per real `CtxConfig` field, including the ones missing from `ENV_MAP`) is checked against the raw file text via two new helpers, `table_section` (slices the file to one table's lines, matching a header whether commented or not, stopping at the next header so a repeated key name like `enabled`/`model` can't produce a false positive from an unrelated section) and `section_has_key` (checks the key appears as its own `key = ` assignment, active or commented, ignoring the value). This fails only when a key is missing from the file text entirely, never when a value is edited -- verified: the test passed with commented-out placeholder values that don't match the real defaults' formatting nuances (e.g. `604_800` vs `604800`), because it only ever checks the key name.

   Considered but rejected: adding `Serialize` derives to the config structs and using `toml::to_string(&CtxConfig::default())` to generate the exhaustive key list programmatically (ties coverage directly to the real struct instead of a hand-maintained parallel list, so it can't drift). Rejected because `Option<T>` fields (`agent`, `supervise.on_failure`, `handoff.model`, `pace.max_wait_secs`, `chat.model`) serialize as `None` at least twice in `CtxConfig::default()`, and TOML has no null representation -- `toml::to_string` on a struct with an active `None` field is a real risk of an unhandled serialization error (untested against this repo's exact `toml` crate version), and adding `Serialize` to ten-plus already-`Deserialize`-only production structs is a wider-blast-radius change than the task asked for. The hand-maintained list is the "cheaper honest mechanism" the task explicitly sanctioned as an alternative.

### Bug fix note

The `chat.model` conflict flagged in the original report (deviation from literal "every value IS the default") is now resolved by design rather than by exception: it's still the one active key, but the file's whole model changed from "active defaults + one active override" to "commented defaults + one active override" -- so `chat.model` is no longer an exception to a stated invariant, it's the only key the invariant was ever meant to allow active.

### Test evidence (revision)

```
cargo test --quiet -- --test-threads=1 config settings
  test result: ok. 110 passed; 0 failed; 0 ignored; 0 measured; 1346 filtered out

cargo fmt -- --check
  clean (one rustfmt-driven closure reformat applied via `cargo fmt`, then reverified clean)

cargo clippy --all-targets -- -D warnings
  clean
```

### Concerns (revision)

- `ALL_CONFIG_KEYS` is a hand-maintained parallel list; if a future field is added to `CtxConfig`'s tree without a matching entry added here, the exhaustiveness test will not catch that omission (it only checks the list's own keys against the file, not the file/list against the live struct). This is the accepted tradeoff of not deriving `Serialize` (see above) -- flagging it so a future session knows the guarantee's actual shape.
