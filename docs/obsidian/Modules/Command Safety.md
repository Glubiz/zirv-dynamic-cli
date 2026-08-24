---
last-verified: 2026-08-24
---

# Command Safety

## Quick Reference

- **Files:** `src/commands/ctx/safety.rs`
- **Used by:** [[Ctx Subsystem]] (`zirv ctx safety check|list|explain`, dispatched from `CtxVerb::Safety`), [[Ctx Adapters]] (`ClaudeAdapter::default_sandbox_args` projects `SafetyPolicy` onto `--allowedTools=`/`--disallowedTools=` and attests the hook on every launch; `CodexAdapter::default_sandbox_args` receives the same policy but has no per-command mechanism and ignores it), `src/commands/setup.rs` (`zirv setup apply` also wires the hook persistently for non-Zirv launches)
- **Depends on:** [[Ctx Subsystem]] for `CtxConfig`/`EnvLookup`/`CtxResult`, [[Ctx Adapters]] for `LaunchMode` and `adapters::SHIPPED_POSTURE_ALLOW`/`_ASK`/`_DENY` (the built-in rule set is derived from these, not duplicated)
- **Tests:** inline `#[cfg(test)] mod tests` in `safety.rs` — the primary gate is `the_product_requirement_no_everyday_or_novel_command_ever_prompts`, paired with `the_product_requirement_only_genuinely_dangerous_commands_prompt`; the suite also covers mode-specific defaults, the shipped sets, normalization bypasses, the SQL classifier's adversarial corpus, repo narrowing, hook output and mode-aware `explain`; adapter/setup tests pin the launch projection and hook installation
- **If changed:** [[Ctx Subsystem]], [[Ctx Adapters]], [[Untrusted Configuration]], [[Decision Log]]
- **Gotchas:** `evaluate`/`glob_match` are pure — no clock, filesystem or environment access, the same discipline `rot.rs` holds its scoring functions to (see that page). `resolve` (the layering step) takes its environment as an injected closure, exactly like `policy::resolve`, and is where the `ZIRV_CTX_SAFETY_*` overrides are read — never inside the matcher. The wired `PreToolUse` hook always exits 0; the deny/ask decision travels in a structured `hookSpecificOutput` JSON envelope on stdout, not the process exit code (see below for why).

## Purpose

> "The endless permission prompts are THE pain point zirv must fix for every wrapped harness. Only truly dangerous commands may prompt; an arbitrary read command (or everyday dev command) must NEVER prompt — including commands zirv has never seen."

That operator acceptance criterion outranks classifier coverage. The built-in ask set is therefore a short, closed list; widening it is a product decision, not a hardening reflex. `the_product_requirement_no_everyday_or_novel_command_ever_prompts` is the executable gate: ordinary reads and mutations (`cargo build`, `npm install`, `git commit`, `mkdir`, in-repo writes, plain `curl`/`wget`) plus arbitrary unknown commands must all avoid `Ask` on an interactive launch.

Every harness zirv wraps has its own, incompatible way of deciding whether a command is safe to run unattended — claude's permission globs plus hooks, codex's sandbox/approval flags plus `.rules` execpolicy files. `safety.rs` gives zirv one harness-neutral classification. Claude receives its per-command verdicts through the wired `PreToolUse` hook; codex has no verified per-command projection, so its adapter reports that gap honestly and relies on its sandbox/approval boundary instead.

This builds on, rather than replaces, the foundation PR #96 shipped: `adapters::SHIPPED_POSTURE_ALLOW`/`_DENY` (the live-verified claude permission-rule list), `sandbox.extra_allow`/`extra_deny`, and `policy_launch_args`/`flags_pin_policy` wired into every launch seam (see [[Ctx Adapters]]). `[safety]` is the harness-neutral declaration and pure evaluator that was still open; the shipped posture constants are now the *source* the built-in default rule set is derived from, not a separate hand-maintained list.

## The model

```rust
pub enum Verdict { Allow, Ask, Deny }

pub struct Rule { pattern: String, origin: Origin }   // Origin: BuiltIn | Operator | Repo | Env

pub struct SafetyPolicy {
    deny: Vec<Rule>,
    ask: Vec<Rule>,
    allow: Vec<Rule>,
    default: Verdict,              // headless unmatched command
    interactive_default: Verdict,  // interactive unmatched command
    sql: SqlMode,
}
```

`evaluate(policy, command, mode) -> Outcome { verdict, matched: Option<Rule> }` checks `deny`, then `ask`, then `allow`, first pattern match within a category wins; a command matching nothing gets `policy.default_verdict(mode)` with no matched rule. A matched rule is never softened by launch mode.

### Three verdicts, two defaults

The verdict vocabulary is shared, but an unmatched command deliberately has two answers:

| Launch | Config key | Shipped default | Consequence |
|---|---|---|---|
| Headless | `safety.default` | `ask` | Nobody can answer, so claude's `dontAsk`/codex's `never` posture fails closed. |
| Interactive | `safety.interactive_default` | `allow` | The operator is present, but an unknown command is not dangerous merely because zirv has never catalogued it; it runs silently. |

This asymmetry is the design, not a compatibility exception. Sharing the headless `ask` default would recreate endless prompting; sharing interactive `allow` with headless work would remove the fail-closed automation boundary. Deny always beats ask and allow because category order is fixed, not because of list ordering.

`glob_match(pattern, text)` supports only `*` (any run of characters, including none), case-sensitive, matched against one already-normalized command string (see below). It is the standard iterative two-pointer `fnmatch`-style algorithm with a saved star position, not recursive backtracking: a command string can originate from repository-influenced text (a prompt-injected shell command an agent was talked into proposing), so the matcher must not be a stack-depth or exponential-blowup DoS surface. Worst case is `O(pattern * command)` with no recursion.

### Structural executable-node analysis, and its explicit non-goals

`evaluate` does not treat `command` as one opaque string. Inspired by Dippy's strongest transferable property—visit every executable node and fold the most restrictive result—`normalize_segments(command)` derives a bounded candidate set and `evaluate_candidates` applies both ordinary policy matching and SQL classification to every candidate before taking `deny > ask > allow`:

1. The raw command, unmodified — always checked first, so a pattern written against the *whole* string (there are none in the shipped `deny` set today, but a repo/operator `[safety]` entry could still be one) keeps matching exactly as before this fix.
2. One entry per quote-aware shell-separator segment (`split_segments`: `;`, `&&`, `||`, `|`, newline), whitespace-collapsed (`collapse_whitespace`) and leading-directory-stripped (`strip_program_dir`, so `/usr/bin/rm -rf /` and `rm -rf /` compare identically). A separator inside quoted data is not treated as executable syntax.
3. Recursively unwrapped inline shells (`unwrap_shell_wrapper`: `sh`/`bash`/`zsh` flags containing `c`, `cmd /c`, PowerShell `-Command`), each split and normalized again.
4. Executable text inside `$()` and legacy backtick substitutions, including substitutions inside double quotes; single-quoted lookalikes remain inert data.

Depth is capped at 16 and the candidate set at 128, so repository-influenced hook input cannot grow recursion or work without limit. These ceilings are a parser-availability boundary, not a permission widening: Claude's/Codex's OS sandbox remains the containment layer beneath the semantic tripwire.

- **No encoding/obfuscation awareness.** Base64-encoded payloads piped into `sh`, `eval "$(...)"`, environment-variable reassembly of a command string, and similar obfuscation are not decoded or evaluated — the matcher only ever compares literal text.
- **No `eval`/`exec`/`source`/`.`-style indirection.** A command that reads and runs a file's contents at runtime is invisible to a static string matcher by construction; this is the same limit every static-glob classifier (claude's own `permissions.allow`/`deny`, codex's `.rules`) already has.

A determined adversarial actor can still express a destructive effect through an allowed interpreter or dynamic script in a form this literal analyzer cannot recognize. `[safety]` is therefore a high-signal classifier, never described as the security boundary; OS sandboxing and native permission rules contain what an unrecognized command can reach. Native Windows Claude is the honest exception because Claude's OS sandbox is unsupported there—see [[Known Issues]].

## The built-in default rule set

`builtin_deny()`/`builtin_ask()`/`builtin_allow()` derive from `adapters::SHIPPED_POSTURE_DENY`/`_ASK`/`_ALLOW` by stripping each entry's `Bash(...)` wrapper (`command_pattern_from_bash_rule`). Non-`Bash` file/tool rules remain an adapter concern and are skipped by the command classifier. `SafetyPolicy::default()` combines those three lists with `default: Ask`, `interactive_default: Allow`, and `sql: On`.

`SHIPPED_POSTURE_ASK` is split from `_DENY` by **reversibility, not danger**. Recoverable-but-dangerous families such as force-push, hard reset, recursive deletion, destructive `find`, process termination, device formatting, registry mutation and shutdown ask before running interactively. Irreversible publication/deletion, credential exfiltration, privilege escalation and attacks on zirv's own supervisor/config stay denied. In particular, `taskkill*zirv*`, `Stop-Process*zirv*`, `pkill*zirv*`, `killall*zirv*` and zirv-targeting recursive deletes are specific deny entries; because evaluation walks all deny rules before ask rules, they beat the broader process-termination/deletion asks without a fragile within-list ordering rule.

Plain `curl` and `wget` moved to allow: fetching an API response or fixture is everyday development work. The actual high-risk shape is denied directly as `* | sh`, `* | bash` and `* | zsh` (including no-space pipe variants). Similarly, `find -exec grep` and `reg query` stay silent; only delete-executing `find` forms and registry mutation (`reg delete`/`add`/`import`) ask. Adding another ask family means adding an operator interruption and must be justified as a product change.

Order is preserved through derivation so the headless Claude projection can concatenate deny and ask as one fail-closed disallow set while the interactive projection keeps ask out of both native lists for the hook to decide.

**Issue #98 (2026-08-23):** the injected prompt (`prompt.rs`'s `HARNESS_PROMPT`, `ORCHESTRATOR_PROMPT` in `adapters::claude`) mandates `zirv ctx status`/`inbox`/`send`/`nudge`/`remember`/`recall`, `zirv agent <name> "..."`, and `zirv <script>` — but `SHIPPED_POSTURE_ALLOW` had no entry for `zirv` at all, so every one of those mandated commands was silently denied under `dontAsk`. A prompt must never mandate a command family the shipped posture denies. `SHIPPED_POSTURE_ALLOW` now carries `Bash(zirv *)` (the same trust class as the already-allowed `make build`/`npm run build`, since `zirv <script>` runs repo-defined commands) plus `Bash(cargo fmt *)`/`Bash(cargo clippy *)` next to the existing `cargo build`/`test`/`check` entries. `prompt_mandated_zirv_commands_are_allowed_by_the_shipped_posture` (in `safety.rs`'s test module) pins every prompt-mandated command as `Allow`, and still asserts a destructive family unrelated to `zirv` (`git push --force`, `rm -rf`) stays `Deny` — deny still wins over the new broad allow.

**Issue #111 (2026-08-23), argument-reordering and sibling-utility bypasses:** PR #107's review found the round-4 `git push`/`git reset` deny entries were flag-anchored (`Bash(git push --force *)`), so `git push origin --force` (the flag reordered away from the front) slipped past untouched, as did the short-flag spellings (`-f`/`-d`), an empty-src refspec delete (`git push origin :branch`), and a force-refspec push (`git push origin +branch`). Those entries are now mid-string-wildcard patterns (`git push*--force*`, `git push* -f *`/`git push* -f`, `git push*--delete*`, `git push* -d *`/`git push* -d`, `git push* :*`, `git push* +*`, `git reset*--hard*`) — `glob_match` already supports `*` anywhere, not only as a suffix, and `git push*--force*` also covers `--force-with-lease` since that flag's own text contains `--force`. The same reordering gap existed for `find`'s own `-delete`/`-exec`/`-ok` actions (the `find *` allow was read-only only up to those), for `head`/`tail`/`diff` reading the same credential paths `cat`'s four deny entries already covered, and for three `gh` escapes (`gh api -X DELETE`/`--method DELETE`, `gh secret`, `gh codespace ssh`) — all closed the same way. `evaluate_deny_survives_argument_reordering_issue_111` (`safety.rs`'s test module) pins every bypass form to `Deny` and confirms ordinary uses (`git push -u origin x`, `find . -name foo.rs`, `gh pr create --fill`) stay `Allow`. As `adapters::SHIPPED_POSTURE_DENY`'s own doc comment now states explicitly: with arbitrary-code toolchains (`python *`, `node *`, ...) allowed, this list is a tripwire for named destructive/credential command families, not a security boundary.

## Layering (`resolve`)

`[safety]` cannot use the ordinary deep merge (a later layer's array would replace an earlier one's) or `REPO_FORBIDDEN`'s all-or-nothing rejection (a repo must be able to *narrow* the policy). So `safety::resolve` folds the layers the way `policy::resolve` folds `[policy]` — lifted whole out of `ctx.toml` by `CtxConfig::load` (both `home_safety`/`repo_safety` removed via the same `SAFETY_SECTION` constant `POLICY_SECTION` already uses) before the ordinary deep merge, and is itself a `#[serde(skip)]` field on `CtxConfig`, resolved separately at the end of `load`:

- **`deny`/`ask`** are additive across layers: built-in, plus the operator's own `~/.zirv/ctx.toml` entries, plus the repo's own `.zirv/ctx.toml` entries, all unioned. Adding a `deny`/`ask` entry can only ever make a command *stricter* to evaluate (both are checked before `allow`), so a repo checkout contributing to either is always safe — the identical reasoning `sandbox.extra_deny`'s own union already established.
- **`allow`** may be extended only by the operator's own home layer. `config.rs`'s `REPO_FORBIDDEN` table rejects a repo `ctx.toml` that sets `safety.allow` at all — there is no narrowing reading of adding an allow entry (unlike `deny`/`ask`, evaluated *after* both) — mirroring `sandbox.extra_allow`.
- **`default` and `interactive_default`** are operator-only. The first controls unmatched headless commands; the second controls unmatched interactive commands. Both are `REPO_FORBIDDEN`, with `interactive_default` especially sensitive because `allow` is the loosest possible verdict.
- **`sql`** is also operator-only and `REPO_FORBIDDEN`: turning the conservative classifier off can remove an `Ask` narrowing that would otherwise protect a broad allow/default.
- **Environment** (`ZIRV_CTX_SAFETY_DENY`/`_ASK`/`_ALLOW`/`_DEFAULT`/`_INTERACTIVE_DEFAULT`/`_SQL`) sits above the fold and wins outright. It replaces only the operator+repo contribution to a list, never the built-in set: there is no environment variable that removes a built-in protection.

`resolve` never reads `allow`/`default`/`interactive_default`/`sql` from its `repo` parameter even defensively, though in production those values cannot reach it: `reject_untrusted_keys` hard-errors first. See [[Untrusted Configuration]] for the full table.

## Conservative SQL classification

With `safety.sql = "on"` (the default), `psql`, `mysql`, `mariadb`, `sqlite3`, `duckdb`, and `sqlcmd` invocations get a second pure classification. A statement is silent only when all four gates hold: the client exposes exactly one statement on argv; comments and quoting are balanced; after an optional trailing semicolon the statement begins with `SELECT`, `EXPLAIN`, or `SHOW`; and token scanning finds none of the write/escape words (`INSERT`, `UPDATE`, `DELETE`, `DROP`, `CREATE`, `ALTER`, `TRUNCATE`, `GRANT`, `REVOKE`, `MERGE`, `REPLACE`, `CALL`, `COPY`, `VACUUM`, `ATTACH`, `DETACH`, `PRAGMA`, `WITH`, `INTO`, `OUTFILE`, `DUMPFILE`, `LOAD_EXTENSION`, large-object/file helpers, or `SYSTEM`). Anything else through a recognized client asks.

The classifier runs on every structural candidate, so `echo ok && psql -c 'DROP TABLE t'` and recursively shell-wrapped DB clients cannot inherit an outer interactive allow. It may always narrow an `Allow` to `Ask`. It may widen the result to `Allow` only when no ordinary rule matched, and it never overrides `Deny`; an operator/repo `ask = ["psql *"]` also wins over a provably read-only statement. This is deliberately not a SQL parser or obfuscation defense: stdin/script/interactive input, multiple statements, malformed quotes/comments and every CTE (`WITH`, including a read-only one) ask. Rejecting all CTEs is the conservative superset chosen instead of pretending to distinguish a write-wrapping CTE without a parser.

## The `zirv ctx safety` verbs

- **`check --mode <interactive|headless>`** (`-- <command>`): prints the verdict and matched rule, exits `Verdict::exit_code()` (`allow`→0, `ask`→1, `deny`→2). No network or adapter probing.
- **`list`** (`--json` for machine-readable output): the effective merged policy, one line per rule with its origin (`built-in` / `~/.zirv/ctx.toml` / `repo .zirv/ctx.toml` / `environment`) — what an operator reads to see what a repo checkout narrowed.
- **`explain --mode <interactive|headless>`** (`-- <command>`): names the rule/default and what that verdict actually does in the selected posture—prompt interactively, fail closed headlessly, or run silently; same exit codes as `check`. Interactive is the CLI default.

`check` is dual-mode, chosen by whether a trailing command was given:

- **CLI mode** (`-- <command>` present): the ordinary case above.
- **Hook mode** (no trailing command): reads a claude `PreToolUse` JSON payload from stdin instead (`{"tool_name": ..., "tool_input": {"command": ..., "dangerouslyDisableSandbox": ...}}`). A non-`Bash` tool, an empty command, or unparseable JSON all fail open — print nothing, exit 0 — the same rule `hook.rs::run_pretool` already holds every hook in this codebase to. An explicit unsandboxed retry never inherits an ordinary allow: it asks interactively and denies headlessly.

## The wired `PreToolUse` hook

Every supervised Claude launch materializes a private, atomic `~/.zirv/runtime/claude-launch-settings.json` and names it with `--settings`. This launch-local attestation sets `disableAllHooks = false`, installs `zirv ctx safety check` for Bash, asks on `dangerouslyDisableSandbox`, scrubs cloud credentials from subprocess environments, and denies common credential paths through both `permissions.deny` and `sandbox.filesystem.denyRead`. On macOS/Linux/WSL2 it also enables Claude's OS sandbox with auto-allow, allows an unsandboxed retry only through the explicit ask boundary, and sets `failIfUnavailable = true`; native Windows omits the unsupported sandbox key. A materialization failure adds no settings flag and no blanket Bash allow, falling back to native prompts rather than widening.

`zirv setup apply` (`src/commands/setup.rs`) still installs the same hook persistently for Claude sessions started outside Zirv, as a distinct entry from the existing `Agent|Task` guard. That setup path is backed up and idempotent, but supervised-launch correctness no longer depends on it having run once in the past.

**Claude only.** `HARNESS_HOOKS` (the shared four-hook array `install_claude_integration`/`install_codex_hooks` both iterate) is untouched; the safety hook is a separate `CLAUDE_SAFETY_HOOK` constant wired only into `install_claude_integration`. Codex has no verified equivalent of the structured `permissionDecision` contract this hook relies on, so wiring it into the shared array would write a hook codex has no verified way to honor.

**Decision travels in the JSON envelope, not the exit code.** In hook mode, `hook_output` emits the same shape `hook.rs`'s own `pretool_output` already uses and this codebase has already verified against the installed claude CLI:

```json
{
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "permissionDecision": "deny",
    "permissionDecisionReason": "`rm -rf /` is deny because it matched the deny rule `rm -rf *` from built-in."
  }
}
```

`permissionDecision` can be `"deny"`, `"ask"`, or `"allow"`. On an interactive launch the hook is the sole per-command prompting gate, so an everyday or unmatched command must emit explicit `"allow"`; silence would fall through to Claude's native prompt and violate the primary criterion. The hook always exits 0: the decision belongs in the structured envelope, where all three outcomes are expressible.

**An `"ask"` still falls through under `--permission-mode dontAsk` (issue #102), but zirv's own interactive launches no longer use that mode.** Under `dontAsk`, an active ask is an unsatisfiable prompt Claude converts into a denial, so `hook_output` emits nothing and preserves the operator's own native allow list. The remaining callers are exactly a headless zirv launch and an operator who explicitly pinned `dontAsk`; the adapter's real argv is covered end to end by `the_dont_ask_suppression_is_reachable_only_from_the_headless_posture`. `Deny` emits in every mode, while non-`dontAsk`/missing modes are treated as interactive for the explanation and keep a real ask.

## Adapter projection

`AgentAdapter::default_sandbox_args(&self, sandbox, safety, mode)` and `adapters::policy_launch_args(cfg, adapter, flags, mode)` receive an explicit `LaunchMode`; no call site infers posture from TTY state.

- **Claude interactive**: `--permission-mode default`; Design B ships, so there is no blanket native `Bash(*)` allow. Non-command allow rules and scratchpad paths are pre-approved, deny rules are natively disallowed, and ask rules are on neither list. The hook emits explicit allow/ask/deny and therefore carries everyday and unknown Bash commands without making the finite native allow list a prompt surface.
- **Claude headless**: unchanged fail-closed shape, `--permission-mode dontAsk`; allow rules are pre-approved and `deny ∪ ask` is disallowed because nobody can answer.
- **Codex interactive/headless**: `--sandbox workspace-write` in both modes, with `--ask-for-approval on-request` only when the cached live probe confirms support, otherwise `never`; a second probe adds `--approve-for-me` interactively when the installed CLI advertises its native automatic reviewer. Headless always uses `never`. No `[safety]` rule reaches codex per command.

## See also

- [[Ctx Adapters]] — `LaunchMode`, `policy_launch_args`/`default_sandbox_args`, `SHIPPED_POSTURE_ALLOW`/`_ASK`/`_DENY`, and each harness projection.
- [[Ctx Subsystem]] — the `zirv ctx` verb tree `safety check`/`list`/`explain` join, and `CtxConfig`'s layering conventions this module's `resolve` mirrors.
- [[Untrusted Configuration]] — the repo-narrowing trust boundary `[safety]`'s fold is a third instance of, alongside `[policy]` and `sandbox.extra_deny`.
- [[Rot Engine]] — the sibling pure-evaluator module this one's purity discipline is modeled on.
