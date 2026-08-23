# zirv -- Claude-specific working instructions

## Orchestrator and subagent model policy

- EVERY `Agent` dispatch sets `model` explicitly. haiku = mechanical work
  (greps, inventories, formatting sweeps); sonnet = ordinary implementation,
  research, and review; opus = genuinely hard design or cross-cutting
  debugging. An unset model inherits the caller's seat -- that is the mistake.
- Never use `subagent_type: "fork"` from an expensive seat. A fork inherits the
  parent's model and fans it out; spawn a fresh typed agent with an explicit
  cheaper model instead.
- No `/code-review` fan-out above medium effort. One high run on forked
  expensive models ate roughly 48 points of a 5-hour window. Use a single
  sonnet reviewer.
- Implementer briefs MUST mandate FOREGROUND test runs. Agents that background
  `cargo test` stall silently and have to be nudged.
- Every substantive diff gets a codex cross-review round: `zirv agent codex
  "..."`. codex-cli is installed at
  `~/AppData/Local/Programs/OpenAI/Codex/bin` even when a roster line claims
  it is not.
- Run the `vault-keeper` agent before pushing; it enforces the doc-update
  contract in `.zirv/context/common.md`.

## This Windows dev machine

- Around 50 cargo tests fail on `main` itself here: Windows `os error 193`
  from fake test child binaries, a `%TEMP%` path-length socket test, and a
  test that reads the operator's real `~/.zirv/ctx.toml`. The COUNT drifts;
  the sorted failure-NAME list from actual `main` is the baseline. Do not
  chase them, and never use `git stash` as the baseline -- it diffs the
  branch's own HEAD, so failures introduced by earlier commits on the same
  branch get misclassified as pre-existing.
- A crashed run looks like a clean one: on `STATUS_ACCESS_VIOLATION` cargo
  prints no `test result:` line and no `failures:` block, so a grep for
  failure names returns EMPTY. Confirm the `test result:` line exists before
  trusting any failure list; re-running usually succeeds.
- `wrap.rs` holds roughly 30 `#[cfg(unix)]` real-PTY tests that never compile
  or run on Windows. Anything touching `wrap`, `announce`, `pace`, or adapter
  argv must be verified on Linux/Docker first: export with
  `git -c core.autocrlf=false archive HEAD` (plain `git archive` emits CRLF
  and corrupts `tests/fixtures/stub-tui.sh`), then run
  `cargo test --bin zirv wrap:: -- --test-threads=1` on `rust:1-bookworm` as a
  NON-root user, plus `cargo clippy --all-targets -- -D warnings` there
  (`#[cfg(unix)]` blocks never lint on Windows). Never assert an exact argv
  that depends on an installed-binary probe; assert the invariant.
- NEVER `taskkill` a `zirv*.exe`: this session itself runs under zirv and you
  will kill it. To clear an LNK1104 build lock, rename the locked exe aside.
- Swapping the Chocolatey-installed `zirv.exe` needs an elevated shell, and an
  old binary hard-fails on unknown `.zirv` settings keys -- install the new
  binary BEFORE adding config that uses new keys.
