---
last-verified: 2026-08-23
---

# Command Safety

## Quick Reference

- **Files:** `src/commands/ctx/safety.rs`
- **Used by:** [[Ctx Subsystem]] (`zirv ctx safety check|list|explain`, dispatched from `CtxVerb::Safety`), [[Ctx Adapters]] (`ClaudeAdapter::default_sandbox_args` projects `SafetyPolicy` onto `--allowedTools=`/`--disallowedTools=`; `CodexAdapter::default_sandbox_args` receives the same policy but has no per-command mechanism and ignores it), `src/commands/setup.rs` (`zirv setup apply` wires `zirv ctx safety check` into claude's `PreToolUse` hook, matched on `Bash`)
- **Depends on:** [[Ctx Subsystem]] for `CtxConfig`/`EnvLookup`/`CtxResult`, [[Ctx Adapters]] for `adapters::SHIPPED_POSTURE_ALLOW`/`_DENY` (the built-in rule set is derived from these, not duplicated)
- **Tests:** inline `#[cfg(test)] mod tests` in `safety.rs` — `glob_match` (exact/prefix/suffix/bare/middle/multi-star, case sensitivity), `evaluate` against a table matching the destructive families the design lists, the built-in policy (every family the shipped posture denies), `resolve`'s repo-narrowing trust boundary (a repo may add `deny`/`ask`, never `allow`/`default`), and (finding #4) `evaluate_catches_normalization_bypasses_of_the_built_in_deny_list`/`evaluate_unwraps_cmd_and_powershell_inline_command_flags` (the four confirmed bypasses, each denied) plus `evaluate_still_matches_whole_string_patterns_against_the_raw_command`/`evaluate_normalization_does_not_widen_harmless_commands` (no regression on the pre-fix behavior); `src/commands/ctx/adapters/claude.rs`'s `default_sandbox_args_stays_byte_identical_to_the_pre_safety_shipped_default` pins the claude projection against the pre-refactor hardcoded one; `src/commands/setup.rs`'s `install_claude_integration_wires_the_safety_hook_idempotently` pins the hook wiring
- **If changed:** [[Ctx Subsystem]], [[Ctx Adapters]], [[Untrusted Configuration]], [[Decision Log]]
- **Gotchas:** `evaluate`/`glob_match` are pure — no clock, filesystem or environment access, the same discipline `rot.rs` holds its scoring functions to (see that page). `resolve` (the layering step) takes its environment as an injected closure, exactly like `policy::resolve`, and is where the `ZIRV_CTX_SAFETY_*` overrides are read — never inside the matcher. The wired `PreToolUse` hook always exits 0; the deny/ask decision travels in a structured `hookSpecificOutput` JSON envelope on stdout, not the process exit code (see below for why).

## Purpose

Every harness zirv wraps has its own, incompatible way of deciding whether a command is safe to run unattended — claude's `permissions.allow`/`permissions.deny` globs plus hooks, codex's `--sandbox`/`--ask-for-approval` flags plus `.rules` execpolicy files. Before this module, "never auto-run `rm -rf`, `git push --force`, `curl | sh`" had to be encoded twice, in two dialects with different expressive power, which is exactly the cross-harness friction zirv exists to remove (issue #83). `safety.rs` gives zirv one harness-neutral classification, and every adapter projects that single policy onto its own native mechanism.

This builds on, rather than replaces, the foundation PR #96 shipped: `adapters::SHIPPED_POSTURE_ALLOW`/`_DENY` (the live-verified claude permission-rule list), `sandbox.extra_allow`/`extra_deny`, and `policy_launch_args`/`flags_pin_policy` wired into every launch seam (see [[Ctx Adapters]]). `[safety]` is the harness-neutral declaration and pure evaluator that was still open; the shipped posture constants are now the *source* the built-in default rule set is derived from, not a separate hand-maintained list.

## The model

```rust
pub enum Verdict { Allow, Ask, Deny }

pub struct Rule { pattern: String, origin: Origin }   // Origin: BuiltIn | Operator | Repo | Env

pub struct SafetyPolicy {
    deny: Vec<Rule>,
    ask: Vec<Rule>,
    allow: Vec<Rule>,
    default: Verdict,
}
```

`evaluate(policy, command) -> Outcome { verdict, matched: Option<Rule> }` checks `deny`, then `ask`, then `allow`, first pattern match within a category wins; a command matching nothing gets `policy.default` with no matched rule. Deny always beats a broader, overlapping allow entry — the same precedence PR #96 verified live for claude's own rules — because category order is fixed, not because of any ordering trick within one list.

`glob_match(pattern, text)` supports only `*` (any run of characters, including none), case-sensitive, matched against one already-normalized command string (see below). It is the standard iterative two-pointer `fnmatch`-style algorithm with a saved star position, not recursive backtracking: a command string can originate from repository-influenced text (a prompt-injected shell command an agent was talked into proposing), so the matcher must not be a stack-depth or exponential-blowup DoS surface. Worst case is `O(pattern * command)` with no recursion.

### Normalization, and its explicit non-goals

`evaluate` does not match `command` as one raw string against every rule any more — that read as a single opaque string a compound command (`a && b`), a one-layer shell-wrapped one (`bash -c '<cmd>'`, `cmd /c <cmd>`, `powershell -Command <cmd>`), a `/usr/bin/rm`-style absolute-path invocation, or merely doubled whitespace (`rm  -rf /`) never matched a shipped `deny` pattern at all — four confirmed bypasses of the destructive-family denylist. `normalize_segments(command)` now derives a list of candidate strings `evaluate` checks independently, taking the single most restrictive `Outcome` (`deny` > `ask` > `allow`) across all of them:

1. The raw command, unmodified — always checked first, so a pattern written against the *whole* string (there are none in the shipped `deny` set today, but a repo/operator `[safety]` entry could still be one) keeps matching exactly as before this fix.
2. One entry per shell-separator segment (`split_segments`: splits on `;`, `&&`, `||`, `|`, newline), whitespace-collapsed (`collapse_whitespace`) and leading-directory-stripped (`strip_program_dir`, so `/usr/bin/rm -rf /` and `rm -rf /` compare identically).
3. For a segment that is itself a recognised one-layer shell-wrapper invocation (`unwrap_shell_wrapper`: `sh`/`bash`/`zsh -c '<inner>'`, `cmd /c <inner>`, `powershell -Command <inner>`), its unwrapped, quote-stripped inner command, normalized the same way.

**Explicit non-goals**, not bugs to be filed later: this is one layer of normalization, not a shell parser or a sandbox.

- **No quote-aware splitting.** `split_segments` does not track quoting, so a separator character inside a quoted string (`bash -c 'a; b'`) still splits the outer command at `;` — the inner `a`/`b` pieces are evaluated as their own (likely meaningless) segments rather than staying joined.
- **No nested unwrapping.** `unwrap_shell_wrapper` unwraps exactly one layer; `bash -c 'bash -c "rm -rf /"'` is not chased down to the innermost command.
- **No encoding/obfuscation awareness.** Base64-encoded payloads piped into `sh`, `eval "$(...)"`, environment-variable reassembly of a command string, and similar obfuscation are not decoded or evaluated — the matcher only ever compares literal text.
- **No `eval`/`exec`/`source`/`.`-style indirection.** A command that reads and runs a file's contents at runtime is invisible to a static string matcher by construction; this is the same limit every static-glob classifier (claude's own `permissions.allow`/`deny`, codex's `.rules`) already has.

A determined adversarial actor with control over the exact command text can still construct something none of the four confirmed-fixed bypasses (`bash -c '<denied cmd>'`, absolute path, doubled whitespace, `a && <denied cmd>`) cover — this module raises the bar against the bypasses that were actually demonstrated, not against every possible obfuscation. `zirv setup apply`'s wired `PreToolUse` hook and `[policy]`'s sandbox enforcement are independent layers underneath this one; `[safety]` is a classifier, not the only defense.

## The built-in default rule set

`builtin_deny()`/`builtin_allow()` derive from `adapters::SHIPPED_POSTURE_DENY`/`_ALLOW` by stripping each entry's `Bash(...)` wrapper (`command_pattern_from_bash_rule`); a non-`Bash` entry (`Read(./**)`/`Edit(./**)`/`Read(~/.claude/**)`/`Edit(~/.claude/projects/**)`/`Read(~/.zirv/**)`/`Edit(~/.zirv/**)`/`Read(~/.claude/.credentials.json)`/`WebFetch`/`WebSearch`, which scope file access or name a bare tool rather than a command, issue #104) is skipped — outside `[safety]`'s own domain, via the same general prefix check for every entry, not a hard-coded list of two. `builtin_ask()` is empty: no shipped posture maps onto "ask" today, since the existing per-harness posture is a binary allow/deny choice. `SafetyPolicy::default()` (the built-in policy alone, what an operator who has written no `[safety]` table gets) is exactly this: "a fresh install already blocks the obvious destructive families... without anyone writing config" (issue #83's own acceptance wording) with `default: Verdict::Ask`.

Order is preserved through the derivation, which matters for the adapter projection below: iterating `safety.deny`/`safety.allow` in order and re-wrapping each pattern as `Bash(<pattern>)` reproduces `SHIPPED_POSTURE_DENY`/`_ALLOW`'s original strings byte-for-byte.

**Issue #98 (2026-08-23):** the injected prompt (`prompt.rs`'s `HARNESS_PROMPT`, `ORCHESTRATOR_PROMPT` in `adapters::claude`) mandates `zirv ctx status`/`inbox`/`send`/`nudge`/`remember`/`recall`, `zirv agent <name> "..."`, and `zirv <script>` — but `SHIPPED_POSTURE_ALLOW` had no entry for `zirv` at all, so every one of those mandated commands was silently denied under `dontAsk`. A prompt must never mandate a command family the shipped posture denies. `SHIPPED_POSTURE_ALLOW` now carries `Bash(zirv *)` (the same trust class as the already-allowed `make build`/`npm run build`, since `zirv <script>` runs repo-defined commands) plus `Bash(cargo fmt *)`/`Bash(cargo clippy *)` next to the existing `cargo build`/`test`/`check` entries. `prompt_mandated_zirv_commands_are_allowed_by_the_shipped_posture` (in `safety.rs`'s test module) pins every prompt-mandated command as `Allow`, and still asserts a destructive family unrelated to `zirv` (`git push --force`, `rm -rf`) stays `Deny` — deny still wins over the new broad allow.

**Issue #111 (2026-08-23), argument-reordering and sibling-utility bypasses:** PR #107's review found the round-4 `git push`/`git reset` deny entries were flag-anchored (`Bash(git push --force *)`), so `git push origin --force` (the flag reordered away from the front) slipped past untouched, as did the short-flag spellings (`-f`/`-d`), an empty-src refspec delete (`git push origin :branch`), and a force-refspec push (`git push origin +branch`). Those entries are now mid-string-wildcard patterns (`git push*--force*`, `git push* -f *`/`git push* -f`, `git push*--delete*`, `git push* -d *`/`git push* -d`, `git push* :*`, `git push* +*`, `git reset*--hard*`) — `glob_match` already supports `*` anywhere, not only as a suffix, and `git push*--force*` also covers `--force-with-lease` since that flag's own text contains `--force`. The same reordering gap existed for `find`'s own `-delete`/`-exec`/`-ok` actions (the `find *` allow was read-only only up to those), for `head`/`tail`/`diff` reading the same credential paths `cat`'s four deny entries already covered, and for three `gh` escapes (`gh api -X DELETE`/`--method DELETE`, `gh secret`, `gh codespace ssh`) — all closed the same way. `evaluate_deny_survives_argument_reordering_issue_111` (`safety.rs`'s test module) pins every bypass form to `Deny` and confirms ordinary uses (`git push -u origin x`, `find . -name foo.rs`, `gh pr create --fill`) stay `Allow`. As `adapters::SHIPPED_POSTURE_DENY`'s own doc comment now states explicitly: with arbitrary-code toolchains (`python *`, `node *`, ...) allowed, this list is a tripwire for named destructive/credential command families, not a security boundary.

## Layering (`resolve`)

`[safety]` cannot use the ordinary deep merge (a later layer's array would replace an earlier one's) or `REPO_FORBIDDEN`'s all-or-nothing rejection (a repo must be able to *narrow* the policy). So `safety::resolve` folds the layers the way `policy::resolve` folds `[policy]` — lifted whole out of `ctx.toml` by `CtxConfig::load` (both `home_safety`/`repo_safety` removed via the same `SAFETY_SECTION` constant `POLICY_SECTION` already uses) before the ordinary deep merge, and is itself a `#[serde(skip)]` field on `CtxConfig`, resolved separately at the end of `load`:

- **`deny`/`ask`** are additive across layers: built-in, plus the operator's own `~/.zirv/ctx.toml` entries, plus the repo's own `.zirv/ctx.toml` entries, all unioned. Adding a `deny`/`ask` entry can only ever make a command *stricter* to evaluate (both are checked before `allow`), so a repo checkout contributing to either is always safe — the identical reasoning `sandbox.extra_deny`'s own union already established.
- **`allow`** may be extended only by the operator's own home layer. `config.rs`'s `REPO_FORBIDDEN` table rejects a repo `ctx.toml` that sets `safety.allow` at all — there is no narrowing reading of adding an allow entry (unlike `deny`/`ask`, evaluated *after* both) — mirroring `sandbox.extra_allow`.
- **`default`** (the verdict for a command matching nothing) is `REPO_FORBIDDEN` outright too, for the identical reason: a single scalar with no narrowing direction of its own.
- **Environment** (`ZIRV_CTX_SAFETY_DENY`/`_ASK`/`_ALLOW`/`_DEFAULT`) sits above the fold and wins outright, mirroring `ZIRV_CTX_SANDBOX_EXTRA_DENY`/`_ALLOW`. It replaces the operator+repo *contribution* to a list, never the built-in set: there is no environment variable that removes a built-in protection.

`resolve` never reads `allow`/`default` from its `repo` parameter even defensively, though in production a repo value carrying either can never actually reach it — `reject_untrusted_keys` in `config.rs` hard-errors before the lift removes `[safety]` from the repo table at all. See [[Untrusted Configuration]]'s "A third fold, now for `[safety]`" section for the full REPO_FORBIDDEN table entries.

## The `zirv ctx safety` verbs

- **`check`** (`-- <command>`): prints the verdict and matched rule, exits `Verdict::exit_code()` (`allow`→0, `ask`→1, `deny`→2). No network, no adapter probing — `CtxConfig::load` reads only local TOML files and process environment, so this is fast enough to run on every tool call.
- **`list`** (`--json` for machine-readable output): the effective merged policy, one line per rule with its origin (`built-in` / `~/.zirv/ctx.toml` / `repo .zirv/ctx.toml` / `environment`) — what an operator reads to see what a repo checkout narrowed.
- **`explain`** (`-- <command>`): a one-sentence prose explanation of why a command got its verdict; same exit codes as `check`.

`check` is dual-mode, chosen by whether a trailing command was given:

- **CLI mode** (`-- <command>` present): the ordinary case above.
- **Hook mode** (no trailing command): reads a claude `PreToolUse` JSON payload from stdin instead (`{"tool_name": ..., "tool_input": {"command": ...}}`). A non-`Bash` tool, an empty command, or unparseable JSON all fail open — print nothing, exit 0 — the same rule `hook.rs::run_pretool` already holds every hook in this codebase to: a safety hook that crashes or misbehaves must never be the reason a session cannot make progress.

## The wired `PreToolUse` hook

`zirv setup apply` (`src/commands/setup.rs`) installs `zirv ctx safety check` as a `PreToolUse` hook matched on `Bash`, a distinct entry from the existing `Agent|Task`-matched `PreToolUse` hook (`zirv ctx hook pretool`) — both live in the same `PreToolUse` event array, since `ensure_harness_hook` pushes a new entry per distinct command string rather than replacing. Backed up via the same manifest system every other `zirv setup` write already uses, and idempotent (`contains_command` skips re-adding a command already present).

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

`permissionDecision` is `"deny"` or `"ask"`; `Verdict::Allow` produces no output at all — printing nothing is claude's own "no opinion, fall through to the normal permission flow" reading. The hook always exits 0: exit code 2 blocks too, but only via stderr text with no `"ask"` equivalent, and `run_pretool`'s own doc comment already records this exact tradeoff for the codebase's other `PreToolUse` hook — this hook follows the same, already-verified convention rather than inventing a second one.

**An `"ask"` falls through to nothing under `--permission-mode dontAsk` (issue #102).** The stdin payload also carries `permission_mode` (`HookToolPayload`), one of claude's documented values (`default`/`plan`/`acceptEdits`/`auto`/`dontAsk`/`bypassPermissions`). Claude's own docs say a hook decision never bypasses permission rules, and `dontAsk` itself means "deny if not pre-approved" — so an active `"ask"` in that mode is not a prompt, it is an unsatisfiable denial that would otherwise strip the operator's own `permissions.allow` entries (e.g. `gh *`, `cargo *`) from every zirv-launched session the moment the hook is installed. `hook_output` therefore emits nothing (same as `Allow`) when `verdict == Ask && permission_mode == "dontAsk"`, letting claude's own flow — and the operator's `allow` list — decide instead. `Deny` is unaffected by mode, and every mode other than `dontAsk` (including a payload that omits `permission_mode` entirely, for backward compatibility) keeps emitting `"ask"` unchanged.

## Adapter projection

`AgentAdapter::default_sandbox_args(&self, sandbox: &SandboxConfig, safety: &SafetyPolicy) -> Vec<String>` now takes the resolved `SafetyPolicy` alongside `SandboxConfig` (both registered adapters' signatures changed together with `policy_launch_args`, the one call site).

- **claude**: every non-`Bash` entry in `SHIPPED_POSTURE_ALLOW`/`_DENY` (file-scope rules like `Read(./**)`/`Edit(./**)`, the harness/operator dir rules, `WebFetch`/`WebSearch` — outside `[safety]`'s domain) prepended directly, in declared order, plus every `safety.allow`/`safety.deny` rule re-wrapped as `Bash(<pattern>)`, plus two scratchpad rules computed at launch from the real `std::env::temp_dir()` (`adapters::scratchpad_rules`, issue #104 — per-machine, so not part of the `&'static` constant), plus `sandbox.extra_allow`/`extra_deny` appended last. Under the shipped default (no `[safety]`/`sandbox.extra_*` configured), this reproduces `SHIPPED_POSTURE_ALLOW`/`_DENY`'s original argv byte-for-byte plus the scratchpad rules, pinned by a dedicated test (see Quick Reference).
- **codex**: unchanged — `--sandbox workspace-write --ask-for-approval never`. Codex has no per-command mechanism to receive individual `[safety]` rules, so its `default_sandbox_args` accepts the `SafetyPolicy` parameter (for trait-signature parity) and ignores it, exactly as it already ignored `sandbox.extra_allow`/`extra_deny`.

## See also

- [[Ctx Adapters]] — `policy_launch_args`/`default_sandbox_args`, `SHIPPED_POSTURE_ALLOW`/`_DENY`, and the shipped-default posture this module's built-in rule set is derived from.
- [[Ctx Subsystem]] — the `zirv ctx` verb tree `safety check`/`list`/`explain` join, and `CtxConfig`'s layering conventions this module's `resolve` mirrors.
- [[Untrusted Configuration]] — the repo-narrowing trust boundary `[safety]`'s fold is a third instance of, alongside `[policy]` and `sandbox.extra_deny`.
- [[Rot Engine]] — the sibling pure-evaluator module this one's purity discipline is modeled on.
