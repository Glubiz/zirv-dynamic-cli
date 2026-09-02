# Cross-harness command permission policy — design

Date: 2026-08-24
Status: approved by operator (chat), pending spec review
Target version: 2.26.0

## Problem

The operator runs agent sessions through zirv across multiple harnesses
(Claude Code, Codex). Today zirv's shipped Claude posture pins
`--permission-mode dontAsk`, so any tool call not on the allowlist is
silently denied ("dies") instead of prompting, and the safety hook's `Ask`
verdicts are deliberately suppressed under dontAsk (issue #102). Codex
interactive launches ship `--ask-for-approval never` and ignore the
`SafetyPolicy` entirely. The operator wants, without configuring Claude or
Codex themselves:

1. Read-only work (including `SELECT` SQL) to run silently.
2. Mutating/destructive commands to **prompt for permission**.
3. What is denied today to **ask instead of dying**.
4. Every zirv-wrapped harness to act the same, from one zirv-owned policy.
5. The whole tool surface governed (not just shell commands).

## Primary acceptance criterion (operator amendment, 2026-08-24)

> The endless permission prompts are THE pain point zirv must fix for every
> wrapped harness. Only truly dangerous commands may prompt; an arbitrary
> read command (or everyday dev command) must NEVER prompt — including
> commands zirv has never seen.

This outranks every other requirement in this document. Where a design choice
below trades prompt volume against classification coverage, prompt volume
wins: a family zirv has not classified is `allow` on an interactive launch,
not `ask`. The `ask` set is deliberately a short list of genuinely dangerous
and irreversible families, not a general "might mutate something" net.

Two consequences are load-bearing and are reflected in the sections below:

- A **finite allow-list under a prompting permission mode fails this
  criterion**, because every novel command falls off the end of the list and
  prompts. The interactive projection must therefore be inverted: allow
  broadly, and let zirv's own classifier be the thing that prompts.
- The interactive **unmatched-command verdict is `allow`**, not `ask`.
  Headless is unchanged and stays fail-closed, because nobody is present to
  answer and an unclassified command is genuinely unsafe to run unattended.

## Approach (chosen: "C — keep both models, sharpen the seam")

zirv already has two half-built instances of this feature:

- `src/commands/ctx/safety.rs` — harness-neutral `Allow/Ask/Deny` command
  classifier (issue #83): builtin/operator/repo layering, glob matcher over
  normalized command segments, `zirv ctx safety check|list|explain` CLI,
  wired as a Claude `PreToolUse`/`Bash` hook by `setup.rs`
  (`CLAUDE_SAFETY_HOOK`), `hook_output` emitting `permissionDecision`.
  `builtin_ask()` is empty today.
- `src/commands/ctx/policy.rs` — capability-stance model (issue #43):
  `Capability × Stance{Allow,Ask,Deny}` with the honesty contract
  (`Support::{Enforced,Degraded,Unsupported,OperatorControlled}` per
  adapter, never claimable from prompt text), `max`-fold narrowing,
  fail-closed degrade on config-load failure.

They stay separate, with sharpened roles:

- **`policy.rs` = coarse layer, all tools.** Per-capability stances
  (`RepoFsWrite`, `OutsideRepoFsWrite`, `ShellExec`, `Network`,
  `GitPushDestructive`, `ToolAccess`, …) govern the full tool surface on
  every harness, honestly reported per adapter.
- **`safety.rs` = fine-grained layer inside `ShellExec`.** Command-string
  classification, now including destructive-command `ask` built-ins and a
  SQL statement classifier.
- **The new work concentrates in the projection seam**
  (`adapters::policy_launch_args` → each adapter's `default_sandbox_args`
  / `policy_args`): compile the two-layer policy into each harness's
  native mechanism at launch, differently for interactive vs headless.

## Built-in defaults (zero-config UX)

Shipped classification; the operator writes config only to disagree.

Shell commands (`safety.rs`):

| Class | Verdict |
| --- | --- |
| Read-only commands (existing `builtin_allow()` posture) | allow |
| SQL: `SELECT` / `EXPLAIN` / `SHOW` via a recognized DB CLI | allow |
| SQL: anything else, chained, CTE-wrapped, stdin-fed, unparseable | ask |
| Everyday mutating dev commands (`cargo build`, `npm install`, `git commit`, `mkdir`, in-repo file writes, `curl`/`wget` on their own) | allow |
| **A command matching no rule at all, INTERACTIVE** | **allow** (new `interactive_default`) |
| A command matching no rule at all, headless | ask (existing `default`, unchanged) |
| Genuinely dangerous shell: `rm -rf`-class, force-push / history rewrite, `git reset --hard`, `git clean`, `find -delete`, `taskkill`/`Stop-Process`/`pkill`/`killall`, `Remove-Item -Recurse`, `dd`/`mkfs`/`mkswap`, `diskpart`/`fdisk`/`format`, `reg delete`, `shutdown`/`reboot` | ask (new `builtin_ask()` content) |
| Self-destructive and irreversible: `taskkill *zirv*`-class (kills the supervising session), destructive ops on zirv state dirs, a download piped into a shell, credential-file reads, `cargo`/`npm publish`, `gh repo`/`release delete` | deny (existing deny posture, extended) |

The `ask` row is deliberately a **short, closed list of genuinely dangerous
and irreversible families**, not a general "might mutate something" net. Per
the primary acceptance criterion, a family that is merely mutating —
installing dependencies, building, committing, creating a directory, writing
a file inside the repo — is `allow`, and so is anything unclassified on an
interactive launch. `curl`/`wget` move from `deny` to `allow` for the same
reason (fetching a URL is everyday work); the actual danger, a download piped
straight into a shell, is closed by an explicit `* | sh` / `* | bash` deny
entry instead of by denying the fetch tools themselves.

Capabilities (`policy.rs` defaults for interactive sessions):

| Capability | Default stance |
| --- | --- |
| Repo file edits | allow |
| Writes outside the repo | ask |
| Network fetch | allow |
| Plain `git push` | allow |
| Force-push / history rewrite | ask (via safety built-ins) |
| Shell exec | governed by `safety.rs` classification |

## Per-harness projection

**Claude Code, interactive** (chat, wrap, dash panes):
`--permission-mode dontAsk` → `--permission-mode default`, **with the
allow-set inverted**.

A finite `--allowedTools` list under a prompting mode fails the primary
acceptance criterion outright: every command not on the list — which is every
command zirv has never seen — prompts. So the interactive projection
blanket-allows `Bash` alongside the everyday tool surface, and the
`CLAUDE_SAFETY_HOOK` (`zirv ctx safety check`) becomes the **sole prompting
gate**:

- The hook emits an explicit `"allow"` decision for an `Allow` verdict
  (previously it emitted nothing and fell through to claude's own flow),
  `"ask"` for `Ask`, `"deny"` for `Deny`.
- The interactive unmatched-command verdict is `allow`
  (`safety.interactive_default`, operator-overridable, `REPO_FORBIDDEN`), so a
  novel command is silently allowed rather than prompted.
- The deny set stays in `--disallowedTools`, where a permission rule beats
  any hook decision — verified live, and the one place that ordering is
  wanted.

**Hook contract this relies on, to be verified before implementing:** that a
PreToolUse hook's `"ask"` decision still forces a prompt for a tool the
launch natively allowed. Claude's own docs say "hook decisions don't bypass
permission rules", which is unambiguous for `deny` beating `allow` and
ambiguous in this direction. The implementation plan must verify it live
against the installed CLI before relying on it, and carries a recorded
fallback if it does not hold: drop the blanket `Bash` allow, and let the
hook's own explicit `"allow"` decision carry everyday commands instead — a
shape that satisfies the criterion without depending on the contract at all.

The issue-#102 Ask-suppression remains ONLY for launches that actually run
under `dontAsk`: zirv's own headless launches, and an operator whose trailing
flags pin it (`flags_pin_policy` already detects operator pins; zirv never
overrides an explicit operator choice).

**Claude Code, headless** (exec, run_loop workers): unchanged — nobody is
present to answer a prompt, so the fail-closed dontAsk posture stays.

**Codex, interactive**: `--ask-for-approval never` → **`on-request`**, paired
with the existing `--sandbox workspace-write`.

Deliberately **not** `untrusted`, which was this document's first answer and
is the wrong polarity: `untrusted` prompts for everything outside codex's own
narrow built-in trusted set, which is precisely the endless-prompting failure
the primary acceptance criterion exists to remove. `on-request` lets the
session work freely inside the workspace sandbox and escalate only when it
needs to leave it — the lowest-noise posture that still gates real damage.
Probe-verified against the installed CLI's own `--help` exactly like the
existing `--ignore-rules` probe; on any doubt at all the launch keeps
`never`, because an unrecognized argument breaks the launch outright.

Codex has **no per-command mechanism** to receive zirv's `[safety]`
classification (no trusted-command configuration was verified to exist on the
installed CLI), so `CodexAdapter::default_sandbox_args` still projects no
`SafetyPolicy` rules at all — never a guessed `-c` override. `policy_support`
reports `Degraded` and says exactly why: **the sandbox contains the damage,
and approval granularity is codex's own, not zirv's per-command
classification** — so read-only-SQL silence and everyday-command silence are
not carried onto this harness the way they are onto claude. Never fake
parity.

**Codex, headless**: unchanged fail-closed.

## SQL classifier

Lives in `safety.rs` beside the existing normalizer, pure like the rest of
the module. Recognizes common DB CLIs (`psql`, `mysql`, `mariadb`,
`sqlite3`, `duckdb`, `sqlcmd`), extracts statement text from their
argument shapes (`-c`, `-e`, positional), and classifies:

- Upgrade to **allow** only when the entire input is provably read-only:
  a single statement matching `SELECT`/`EXPLAIN`/`SHOW` with no `INTO`,
  no statement chaining, no CTE that wraps a write.
- Everything else — including stdin-fed SQL it cannot see — stays at
  **ask**. The worst case is always an unnecessary prompt, never an
  unprompted write. Non-goals mirror `Command Safety.md`'s existing
  stance: this is a classifier raising the bar, not a SQL parser and not
  the only defense.

## Configuration and trust layering (unchanged patterns)

- Overrides go in the existing `[safety]` (patterns) and `[policy]`
  (stances) tables of `~/.zirv/ctx.toml`. No new file. All new keys
  optional — an unconfigured operator gets the new defaults.
- Two new optional `[safety]` keys, both `REPO_FORBIDDEN` because both can
  only ever loosen the effective policy: `interactive_default` (the
  unmatched-command verdict on an interactive launch, default `allow` — the
  existing `default` still governs headless and stays `ask`) and `sql`
  (`on`/`off`, default `on`).
- Layering reuses the three established folds verbatim: `deny`/`ask`
  additive across builtin+operator+repo (repo may only narrow);
  `allow`/`default` operator-home-only (`REPO_FORBIDDEN` from a repo
  layer); `ZIRV_CTX_*` env above the fold; config-load failure degrades to
  the existing fail-closed policy.
- Introspection: `zirv ctx safety explain <cmd>` (exists) answers "what
  would happen and which rule says so"; the policy report shows the
  effective posture per harness with per-capability `Support` provenance.

## Testing

- **The acceptance corpus — the primary criterion expressed as a test, and
  the gate this whole change is judged on.** Roughly thirty everyday and
  deliberately-novel commands (`cargo build`, `npm install`, `git commit -m
  x`, `mkdir -p src/x`, `ls`, `rg foo`, `curl https://api.example/health`,
  `some-tool-zirv-has-never-heard-of --flag`, …) must classify `Allow` on an
  interactive launch and must NEVER classify `Ask`; the dangerous corpus must
  classify `Ask`. A regression here is a product regression, not a test
  failure.
- Pure classifier unit tests, including an adversarial SQL corpus
  (CTE-wrapped `INSERT`, `;`-chained, `SELECT ... INTO`, comment tricks,
  stdin) — all must classify ask.
- Adapter argv tests asserting the interactive/headless posture split per
  harness; never assert argv that depends on an installed-binary probe
  (assert the invariant).
- Issue-#102 suppression tests updated: suppression only under an
  operator-pinned dontAsk.
- Full gates: build, nextest, serial fallback, fmt, clippy; Linux/Docker
  run (cfg(unix) surfaces); the operator's Docker AI-feature command
  matrix before the PR (harness-facing change — Docker is denied in the
  current session's permissions, so this run needs the operator).

## Rollout

- Version 2.26.0 (user-facing behavior change).
- Binary installed before any new config keys are used (old binaries
  hard-fail on unknown keys; all new keys optional so the unconfigured
  path needs no coordination).
- Vault updates: `Modules/Command Safety.md`, `Modules/Ctx Adapters.md`,
  `Concepts/Untrusted Configuration.md`, decision log + journal.

## Out of scope

- Sessions launched outside zirv (zirv only governs what it launches).
- A zirv-dashboard approval surface (deliberately deferred; the native
  harness UI is the prompt surface — operator decision, this chat).
- Full shell/SQL parsing, obfuscation-proof classification.
- Persisting into the harness's own config files (`.claude/settings.json`
  permissions, `~/.codex/config.toml`): projection stays live per-launch
  argv, the operator's own files stay untouched (existing principle).
