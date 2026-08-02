# 2026-08-02 full-diff review — findings and fix record

Adversarial review of the `improve-agent-workflows` branch (30 files, +4996/−314 vs main at af59345).
All 28 findings were verified against the code before being accepted, and all 28 are fixed on this branch.

## Finding → fix commit

| Commit | Findings |
|---|---|
| 8c3a642 `fix(ctx): stop round-tripping the prompt through argv` | 2, 3, 4, 8, 15 |
| 5444f53 `fix: exempt passthrough from the agent gate, close three leaks` | 6, 9, 11, 13 |
| 6c6fb2a `fix: relaunch-proof cooldown, honest probe target, and review follow-ups` | 1, 5, 7, 10, 12, 14, 16–28 |

## Major findings (all fixed)

1. Stop hook still O(session) per turn: `corrections_in` full-parsed before the cheap gates — gates hoisted above it.
2. Restart passed the prompt twice when it started with `-` (markdown bullets) — prompt now matched by value, not shape.
3. `extra_launch_flags` kept program-invocation tokens (`npx claude`, multi-token agent_bin) as flags — adapters now report their launch-prefix length.
4. Only two-token `--session-id X` was stripped on restart; `--session-id=`, `--continue`, `--resume` survived into the restart meant to escape that session — all stripped now.
5. Injection cooldown keyed on the dead session's absolute turn number, going silent after relaunch/compaction — now keyed on a local monotonic TurnSignal counter.
6. The M5 "unknown agent" gate also refused `--no-supervise`/`--simple` passthrough — passthrough exempted.
7. M7 probed `self.program` but launched a possibly different binary — probe target now derived from the actual launch command.
8. A prompt reading like `--append-system-prompt=…` was stripped from argv and promoted to the highest-precedence system-prompt layer (reachable via `${var}` from captures) — prompt index protected; agent steps pass the prompt as data with no argv round-trip.
9. A step with both `command:` and `agent:` silently ran as a shell command (serde untagged) — key-based dispatch with step-indexed errors.
10. optimize's judgment prompt shipped raw `settings.json` contents (potentially API keys in `env` blocks) to the model — JSON string values redacted, disclosure added.
11. Nested CLAUDE.md scan followed symlinked dirs out of the repo — symlinks skipped.

## Minor findings (all fixed)

12 decision-log evidence scoped to sampled sessions + disclosure; 13 `write_private` re-chmods existing files to 0600; 14 scoring/ and prompts/ pruned to newest 200 (fail-open); 15 user's own `--append-system-prompt-file` merged, not overridden; 16 `zirv --help` always prints usage + builtins; 17 create-only flags rejected outside `create` instead of silently swallowing script args; 18 `--name` path-traversal rejected; 19 exec exit sentinels 75/76 rendered as park/give-up outcomes in agent steps; 20 agent-step option validation at load time (dry-run included); 21 validation ordered before OS-skip; 22 levenshtein length short-circuit; 23 M7 file delivery in loop/resume too; 24 git-appliable label dropped for CRLF/no-final-newline files; 25 RAII `EnvGuard` replaces leaking `with_fake_env` copies; 26 `read_capped`/`read_layer` share `truncate_bytes`; 27 create status lines on stderr via `output::note`; 28 malformed `.shortcuts.yaml` no longer silently replaced.

## Verified clean (no action)

Incremental scoring equivalence (RotState fold vs full parse on every branch and window boundary), checkpoint invalidation fail-open, no hook/supervisor checkpoint races, no shell injection, 0600/0700 on all new state, M2 attribution across restart/park/loop/resume, agent-step dry-run never spawns.

## Known caveats for the merger

- The 48 `commands::ctx::wrap::tests` pty tests hang when the suite itself runs under `zirv ctx wrap` (pty-in-pty, pre-existing, environmental). They must pass on CI or an unwrapped shell before merge. Run with stdin closed (`< /dev/null`).
- Intentional interface change: `zirv ctx exec --prompt X` with no program after `--` now builds the launch from the adapter; `-- --model opus` means extra flags (documented in README).
- Codex adapter remains a stub, blocked on authenticated Codex session observation (issue #11).
