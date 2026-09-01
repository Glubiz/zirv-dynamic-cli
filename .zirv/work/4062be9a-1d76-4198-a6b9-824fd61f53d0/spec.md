# Specification

## Context

- This is an **Operate** surface: a supervised developer session needs to run
  Zirv's routine workflow gates and ordinary GitHub/Git operations without
  stopping for permission prompts. The audience is an operator relying on
  Zirv's shipped defaults, and the single job is "finish normal repository
  work unattended while harmful commands still stop."
- Zirv currently derives the harness-neutral base policy from
  `SHIPPED_POSTURE_ALLOW`/`SHIPPED_POSTURE_DENY`/`SHIPPED_POSTURE_ASK` in
  `src/commands/ctx/adapters/mod.rs`, then evaluates it in
  `src/commands/ctx/safety.rs`. Deny and ask rules are checked before allows,
  and repository policy can only narrow the shipped result.
- Claude receives a per-launch generated settings file from
  `src/commands/ctx/adapters/claude.rs`. Its `permissions.allow` and
  `sandbox.excludedCommands` currently consume the same generated reserved
  Zirv command list. That coupling is the constraint to remove: a command can
  be trusted at the Zirv dispatch layer while its repository-authored child
  payload still needs OS-sandbox containment.
- `zirv test` and `zirv verify` execute commands from repository-owned
  `.zirv/verify.toml`; `zirv frontend` may execute a repository-owned
  `package.json` script. These outer built-ins must be allowed, but their
  children must remain sandboxed.
- `gh` and plain `git push` already have safe base-policy forms, but their
  credential and network access causes Claude's sandbox to request an
  unsandboxed retry. The existing hook-side retry carve-out only covers
  read-only `gh`/Git forms, so common mutations such as `gh issue comment`,
  `gh pr create`, and a non-force push still prompt.
- Claude's generated sandbox filesystem settings currently allow writes only
  to `StateDir::mail()`. Routine Zirv gates also persist workflow,
  verification, frontend, telemetry, and related state elsewhere under
  `StateDir::root()`, so allowing the outer command without allowing the state
  root would replace a permission prompt with a sandbox write failure.
- The product truth is the existing classifier and launch-settings behavior;
  no new command capability, safety claim, UI, or visual content may be
  invented. The design thesis is: **silence is the normal state, and every
  remaining interruption must encode a concrete harmful effect**. The
  memorable signature is a three-way launch partition (allow, sandbox, ask),
  not a generic allow-all switch. The justified risk is excluding broad `gh`
  and `git push` command families from OS containment while relying on Zirv's
  already-attested deny/ask hook to stop harmful members. This refuses the
  category-default arrangement where every mutation prompts regardless of
  reversibility.

## Goals

- Allow `zirv test`, `zirv verify`, and `zirv frontend` at the safety-hook and
  Claude native-permission layers while keeping them out of
  `sandbox.excludedCommands`.
- Allow the resolved Zirv platform state root in Claude's generated sandbox
  filesystem write allowlist so sandboxed Zirv built-ins can persist their
  own state.
- Ship native allows and sandbox exclusions for `gh *` and `git push *`, so
  credential/network-dependent ordinary operations do not trigger an
  unsandboxed-retry prompt.
- Ship an explicit native allow for `git worktree *`; normal `worktree add`
  and non-force `worktree remove` remain sandboxed and prompt-free.
- Generalize the unsandboxed-retry boundary: an explicit sandbox-disabled
  retry of a command whose base verdict is `Allow` passes silently in both
  launch modes, unless the command is escape-sensitive (see Design 4).
  Operator evidence (transcript inventory, 2026-08-31 → 2026-09-01, 81
  transcripts across 7 projects): 349 of 358 live ask prompts (97.5%)
  matched the `<sandbox: unsandboxed retry>` rule on semantically harmless
  commands — zirv's own subcommands (`zirv workflow` 49, `zirv agent` 45,
  `zirv ctx status/send/inbox` 42, `zirv test/verify/frontend` 27),
  multi-repo `git -C … fetch/push` (39), `gh`/`glab` (45), test and lint
  runners (33), `cd <sibling worktree> && git …`, `cargo test` needing a
  real PTY, and `bash <scratchpad>/script.sh` — including fully read-only
  compounds the existing read-only carve-out failed to classify.
- Correct two deny-rule shapes that produced false-positive hard denies
  (see Design 6) without removing either deny family.
- Preserve the current precedence and verdicts for force/delete pushes,
  `gh auth`, `gh secret`, `gh repo delete`, `gh release delete`, `gh api
  DELETE`, `gh codespace ssh`, publishes, `zirv setup`, subprocess-launching
  `zirv ctx` verbs, posture-pinning worker launches, credential reads,
  recursive destructive deletion, privilege escalation, and pipe-to-shell.
- Preserve repository narrowing and all operator-owned override behavior; add
  no repository-settable or operator configuration keys.
- Raise the crate version to 3.6.0 (the branch merges main's 3.5.0 release,
  which closed the #222 issue number in a batch without shipping this
  posture work).

## Non-goals

- Do not turn off Claude or Codex sandboxing, weaken the default
  `dontAsk`/`never` posture, or add a blanket `zirv *` permission.
- Do not sandbox-exclude `.zirv/verify.toml` or `package.json` child commands,
  and do not trust repository-authored payloads merely because a reserved
  Zirv built-in selected them.
- Do not make `zirv setup` unattended or widen `zirv ctx exec`, `wrap`,
  `usage tee`, `agent`, `chat`, or `artifact --server-command`.
- Do not change the repository trust model, add config keys, or let repository
  policy widen `allow`/sandbox escape behavior.
- Do not redesign frontend output or run frontend render/review evidence. The
  name `zirv frontend` is affected only as a CLI permission family; no route,
  component, styling, interaction, responsive behavior, or accessibility
  surface changes.
- Do not change release automation, command parsing, script resolution, or
  the definition of the platform state directory.

## Design

### 1. Split reserved Zirv allows from sandbox exclusions

Replace the current one-list projection with two explicit products in
`src/commands/ctx/safety.rs`:

- The base/native-allow pattern set includes every currently safe reserved
  built-in plus `test`, `verify`, and `frontend`; it continues to expand
  `ctx` only through `ctx_base_allow_verbs`. `setup` remains absent.
- The sandbox-exclusion pattern set is derived from the base/native set but
  removes `test`, `verify`, and `frontend`. Existing exclusions for trusted,
  non-payload Zirv built-ins and safe `ctx` verbs remain unchanged.

Use the base/native set for `builtin_allow`,
`reserved_zirv_auto_allow_rule`, and Claude `permissions.allow`. Use only the
sandbox-exclusion set for `sandbox.excludedCommands`. Keep the installed-binary
and case-insensitive reserved-name checks, compound-command worst-verdict fold,
posture-pinning denies, and repo deny/ask precedence unchanged.

This is a deliberate partition, not a special case at the call site: future
reserved built-ins must choose whether their selected payload is trusted
enough to leave the OS sandbox instead of inheriting an exclusion accidentally.

### 2. Add command-family launch projections

In `src/commands/ctx/adapters/claude.rs`, append a small shipped family table
to the generated settings:

| Family | `permissions.allow` | `sandbox.excludedCommands` | Safety authority |
|---|---:|---:|---|
| `zirv test *`, `zirv verify *`, `zirv frontend *` | yes | no | repo deny/ask may narrow; child payload stays sandboxed |
| `zirv setup *` | no | no | unmatched/default ask |
| `gh *` | yes | yes | existing `gh` deny forms still win in the hook |
| `git push *` | yes | yes | existing force/delete push ask forms still win in the hook |
| `git worktree *` | yes | no | existing broad Git allow; ordinary add/remove stay sandboxed |

Use one source for the extra native permission/exclusion spellings rather than
duplicating raw strings between JSON construction and tests. The generated
rules supplement, not replace, `SHIPPED_POSTURE_ALLOW` and the per-mode
`--allowedTools`/`--disallowedTools` projection.

The broad `gh *` and `git push *` exclusions are safe only in combination with
the attested `zirv ctx safety check` PreToolUse hook. The hook evaluates the
plain command before execution, checks deny then ask then allow, and therefore
still denies harmful `gh` forms and prompts/denies force or delete pushes
according to launch mode. Tests must pin this dependency explicitly.

`git worktree remove --force` remains an ask because it can discard dirty
worktree changes; the acceptance path is `add` and ordinary `remove`, neither
of which is classified as destructive today. Keeping `git worktree` inside the
sandbox also avoids granting an unnecessary credential/network escape.

### 3. Allow the Zirv state root inside Claude's sandbox

Change `ClaudeAdapter::launch_settings_path` to resolve `StateDir` once and
pass `StateDir::root()` into `launch_settings_value`. Rename the parameter and
local variables from mail-specific terminology to state-root terminology, and
write that exact root as the generated filesystem `allowWrite` entry.

Resolution remains best-effort: if `StateDir::resolve` fails, omit
`allowWrite` while still generating the rest of the safety settings, matching
the current mail-dir failure behavior. Do not derive the root from the policy
snapshot path or from repository input. The immutable policy snapshot remains
under the operator-owned `~/.zirv/runtime/policies` path and is not added by
this rule.

This widens sandboxed subprocess write access from the mail subtree to Zirv's
whole machine-local state root, including verification/workflow/log state. It
is required for sandbox-confined Zirv built-ins to function without escape,
and it is bounded to the platform path Zirv itself resolves (or the operator's
explicit `ZIRV_CTX_STATE_DIR`). Repository files cannot configure that path.

### 4. Generalize the unsandboxed-retry boundary

Today the `--dangerously-disable-sandbox` retry branch in
`run_check_hook_mode_with_env` escalates every base-`Allow` command to
Ask (interactive) / Deny (headless) under `<sandbox: unsandboxed retry>`,
unless it hits one of four narrow carve-outs (scratchpad-confined write,
read-only `gh`, reserved escape-safe zirv built-in, read-only escape,
operator `escape_allow`). Operator-observed evidence shows this boundary
produces almost all irrelevant prompts, including on fully read-only
compounds the carve-outs fail to classify (`cd <sibling worktree> && git
ls-files … && git check-ignore …`).

Replace the final catch-all escalation with a general rule: when the base
verdict for the effective command is `Allow` (matched allow or
unmatched-mode-default allow), the retry passes silently under a new
built-in rule tag `<sandbox: allow-verdict retry>`, in BOTH interactive and
headless modes, PROVIDED every executable segment clears an
escape-sensitivity screen:

- No payload-carrying reserved zirv built-in: `zirv test`, `zirv verify`,
  `zirv frontend` (their repo-authored children must stay OS-confined; a
  retry of these keeps the current Ask/Deny escalation), `zirv setup`, and
  any non-reserved `zirv <script>` or directory-qualified `zirv` path.
- No `zirv ctx` verb outside `ZIRV_CTX_ESCAPE_SAFE_VERBS` (and no
  `usage tee`), no `zirv chat` (an interactive session takeover), no
  `zirv artifact --server-command`. Plain `zirv agent`, `zirv workflow`,
  and the other non-payload reserved built-ins DO pass this screen when
  their base verdict is `Allow`: their posture-pinning and self-referential
  spawn forms are deny families evaluated upstream, the spawned worker
  still receives its own generated launch posture, and the operator
  inventory shows these three names alone produced 136 of the 349
  irrelevant prompts. `is_reserved_zirv_escape_safe` remains the authority
  for the `ctx` verb split; the screen extends it rather than forking it.
- The command clears the same credential-path/root-scan screen
  `escape_allow_matches` already applies, reused, not reimplemented.

Review round 1 (finding b1c244e2) added a shell-content screen to the
escape-sensitivity screen: a segment invoking `sh`/`bash`/`zsh`/`dash` on a
script file has the file's contents decomposed by the same shell-AST
segmenter and every extracted segment must clear the deny-family/credential
screen, failing closed on unreadable, non-regular, or oversized (>128 KiB)
files; a `-c` inline string is screened the same way. This restores "deny
families still deny" for the shell-file indirection an unsandboxed retry
would otherwise smuggle past the text globs.

A base verdict of `Ask` (or a mode-default `Ask`/`Deny`) keeps today's
`<sandbox: unsandboxed retry>` escalation unchanged, and a semantic `Deny`
is still preserved with its more specific explanation. The four existing
carve-out rules remain first (their tags are load-bearing in logs and
tests); the new rule is the fallthrough before the escalation. Deny and
ask families — force/delete push, dangerous `gh`, credential reads,
privilege escalation, recursive destructive deletion, pipe-to-shell,
posture-pinning launches — are evaluated before this branch and therefore
still stop every harmful retry.

### 5. Compatibility and documentation

- No serialized schema or CLI syntax changes.
- No new config fields or environment variables.
- Native Windows keeps its existing no-OS-sandbox behavior; the shared safety
  policy and native permission projection still apply where supported.
- Codex keeps its existing `workspace-write`/`never` launch posture; this
  change primarily corrects Claude's generated settings and the shared safety
  verdicts.
- Bump `Cargo.toml` and the root `zirv` package entry in `Cargo.lock` to 3.5.0.
- Because this changes a ctx safety/adapter contract, update
  `docs/obsidian/Modules/Command Safety.md`,
  `docs/obsidian/Modules/Ctx Adapters.md`,
  `docs/obsidian/Concepts/Untrusted Configuration.md`, and the session
  Work Journal/Decision Log as required by the repository documentation
  contract, with behavior-page `last-verified` dates bumped.

### 6. Deny-rule shape corrections (false positives, families kept)

Both observed as live false-positive hard denies in the operator inventory;
both families stay, only their matching shape narrows:

- The `rm -rf` deny/ask glob that guards zirv paths currently matches the
  WHOLE compound command string, so `rm -rf <unrelated path>; zirv frontend
  check --help` is denied merely because "zirv" appears in a later segment.
  Scope the match to the `rm -rf` segment's own argument tokens.
- The `<network: piped into a shell interpreter>` deny currently catches
  purely local idioms such as `find … | xargs -I{} sh -c 'echo "== {} ==";
  cat {}'`. Require a network-fetching program (`curl`, `wget`, or
  equivalent) upstream of the shell-interpreter sink before the family
  matches; a pipe whose upstream segments are all local commands falls
  through to ordinary evaluation.

## Testing strategy

Write the regression tests before changing production behavior.

1. In `safety.rs`, replace the test that groups
   `setup`/`test`/`verify`/`frontend` together with tests proving:
   - `test`, `verify`, and `frontend` are base `Allow`, case-insensitively;
   - `setup` remains `Ask`;
   - repo/operator deny or ask rules still narrow the newly allowed names;
   - non-reserved `zirv <script>` remains `Ask`;
   - ordinary `gh` mutations, plain push, and normal worktree add/remove are
     `Allow`;
   - force/delete push forms keep their current `Ask`, dangerous `gh` forms
     keep `Deny`, and `git worktree remove --force` keeps `Ask`.
2. In `adapters/claude.rs`, update the generated-settings projection test to
   assert exact membership at both seams:
   - `permissions.allow` contains the three sandbox-confined Zirv names plus
     `Bash(gh *)`, `Bash(git push *)`, and `Bash(git worktree *)`;
   - `sandbox.excludedCommands` contains `gh *` and `git push *` but not the
     three Zirv names, `setup`, or `git worktree *`;
   - repo scripts, `zirv ctx exec`/`wrap`/`usage`, and blanket `zirv *` stay
     absent.
3. Update the filesystem-settings test to assert that `allowWrite` contains
   the exact state root, not merely `/state/mail`, and that the operator
   policy snapshot path is not separately allowlisted. Keep the no-resolved-
   state fallback test.
4. Exercise the PreToolUse rendering path for the newly excluded families:
   plain `gh`/push yields `allow`, dangerous `gh` yields `deny`, and force or
   delete push yields `ask` interactively and remains blocked headlessly.
   This proves native allow/exclusion rules do not erase hook precedence.
5. Preserve and run the existing escalation, credential-read,
   pipe-to-shell, posture-pinning, artifact server-command, reserved-script,
   and repo-narrowing regression suites.
6. For the generalized retry boundary, add tests proving an unsandboxed
   retry:
   - of `cd /some/unknown/dir && git status --short`, `git checkout --
     <file> && git status --short`, `cargo test --bin zirv foo -- --test-
     threads=1`, `cargo nextest run --no-fail-fast`, `bash
     <scratchpad>/script.sh 2>&1 | tail -60`, and a multi-line gate script
     (newline-separated statements: `set -e`, `echo` banners, `cargo build
     2>&1 | tail -20`, `cargo fmt -- --check 2>&1`, `echo "fmt exit: $?"`)
     allows silently in both modes under `<sandbox: allow-verdict retry>`;
   - of a command the shell AST cannot segment keeps the current
     escalation (no silent pass for unparseable input);
   - of `zirv test changed`, `zirv verify`, `zirv frontend build` keeps the
     current Ask/Deny escalation (payload confinement survives);
   - of `zirv workflow status`, `zirv agent codex "x"`, and `zirv ctx
     status --brief --diff` allows silently (their posture-pinning/self-
     referential deny forms unchanged);
   - of `zirv ctx exec -- <cmd>`, `zirv chat`, `zirv ctx usage tee --
     <cmd>`, and a non-reserved `zirv <script>` keeps the current
     escalation;
   - of a base-`Ask` command (e.g. a force push) and of every deny-family
     command is unchanged;
   - of a command naming a credential path fails the screen and keeps the
     escalation;
   - of `bash <scratchpad>/script.sh` whose contents are benign allows
     silently, while screened contents (`cat ~/.ssh/id_rsa`), a missing
     file, and a screened `-c` inline string all keep the escalation.
7. For the deny-shape corrections: `rm -rf <unrelated>; zirv frontend check
   --help` no longer trips the zirv-path deletion deny while `rm -rf
   <zirv path>` still does; `find … | xargs sh -c '…'` no longer trips
   pipe-to-shell while `curl … | sh` and `wget … | xargs sh -c '…'`
   still do.
8. Run all required gates in the foreground after implementation:

       cargo build
       cargo nextest run --no-fail-fast
       cargo test --verbose -- --test-threads=1
       cargo fmt -- --check
       cargo clippy --all-targets -- -D warnings

   If a test gate fails, compare the complete sorted failing-test name list
   against the base branch in the same environment; counts alone are not
   evidence.

## Risks

- **Native allow/exclusion outruns semantic safety.** A broad `gh *` or
  `git push *` native rule is dangerous if the PreToolUse hook is absent.
  Mitigation: generated settings continue to attest the hook in the same
  launch-local file and fail closed on sandbox initialization; regression
  tests pin hook installation and harmful-family verdicts alongside the new
  projection.
- **Repository payload escapes through a trusted outer Zirv name.** Mitigation:
  `test`, `verify`, and `frontend` are present only in the base/native allow
  set, never the sandbox-exclusion set; tests assert both halves together.
- **State audit data becomes writable from any sandboxed child.** Allowing the
  state root is broader than the current mail-only carve-out. Mitigation: use
  only `StateDir::resolve`/`root`, never repository input; keep the immutable
  policy snapshot outside that root; retain OS containment for repository
  files, credentials, and unrelated machine paths. This is an explicit
  product tradeoff required by prompt-free sandboxed Zirv gates.
- **Dangerous subcommand spellings bypass narrow rules.** Mitigation: retain
  the existing reordered/short-flag/refspec tests for push and exact dangerous
  `gh` families; add generated-settings tests but do not reimplement semantic
  parsing in the adapter.
- **Opaque interpreter payloads on the retry path.** The shell-content
  screen is text-level parity, not content proof: a nested `bash inner.sh`
  line inside a screened script passes the glob layer exactly as it would
  inline, and non-shell interpreters (`python`, `node`, `php` vendor
  binaries) execute file contents shell deny globs cannot read. Screening
  those would reintroduce the prompt classes the operator explicitly
  ordered removed (phpunit/artisan runners were 10 of the 349 observed
  prompts), so they retain silent Allow-verdict retries by explicit
  operator tradeoff, recorded here.
- **Ordinary mutating commands now run unsandboxed silently after a retry.**
  This is the deliberate product change (operator directive 2026-09-01: only
  truly harmful commands prompt). Mitigation: deny/ask families are evaluated
  before the retry branch and still stop every harmful form; the
  credential/root screen is reused verbatim; payload-carrying zirv names and
  subprocess-launching ctx verbs keep the escalation; the harness only sets
  the retry flag after an actual observed sandbox failure, so the base
  sandboxed posture remains the default path.
- **A future reserved built-in is projected into the wrong layer.** Mitigation:
  make the permission/exclusion split named and test every reserved command,
  so a new name requires an explicit classification rather than inheriting
  both layers silently.
