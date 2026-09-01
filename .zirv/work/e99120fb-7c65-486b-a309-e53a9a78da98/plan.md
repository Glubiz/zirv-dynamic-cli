# Plan

Implementation happens on branch `feat/222-default-prompt-free-posture`
(worktree off `main` @ 52d2cdd). Tests are written failing-first per task;
production code follows in the same task. The spec's Design sections are the
authority; this plan only sequences them.

## Task 1 — Reserved-name partition (spec Design 1)

In `src/commands/ctx/safety.rs`:

- Failing tests first: `test`/`verify`/`frontend` are base `Allow`
  (case-insensitive); `setup` stays `Ask`; repo/operator deny-or-ask still
  narrows the new allows; non-reserved `zirv <script>` stays `Ask`.
- Implement the split: one base/native-allow pattern set (adds `test`,
  `verify`, `frontend`; still expands `ctx` via `ctx_base_allow_verbs`; no
  `setup`) and one sandbox-exclusion set derived from it minus the three
  payload-carrying names. Feed `builtin_allow`,
  `reserved_zirv_auto_allow_rule`, and Claude `permissions.allow` from the
  first; `sandbox.excludedCommands` only from the second.

## Task 2 — Command-family launch projections (spec Design 2)

In `src/commands/ctx/adapters/claude.rs`:

- Failing tests first: exact membership at both seams per the spec's family
  table (`gh *` and `git push *` in both allow and exclusion; the three zirv
  names and `git worktree *` in allow only; `setup`, repo scripts,
  `ctx exec/wrap/usage`, blanket `zirv *` absent from both).
- Implement from one shared source of spellings; keep
  `SHIPPED_POSTURE_ALLOW` and per-mode tool projections untouched.
- Safety tests: ordinary `gh` mutations, plain push, worktree add/ordinary
  remove are `Allow`; force/delete push `Ask`; dangerous `gh` families
  `Deny`; `git worktree remove --force` `Ask`.

## Task 3 — State-root sandbox write allow (spec Design 3)

In `adapters/claude.rs`: resolve `StateDir` once in `launch_settings_path`,
pass `StateDir::root()` into `launch_settings_value`, rename mail-specific
naming to state-root naming, write the root as the filesystem `allowWrite`
entry. Keep best-effort omission on resolve failure. Update the filesystem
settings tests: exact state root present, policy-snapshot path absent,
no-resolve fallback retained.

## Task 4 — Generalized unsandboxed-retry boundary (spec Design 4)

In `safety.rs`, retry branch of `run_check_hook_mode_with_env`:

- Failing tests first, per spec Testing 6: silent `Allow` (both modes,
  tag `<sandbox: allow-verdict retry>`) for retried `cd <unknown dir> &&
  git status --short`, `git checkout -- <file> && git status --short`,
  `cargo test … -- --test-threads=1`, `cargo nextest run --no-fail-fast`,
  `bash <scratchpad>/script.sh 2>&1 | tail -60`, and — per spec Design 4's
  screen — `zirv workflow status`, `zirv agent codex "x"`, `zirv ctx status
  --brief --diff`; unchanged escalation for retried `zirv
  test/verify/frontend`, `zirv ctx exec`/`usage tee`, `zirv chat`,
  non-reserved scripts, base-`Ask` commands, deny families, and
  credential-path commands.
- Implement the fallthrough rule after the four existing carve-outs: base
  verdict `Allow` + escape-sensitivity screen (no payload-carrying or
  subprocess-launching zirv segment; reuse `is_reserved_zirv_escape_safe`
  authority) + the existing `escape_allow` credential/root screen, reused.
- Keep existing carve-out rule tags and ordering; preserve semantic `Deny`
  explanations; headless base-`Ask` retry still denies.

## Task 5 — Deny-rule shape corrections (spec Design 6)

In `safety.rs`, failing tests first per spec Testing 7:

- Scope the zirv-path `rm -rf` deny glob to the `rm -rf` segment's own
  argument tokens so unrelated compounds mentioning zirv later stop
  tripping it; direct `rm -rf <zirv path>` still denies.
- Require a network-fetching upstream segment before
  `<network: piped into a shell interpreter>` matches; local
  `find | xargs sh -c` idioms fall through, `curl … | sh` still denies.

## Task 6 — Version, docs, memory

- Bump `Cargo.toml` (and `Cargo.lock` root entry) 3.4.0 → 3.5.0.
- Obsidian vault updates are waived by operator ruling (2026-08-27: vault
  retired); instead record the new posture via `zirv ctx remember`
  (partition model, new retry rule tag, state-root allowWrite).
- PR description documents the three-way partition table and the retry
  boundary change with the operator evidence (2026-09-01 prompt inventory).

## Gates

All five, foreground, in the worktree, after all tasks:

    cargo build
    cargo nextest run --no-fail-fast
    cargo test --verbose -- --test-threads=1
    cargo fmt -- --check
    cargo clippy --all-targets -- -D warnings

Failures are judged by the complete sorted failing-test NAME diff against
`main` built in the same worktree environment, never counts. Then the
workflow's own test/review/verify gates run (`zirv workflow advance` /
`zirv workflow review run`); the workflow review gate is the single review
round for this change.
