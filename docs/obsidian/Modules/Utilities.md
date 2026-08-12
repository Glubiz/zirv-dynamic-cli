---
last-verified: 2026-08-12
---

# Utilities

## Quick Reference

- **Files:** `src/utils.rs`; also covers `src/commands/ctx/optimize.rs` (the `zirv ctx optimize` verb) and `src/commands/ctx/prompt.rs` (the injected session prompt), since both build directly on `utils.rs` and share its trust-boundary conventions
- **Used by:** [[Script Runner]] (`file_to_script`/`parse_script_content` turn a `.zirv/` file into the `Script` it executes), [[Built-in Commands]] (`is_reserved_command` gates every user script/shortcut name, `candidate_names_in_dir`/`suggest_matches` power the "did you mean" error), [[Ctx Subsystem]] (`optimize` is a `ctx` verb; `prompt.rs` is called from every session-launching supervisor)
- **Depends on:** [[Ctx Adapters]] for `distiller_cmd` (the restricted-child mechanism `optimize.rs` reuses via `handoff::run_model`) and `AgentAdapter::base_system_prompt` (the adapter layer `prompt.rs` splices in); [[Ctx Supervisors]] for the four callers of `prompt::compose` (`exec.rs`, `wrap.rs`, `run_loop.rs`, `resume.rs`)
- **Tests:** `src/utils.rs` has an inline `#[cfg(test)] mod tests` covering `levenshtein`, `truncate_bytes` (char-boundary safety), `suggest_matches` (ordering, distance cutoff, case-insensitivity, the O(n·m)-matrix short-circuit for a 400,000-char argv), and `candidate_names_in_dir` (scripts + shortcuts, `ctx.toml` exclusion, missing dir). `optimize.rs` has its own large `#[cfg(test)] mod tests`; the report-only guarantee is asserted by `the_verb_never_modifies_an_analysed_file`, which snapshots every file under the fixture repo and home directories before and after `run_with(...)` and asserts the snapshots are byte-identical (`tree_snapshot`, sorted `(path, contents)` pairs). `prompt.rs` has its own inline tests covering layer composition, the `--simple`/`enabled=false` opt-outs, and the adapter/repo/command-line splice order.
- **If changed:** [[Script Runner]], [[Built-in Commands]], [[Ctx Subsystem]], [[Ctx Adapters]], [[Ctx Supervisors]], [[Untrusted Configuration]], [[Script Files]], [[Shortcuts]], [[Context Management]], [[Decision Log]]
- **Gotchas:** `zirv ctx optimize` is report-only — it writes only to stdout, its own timestamped copy under the state dir, and an explicit `--out` path, both via `state::write_private` (on Unix this creates the file `0600` and re-asserts that mode even if the path already existed; on non-Unix, including this repo's own Windows dev environment, `write_private` is a plain `fs::write` with no permission restriction). It never edits `CLAUDE.md`, `settings.json`, or any other analysed file; a test proves it by hashing the repo and home trees before and after a run. Its judgment-model child goes through the same restricted `distiller_cmd` path as handoff distillation (see [[Ctx Adapters]]) — not a separate, looser code path — because the prompt it builds embeds untrusted repo `CLAUDE.md` text. Separately, `prompt.rs`'s repo layer (`<repo>/.zirv/system-prompt.md`) cannot enable or widen itself: a repo `ctx.toml` that tries to set `prompt.enabled`, `prompt.repo_layer`, or `prompt.max_repo_bytes` is a **hard config-load error**, not a silently-ignored one — only `~/.zirv/ctx.toml` or `ZIRV_CTX_PROMPT*` env vars (the operator) may set those keys. See [[Untrusted Configuration]].

## Purpose

`src/utils.rs` is the small, dependency-light layer everything else in the CLI leans on: turning a script file into a `Script`, deciding whether a name collides with a built-in, and fuzzy-matching a mistyped command name against what's actually in a `.zirv/` directory. Two `ctx` modules extend the same "treat repo content as untrusted, cap what you read, never surprise the user" posture at larger scale: `optimize.rs` (an analysis-only verb) and `prompt.rs` (the text injected into every zirv-launched agent session).

## How It Works — `utils.rs`

**Parsing.** `SUPPORTED_EXTENSIONS` is `["yaml", "yml", "json", "toml"]`. `file_to_script` reads a path, lowercases its extension, and hands off to `parse_script_content`, which dispatches to `serde_yaml_ng`, `serde_json`, or `toml` to deserialize into a `Script` (see [[Script Runner]] for that type's shape). An unrecognized extension is a plain `Err`, not a panic.

**Reserved names.** `SCRIPT_DIR_NAME` is `.zirv`. `RESERVED_COMMANDS` lists the built-in top-level names and short aliases handled in `main.rs` before any script lookup: `help`/`h`, `version`/`v`, `init`/`i`, `create`/`c`, `ctx`. `is_reserved_command` just checks membership. Because these are intercepted first, a `.zirv/ctx.yaml` or a shortcut named `help` can never be reached — see [[Built-in Commands]].

**Shortcuts.** `Shortcuts` is `{ shortcuts: HashMap<String, String> }`, deserialized from a directory's `.shortcuts.yaml` (short alias → script filename). See [[Shortcuts]].

**Home directory.** `home_dir()` tries `$HOME` then `%USERPROFILE%`, so it resolves on both Unix and Windows.

**"Did you mean" suggestions.** `levenshtein` computes char-based (not byte-based) edit distance so non-ASCII names compare correctly. `suggest_matches(target, candidates)` scores every candidate, skips exact matches and duplicates, applies a length-scaled threshold (`max_suggestion_distance`: 1 for names ≤3 chars, 2 for 4-5, 3 beyond) with a cheap length-difference short-circuit *before* running the O(n·m) matrix — this matters because a mistyped multi-hundred-thousand-character argv must not be diffed character-by-character against every script name — and returns up to 3 matches, closest first, ties broken alphabetically. `candidate_names_in_dir(dir)` gathers the invocable names in a `.zirv/` directory: the file stem of every file with a supported extension (excluding `RESERVED_ZIRV_FILES`), plus every shortcut key; a missing or unreadable directory yields an empty list rather than an error. Together these power the not-found error in `Input::get_file_path`.

**`RESERVED_ZIRV_FILES`.** `[".shortcuts.yaml", "ctx.toml", ".settings.toml"]` — zirv's own configuration files inside a `.zirv/` directory, never invocable scripts. `is_reserved_zirv_file(name)` compares against this list with `eq_ignore_ascii_case`, not exact equality: NTFS (and APFS by default) resolve a file case-insensitively, so `Path::exists` finds `.Settings.toml` when asked for `.settings.toml`, and the guard has to agree or a differently-cased reserved file would be honored by `AgentGate`/`CtxConfig` while still being listed as an invocable script and resolvable as one. This is deliberately stricter than every filesystem requires — on ext4, `CTX.toml` is a distinct, ordinary file from `ctx.toml`, and the guard excludes it anyway — because the goal is one rule that behaves the same everywhere zirv runs, not the minimum each platform demands. Used by `candidate_names_in_dir` here, by `help.rs`'s script listing, and by `input.rs`'s `find_script_in_dir` (so a literal `zirv .settings` cannot resolve `.settings.toml` as a script).

**Shared helper.** `truncate_bytes(text, cap)` truncates a `String` to at most `cap` bytes, backing off to the nearest earlier char boundary so the result is always valid UTF-8 (an `Option<usize>` cap of `None` means unlimited). It's the one place both `optimize.rs` and `prompt.rs` cap untrusted disk content, so the two modules cap the same way.

## How It Works — `zirv ctx optimize` (`optimize.rs`)

`optimize` is an analysis verb: it reads the configuration surfaces that steer a session, looks for friction in recent transcripts, optionally asks a small model to judge the surfaces, and prints a report. It never edits anything it analyses.

**Surfaces collected** (`collect_surfaces`, capped at `MAX_SURFACES = 40`, each file capped at `cfg.optimize.max_surface_bytes`, default 200,000 bytes): global `CLAUDE.md` (`~/CLAUDE.md` and `~/.claude/CLAUDE.md`), the repo's own `CLAUDE.md`, every nested `CLAUDE.md` up to `MAX_NESTED_DEPTH = 4` directories deep (symlinked directories are skipped so a link can't walk the scan outside the repository), and user/project/local `settings.json`. Settings-layer surfaces are flagged `is_settings()` and their values are never sent to the judgment model verbatim, since an `env` block routinely holds secrets.

**Evidence collected**: the newest sampled sessions' transcripts (`cfg.optimize.sessions_sampled`, default 10, overridable with `--sessions`), parsed through the same `AgentAdapter` the other ctx verbs use, plus recent lines from zirv's own decision log (`LOG_LINES_SAMPLED = 500`) — both read-only.

**Findings** come from three deterministic linters (`lint_redundancy`, `lint_dead_references`, `friction_findings`) plus, unless `--no-model` is passed, one call to a judgment/distiller model built from `judgment_prompt(surfaces, evidence, ...)`. That call goes through `handoff::run_model`, which builds its child process via `adapter.distiller_cmd(model)` — the same tool-restricted command builder handoff distillation uses (see [[Ctx Adapters]]), not a separate path — which matters because the prompt embeds raw repo `CLAUDE.md` text that must be treated as untrusted (see [[Untrusted Configuration]]). A failed or unavailable model degrades the report to deterministic findings only; it never fails the run.

**Output, and the report-only guarantee**: the rendered report is written to stdout, and best-effort to a timestamped copy under the state dir (`<state>/optimize-reports/<repo-slug>/<epoch>-report.md`, with a numeric suffix if two runs land in the same wall-clock second — `unique_report_path`) via `state::write_private`. On Unix that helper creates the file mode `0600` and re-sets that mode even if a file already existed at the path, rather than leaving whatever a prior `touch` left behind; on non-Unix it's a plain `fs::write`. If `--out <path>` is given, the same helper writes the report there too. No other write happens. This is exercised directly by `the_verb_never_modifies_an_analysed_file`, which snapshots the full fixture repo tree and home tree (every file's path and contents) before calling `run_with(...)` and asserts both snapshots are unchanged afterward — including the global `CLAUDE.md` layer.

Args (`OptimizeArgs`): `--agent` (adapter override), `--no-model` (skip the judgment call), `--sessions <n>` (override the sample size), `--out <path>` (extra write target).

## How It Works — the injected session prompt (`prompt.rs`)

`prompt::compose(home, repo, simple, cfg: &PromptConfig)` builds the text zirv injects as the agent's system prompt for a launched session, or returns `None` when nothing should be injected (`simple` — e.g. `--simple` — or `cfg.enabled == false` both suppress it entirely, including the shipped default).

**Layers, in fixed order**, each separated by a `---` divider:

1. **Default** — `DEFAULT_PROMPT`, a zirv-authored, deliberately short, three-rule floor (follow the repo's own conventions and let a repository instruction file win over these defaults; prefer deterministic tool use; report failures honestly rather than describing unverified work as done).
2. **Adapter** (spliced in right after Default, by `with_adapter_layer`, called from the supervisor after the adapter is known — `compose` itself doesn't see the adapter) — `AgentAdapter::base_system_prompt()`, agent-specific text naming that agent's own tools, so only that agent ever gets it. `None` by default; only the claude adapter currently overrides it.
3. **User** — `~/.zirv/system-prompt.md`, read uncapped (the operator's own file).
4. **Repo** — `<repo>/.zirv/system-prompt.md`, read only when `cfg.repo_layer` is true and truncated to `cfg.max_repo_bytes` (default 4096) via `truncate_bytes`. It is explicitly labeled in the composed text as coming from the repository checkout and told it does not override anything above it and grants no permissions — the same "capped, labeled, can't enable itself" treatment `CLAUDE.md` gets in `optimize.rs`'s judgment prompt.
5. **Command-line** — the operator's own `--system-prompt`/equivalent flag value, added last as the highest-priority layer.

`PromptConfig` (`src/commands/ctx/config.rs`) has three fields: `enabled` (default `true`), `repo_layer` (default `true`), `max_repo_bytes` (default `4096`). All three are on the `REPO_FORBIDDEN` list in `config.rs`: if a repository's own `.zirv/ctx.toml` sets any of `prompt.enabled`, `prompt.repo_layer`, or `prompt.max_repo_bytes`, `CtxConfig::load` returns a hard error naming the key and where it belongs instead (`~/.zirv/ctx.toml` or the corresponding `ZIRV_CTX_PROMPT*` / `ZIRV_CTX_PROMPT_REPO` / `ZIRV_CTX_PROMPT_MAX_REPO_BYTES` env var) — a repo checkout cannot raise its own cap or turn its own layer on, and the failure mode is loud rather than a silently-clamped value.

**Injection point**: `compose` is called from the four session-launching verbs — `exec.rs`, `wrap.rs`, `run_loop.rs`, and `resume.rs` (all under `src/commands/ctx/`) — each of which resolves `cfg.prompt`, calls `compose`, splices in the adapter layer once the adapter is chosen, and delivers the composed text to the child process via the adapter's system-prompt mechanism (a file-flag or inline-flag argv, per `AgentAdapter::system_prompt_file_flag`/`user_system_prompt_flag`). `ComposedPrompt::describe()` produces a one-line `"<version> layers: default+adapter+repo+..."` summary that gets attributed in the decision log, so a transcript can be traced back to exactly which layers shaped it.

## Data Flow

```mermaid
flowchart TD
    A[.zirv/*.yaml/json/toml] -->|file_to_script| B[Script]
    C[.zirv/.shortcuts.yaml] -->|Shortcuts| D[candidate_names_in_dir]
    D --> E[suggest_matches]
    F[CLAUDE.md, settings.json, nested CLAUDE.md] -->|collect_surfaces, capped| G[optimize findings]
    G -->|judgment_prompt| H[distiller_cmd child, restricted tools]
    H --> I[report: stdout + state copy 0600 + --out]
    J[DEFAULT_PROMPT] --> K[compose]
    L[adapter.base_system_prompt] --> K
    M["~/.zirv/system-prompt.md"] --> K
    N["repo/.zirv/system-prompt.md, capped+labeled"] --> K
    K --> O[exec.rs / wrap.rs / run_loop.rs / resume.rs]
    O --> P[agent child process]
```
