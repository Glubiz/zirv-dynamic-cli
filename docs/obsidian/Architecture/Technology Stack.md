---
last-verified: 2026-08-30
---

# Technology Stack

## Quick Reference

- Crate `zirv`, version 2.26.0, Rust edition 2024.
- Async runtime is `tokio` (multi-thread), used for process spawning throughout `script_runner/` and the `ctx` supervisors.
- Release profile is tuned for a small, fast-starting CLI binary and sets `panic = "abort"` — see the Gotcha below.
- Local/CI test runner is `cargo-nextest` (pinned 0.9.143), replacing serial `cargo test -- --test-threads=1` as the primary loop — see "Test runner" below.
- **If changed:** update this page whenever a dependency is added, removed, or re-pinned in `Cargo.toml`. If the release profile changes (especially `panic`), also check [[Ctx Supervisors]] and [[Architecture Overview]] for correctness of any assumption built on it.
- **Gotchas:** `panic = "abort"` means no unwind-time `Drop` runs on panic. `wrap`'s PTY/raw-mode supervision code (see [[Ctx Supervisors]]) has to restore terminal state in explicit error arms rather than relying on `Drop`, and avoids `unwrap`/`expect` on its hot path for this reason.

## Package metadata

| Field | Value |
|---|---|
| name | `zirv` |
| version | `2.26.0` |
| edition | `2024` |
| license | MIT |
| repository | https://github.com/Glubiz/zirv |

## Dependencies

| Crate | Version | Purpose |
|---|---|---|
| `clap` (derive) | 4.6.0 | CLI argument parsing for the top-level `Input`, `zirv ctx`, and provider-neutral workflow command trees |
| `serde` (derive) | 1.0.228 | Serialization/deserialization backbone for scripts and config |
| `serde_yaml_ng` | 0.10.0 | YAML script parsing (`.yaml`/`.yml`), and `.shortcuts.yaml` |
| `serde_json` | 1.0.149 | JSON script parsing (`.json`) |
| `toml` | 1.0.6 | TOML script parsing (`.toml`) and `ctx.toml` config layering |
| `dirs` | 6.0.0 | Platform home directory and state directory resolution |
| `console` | 0.16.3 | Terminal output styling |
| `dialoguer` | 0.12.0 | Interactive prompts (`zirv create`'s wizard) |
| `tokio` (macros, rt-multi-thread, time, process) | 1.50.0 | Async runtime: spawning shell/agent child processes, delays, the multi-threaded executor |
| `hashbrown` (serde) | 0.16.1 | `HashMap` implementation used for the script execution context |
| `futures` | 0.3.32 | Async combinators |
| `slab` | 0.4.12 | Slot-keyed allocation, used in the `ctx` supervisor machinery |
| `regex` | 1 | Detecting unresolved `${...}` placeholders after substitution |
| `sha2` | 0.11.0 | Stable SHA-256 identities for immutable launch-policy snapshots, privacy-preserving command-decision audit correlation, and tamper detection in the Claude safety-hook attestation |
| `portable-pty` | 0.9.0 | Cross-platform PTY for `zirv ctx wrap`'s interactive supervision, and for each of the dashboard's own panes (`dash::pane`) |
| `uuid` (v4) | 1.24.0 | Session IDs for `ctx` transcripts and supervised runs |
| `crossterm` | 0.29 | Terminal event/key input and raw-mode primitives for the dashboard's own event loop (`dash/mod.rs`) |
| `ratatui` | 0.30 | Immediate-mode TUI rendering for the dashboard's header, sidebar, pane grid, and overlays (`dash/ui.rs`) |
| `vt100` | 0.16 | Embedded terminal-screen emulation, one `vt100::Screen` per dashboard pane, so a pane's own child renders correctly without owning the real terminal |
| `unicode-width` | 0.2.2 | Display-width-aware text truncation/padding for `style.rs` (issue #202, v2.38.0) — zirv's single terminal design system: semantic `Tone`/`paint`, `format_age`/`format_pct`/the shared `PLACEHOLDER` en dash, and `style::tui`'s ratatui tokens (the brand `chip()`, `SPINNER_FRAMES`). A CJK/emoji character's on-screen column width differs from its `char`/byte count, so every truncation/padding decision across `chrome.rs`'s banner/status bar and `dash/ui.rs`'s header/sidebar/dialogs goes through this crate rather than counting characters |
| `ureq` | 3 | Blocking HTTP client for `ctx::poll::HttpPoller` — this crate's **first** HTTP dependency (2026-08-16). Deliberately narrow in scope: it backs only the active usage-poll *fallback*, consulted solely when the passive collector reading (statusline tee or codex rollout scan) has already gone stale at a pacing decision point — never on a path that must stay network-free (`wrap`'s status-bar redraw never constructs a poller). Chosen over `reqwest` for a synchronous, blocking call with no async runtime coupling needed for one occasional GET; pulls in `rustls` (and transitively `ring`/`cc`) rather than a system TLS dependency, keeping the binary's TLS story self-contained the way the rest of the dependency tree already is |

### Platform-specific

| Crate | Version | Scope | Purpose |
|---|---|---|---|
| `libc` | 0.2.183 | `cfg(unix)` | Unix system calls needed by `ctx` process/terminal primitives and workflow process-group cleanup on verification or interactive-artifact timeout |
| `windows-sys` (Win32_Foundation, Win32_Security, Win32_Storage_FileSystem, Win32_System_Console, Win32_System_IO, Win32_System_JobObjects, Win32_System_Pipes, Win32_System_Threading) | 0.61.2 | `cfg(windows)` | Console-mode and named-pipe APIs for `ctx wrap` on Windows. `Win32_System_JobObjects` (added 2026-08-16) is `ctx::supervise::JobGuard`'s kill-on-close job object, the kernel-enforced backstop that reaps a supervised agent's whole process tree when zirv itself dies with no user code running (`taskkill /F`, a crash, `panic = "abort"`) — see [[Ctx Supervisors]]. Named explicitly rather than relying on the transitive copy pulled in by `console`/`dirs-sys`/`mio`/`tempfile`, to avoid a second win32 binding crate |

### Dev-dependencies

| Crate | Version | Purpose |
|---|---|---|
| `tempfile` | 3.27.0 | Isolated temp directories in tests (script resolution, `ctx` config/state tests) |

## Release profile

```toml
[profile.release]
opt-level        = "z"
lto              = true
codegen-units    = 1
debug            = false
debug-assertions = false
panic            = "abort"
strip            = "symbols"
```

Optimized for binary size (`opt-level = "z"`, LTO, single codegen unit, stripped symbols) over compile speed, appropriate for a CLI that is installed once and run many times. `panic = "abort"` means a panic terminates the process immediately without unwinding — no `Drop` cleanup runs on the way out. This is load-bearing for [[Ctx Supervisors]]: `wrap` must restore terminal raw-mode in explicit error-handling arms, not in a `Drop` impl, since a panic would skip that cleanup entirely.

## Test runner

`cargo nextest run --no-fail-fast` (config: `.config/nextest.toml`) is now the primary local and CI test loop, replacing serial `cargo test --verbose -- --test-threads=1` as the default (~11–12 min → ~23–54s on this machine, v2.25.1, PR #113). nextest runs each test in its own process, so the process-wide `env::set_var` races that used to force `--test-threads=1` cannot happen. `--no-fail-fast` is required, not optional: nextest's own default is fail-fast (stop at the first failure), which cannot produce the complete, sorted failure-NAME list a pre-existing-failure baseline has to be diffed against (see this repo's own Windows-baseline methodology). CI installs a pinned `cargo-nextest@0.9.143` via `taiki-e/install-action@v2` (`.github/workflows/ci.yaml`) rather than trusting whatever version a runner image happens to ship. Serial `cargo test -- --test-threads=1` is kept as a compatibility fallback — `.zirv/context/common.md`'s "Build and verify" now lists both as required before claiming done, not either/or.

`.config/nextest.toml` also defines a `test-groups.exec-nudge-restart` group (`max-threads = 1`, `threads-required = "num-test-threads"`) for the family of `exec.rs` tests that drive a real supervised session through a background writer thread polling a bounded budget before nudging it — under CPU contention that poll can lose the race against the fake agent starting. The group claims the whole nextest concurrency pool while any of its members run, so nothing else in the same run contends with them; it cannot exclude contention from other processes on the host. See [[Known Issues]] for the two related fixes this same PR made test-side (`wait_for_lines_or_panic`/`wait_for_live_session_or_panic` panicking instead of silently giving up, and the `fake-codex-agent.sh` capability-probe mode-shift).
