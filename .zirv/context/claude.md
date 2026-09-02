# zirv -- Claude-specific working instructions

## Repo additions to the injected orchestrator rules

- For the cross-harness review worker on a substantial diff, codex-cli is
  installed at `~/AppData/Local/Programs/OpenAI/Codex/bin` even when a roster
  line claims it is not.
- Run the `vault-keeper` agent before pushing a PR whose diff changes
  behaviour, contract, or architecture; it enforces the doc-update contract
  in `.zirv/context/common.md`.

## This Windows dev machine

- 7 cargo tests fail on `main` itself here (as of 2026-08-23), all in
  `commands::ctx::wrap::tests` (4 nesting-guard/echo + 3 `win::` exit-code/
  turn-signal). The COUNT drifts; the sorted failure-NAME list from actual
  `main` is the baseline. Do not chase them, and never use `git stash` as
  the baseline -- it diffs the branch's own HEAD, so failures introduced by
  earlier commits on the same branch get misclassified as pre-existing. One
  wrap test flakes "could not determine a platform state directory" only in
  FILTERED (`cargo test <filter>`) runs; it passes in the full suite.
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
