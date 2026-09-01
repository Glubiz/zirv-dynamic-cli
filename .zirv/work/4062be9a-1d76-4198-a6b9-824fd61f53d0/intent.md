# Intent: prompt-free default posture — only truly harmful commands ask (#222)

## Problem statement

After PR #234, zirv-supervised sessions still permission-prompt on commands the
operator considers harmless day-to-day work (observed 2026-08-31):

- `zirv test changed` / `zirv test baseline` / `zirv verify` — excluded from the
  built-in auto-allow by #234's hardening (repo-authored `verify.toml`
  payloads), yet mandated by zirv's own workflow test/verify gates, costing one
  prompt per gate run.
- `zirv frontend` — same exclusion class (repo-authored `package.json` bodies).
- Direct `gh` calls (`issue view/comment`, `pr create`) — Claude Code's sandbox
  denies reading `~/.config/gh/hosts.yml`, forcing an unsandboxed-retry ask.
- Plain `git push` — credentials/network under the sandbox force the same retry
  ask; `git worktree` operations are gated as destructive VCS actions.

The operator directive: these fixes ship IN ZIRV (defaults for every install),
not as machine-local config; only truly harmful commands should prompt.

## Desired outcome

- zirv's generated Claude launch settings allowlist zirv's platform state root
  for sandbox writes (the mechanism that today covers only the mail dir), so
  zirv built-ins that only need state-dir writes run INSIDE the sandbox.
- `zirv test`, `zirv verify`, `zirv frontend` become auto-allowed at the hook
  layer and in `permissions.allow`, but are NOT sandbox-excluded: their
  repo-authored child commands stay sandbox-confined, preserving #234's
  trust-boundary intent while eliminating the prompt.
- `gh *` and plain `git push` become shipped allows WITH sandbox exclusion
  (they need credentials/network); every existing deny family stays and still
  wins: force/delete pushes, `gh auth/secret/repo delete/release delete/api
  DELETE/codespace ssh`, publishes.
- `git worktree` operations are shipped allows.
- Still prompting/denied (truly harmful): `zirv setup` (harness-config
  destruction), `zirv ctx exec/wrap` (arbitrary argv), posture-pinning
  `agent`/`chat` spawns and `artifact --server-command` (denied), sudo/su,
  credential reads, rm -rf, disk/system commands, pipe-to-shell.

## Constraints

- Ships as built-in defaults on top of PR #234's partition model (branch
  `feat/222-default-prompt-free-posture` stacked on `fix/224-226-bug-batch`).
- Repo layer stays narrow-only; no new repo-settable keys. Operator layer can
  still narrow via `~/.zirv/ctx.toml` deny/ask.
- Failing tests first; all five gates; serial fallback; name-diff vs the base
  branch in the same environment.

## Acceptance criteria

- A supervised session runs `zirv test changed`, `zirv verify`, `gh issue
  view`, `gh pr create`, `git push`, `git worktree add/remove` without any
  permission prompt, while `verify.toml`/`package.json` children execute inside
  the sandbox (asserted via the generated settings: those zirv names appear in
  `permissions.allow` but NOT `sandbox.excludedCommands`; the state root
  appears in the sandbox filesystem write allowlist).
- Existing deny families provably still deny (tests).
- `zirv setup` still prompts; escalation tests from #234 all still pass.
