# zirv ctx Context Management Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `zirv ctx <verb>` command family that autonomously detects context rot in AI coding agent sessions and intervenes by advising, compacting, or restarting-with-handoff, for headless loops, one-shot headless runs, and interactive TUI sessions.

**Architecture:** Four agent-agnostic layers inside `src/commands/ctx/`: an `AgentAdapter` trait holding everything agent-specific (claude, codex), a pure deterministic rot engine scoring a normalized event stream, three supervisors (`loop`, `exec`, `wrap`) that act on verdicts, and a handoff layer that distills and re-injects session state across restarts. No daemon: every mode is a foreground process owning its children. Built-in dispatch happens before YAML script lookup.

**Tech Stack:** Rust edition 2024, clap 4 (derive), serde/serde_json/toml (already present), hashbrown (already present), `portable-pty` (new), `uuid` (new), `libc` (new, unix-only), `tempfile` (dev, already present). Tests are inline `#[cfg(test)] mod tests` blocks; fixtures are data files under `tests/fixtures/`.

## Global Constraints

- Command family is `zirv ctx <verb>`, implemented as `src/commands/ctx/` with **one submodule per verb**, following the existing `src/commands/` pattern. Built-ins resolve before YAML scripts; a repo-local `.zirv/ctx.yaml` script is shadowed (accepted, documented).
- Verbs (exactly these nine): `score`, `loop`, `exec`, `wrap`, `handoff`, `resume`, `hook`, `status`, `usage`.
- Scoring is **pure and deterministic**: same events in, same verdict out. No ML, no learned scoring, no clock or filesystem reads inside the engine.
- Signal set: token gate (floor **100000**, ceiling **160000**), tool-failure rate, repetition loops (**>= 3** identical `(tool, input_hash)` in the window), capability-gated marker signal (default marker text **`[zirv]`**).
- Verdict thresholds: score **>= 40 `advise`**, **>= 60 `compact`**, **>= 80 `restart`**. Below the token floor the verdict is always `healthy`. At or above the ceiling the verdict is at least `compact`; at or above the ceiling **with score >= 60** it is `restart`.
- Default trailing window: **last 10 turns**. Default maturity gate for the marker signal: **10 turns**.
- Adapters in v1: `claude` (full capabilities) and `codex` (**no marker signal**). Selection is automatic from wrapped argv via `detect`, overridable with `--agent <name>`.
- Handoff distillation runs a **fresh headless model call** (cheap model, default `haiku`-class via the adapter) over a compact transcript tail. A rotted session is never asked to summarize itself. On distillation failure a **mechanical structural handoff** is produced instead. Handoff sections, in order: Task, Done, Remaining, Next step, Files touched, Gotchas learned.
- Turn signals travel over **unix domain sockets** in the platform state dir under `zirv/ctx/`. State never lives inside the repo.
- `wrap` injects **only** when both preconditions hold: turn boundary reached **and** user idle (input buffer untouched, PTY output-quiet for a **3s** debounce). On any supervision failure `wrap` degrades to **pure passthrough**; a wrapped session must never be worse than an unwrapped one.
- Injection is **verified**: after sending the compaction command, wrap confirms a compaction event appears in the transcript within a timeout, else it logs and retreats to advisory. No blind keystroke retries.
- The release profile is `panic = "abort"`. Terminal raw-mode restore must happen in **explicit error arms**, never relying on unwind-time `Drop`. No `unwrap`/`expect` on any `wrap` hot path.
- Hooks **never block** the agent's stop. `zirv ctx hook <event>` always exits 0, even on internal error. The same rule covers the statusline: `zirv ctx usage tee` always exits 0 and always emits a statusline, chained output when it can and a built-in fallback line otherwise.
- Usage pacing keeps a subscription window at or below **`pace_max_percent`, default 99**. Data layers in priority order: **collector (server-authoritative) > estimator (approximation) > none**; a fresher lower-priority layer never overrides a fresh collector reading, and the estimator is always labeled an approximation.
- **A pause is never an exit.** At or above `pace_max_percent`, `loop` and `exec` wait until the window's `resets_at` plus jitter (configured fallback delay when the reset is unknown) and then continue. The wait is bounded **per window** (its own length plus slack, so five hours or seven days, never one global clock), with an optional absolute override. A limit hit mid-run is **parked and relaunched without consuming the restart budget**. Every pacing decision is appended to the decision log once per pause, not once per check.
- No test may consume real Claude usage or make an API call. Pacing tests drive fixture statusline JSON, synthetic transcripts, and the existing `tests/fixtures/fake-agent.sh`.
- Config layering, lowest to highest: `~/.zirv/ctx.toml`, then `<repo>/.zirv/ctx.toml`, then `ZIRV_CTX_*` environment variables, then command-line flags.
- All supervisor decisions are appended as JSONL to the state dir for post-hoc audit.
- CI runs `cargo test --verbose -- --test-threads=1`, `cargo fmt -- --check`, `cargo clippy --all-targets -- -D warnings` on Linux; CD runs `cargo build --release` on Linux, macOS **and Windows**. Every unix-only module needs a compiling Windows counterpart that returns a clear `Err`.
- No em dashes in any user-facing CLI string (output, advisories, help text, README copy).

### Dependencies added (each justified)

| Crate | Version | Why it is unavoidable |
|---|---|---|
| `portable-pty` | `0.9.0` | `wrap` must own a real PTY and proxy bytes; std has no PTY API. Named in the spec. |
| `uuid` | `1.24.0` (features `["v4"]`) | Session ids must be generated up front so transcript paths are known before launch. Named in the spec. |
| `libc` | `0.2.183`, `[target.'cfg(unix)'.dependencies]` only | Raw-mode (`tcgetattr`/`tcsetattr`/`cfmakeraw`), window size (`ioctl TIOCGWINSZ`) and `SIGTERM` (`kill`) have no std equivalent. Verified during planning: the already-present `console` crate exposes no public persistent raw-mode API, and `libc 0.2.183` is **already in `Cargo.lock`** transitively via tokio, so this adds zero new compile units. |

Everything else reuses existing dependencies: `serde_json` for transcript parsing and JSON output, `toml` for config layering and merging, `hashbrown` for repetition counting, `clap` for the verb tree, `dirs` for the state dir, `tempfile` for tests. No `chrono` (timestamps are `SystemTime` unix seconds), no `notify` (transcript watching is polling-based), no `regex` beyond what is already used elsewhere.

### Facts verified during planning (do not re-derive, do not "fix")

1. **Context size is not `usage.input_tokens`.** In a real 110k-token claude session `input_tokens` is `2`; the real figure is `input_tokens + cache_creation_input_tokens + cache_read_input_tokens`. The token gate uses that sum.
2. **`PreCompact` hooks cannot inject compaction instructions.** Docs-confirmed: `PreCompact` honors only `decision:"block"`, `reason`, `systemMessage`, `suppressOutput`, `continue`, `stopReason`. So `zirv ctx hook pre-compact` is observational (decision-log entry plus advisory) and the focus instructions are delivered instead as arguments to the injected `/compact <focus>` command in `wrap`.
3. **`Stop` hook stdin** carries `session_id`, `transcript_path`, `cwd`, `hook_event_name`, `permission_mode`, `last_assistant_message`, and (observed in practice, absent from the docs table) `stop_hook_active`. Parse it as optional with `false` default.
4. **`UserPromptSubmit`** injects context via `{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"..."}}`.
5. **Transcript path**: `~/.claude/projects/<slug>/<session-uuid>.jsonl` where `<slug>` is the cwd with every character outside `[A-Za-z0-9-]` replaced by `-`. On-disk evidence confirms `/` and `.` both map to `-` (`/Users/x/repo/.claude-worktrees/b` becomes `-Users-x-repo--claude-worktrees-b`). `_` is unevidenced, so `transcript_path` falls back to scanning `~/.claude/projects/*/<uuid>.jsonl` when the computed path does not exist.
6. **`AgentAdapter::detect` cannot be an associated function** as sketched in the spec (`fn detect(command: &[String]) -> bool`) because the trait must be dyn-compatible for `Box<dyn AgentAdapter>`. It takes `&self`, and selection walks a registry of instances. Mechanical fix, not a design change.
7. **macOS caps unix socket paths at ~104 bytes.** `~/Library/Application Support/zirv/ctx/sockets/<full-uuid>.sock` is 109 bytes and would fail at runtime. Sockets therefore live at `<state>/s/<first-8-hex-of-uuid>.sock` (~75 bytes) and `bind` rejects paths over 100 bytes with a clear error so `wrap` degrades instead of emitting an opaque OS error.
8. **`codex` is not installed on this machine** (`codex not found`, no `~/.codex`). Task A9 verifies real behavior against an installed CLI and records findings; A10 is explicitly gated on that verification succeeding.
9. **`.zirv/ctx.toml` breaks `zirv help` unless excluded.** `src/commands/help.rs::write_scripts` parses every `.toml` in `.zirv/` as a `Script` and propagates the error, so a config file there makes `zirv help` fail outright. Task A2 excludes it, with a regression test.

---

## File Structure

**New, under `src/commands/ctx/`:**

| File | Responsibility |
|---|---|
| `mod.rs` | `CtxCli`/`CtxVerb` clap tree, `dispatch`, `CtxResult` alias, module declarations |
| `config.rs` | `CtxConfig` and sub-configs, TOML layer merge, `ZIRV_CTX_*` overrides |
| `state.rs` | State dir resolution and path helpers, repo slug |
| `log.rs` | Append-only JSONL decision log |
| `event.rs` | `NormalizedEvent`, `SessionId`, `SessionRef`, `Capabilities`, `StructuralContext`, `input_hash` |
| `rot.rs` | Pure rot engine: `Signals`, `Verdict`, `Score`, `signals`, `score_events`, `verdict_for` |
| `signal.rs` | Unix-socket turn signals: `TurnSignal`, `SignalServer`, `send` |
| `supervise.rs` | Process primitives shared by `exec` and `loop`: spawn, tick-supervised wait, terminate, transcript `Watcher` |
| `term.rs` | Raw-mode guard and window size (unix), Windows stubs |
| `adapters/mod.rs` | `AgentAdapter` trait, `TurnSignalSetup`, registry, `select` |
| `adapters/claude.rs` | Claude adapter: commands, paths, event parsing, structural extraction |
| `adapters/codex.rs` | Codex adapter: same surface, gated on verified real behavior |
| `score.rs` | Verb `zirv ctx score` |
| `handoff.rs` | Verb `zirv ctx handoff` plus `Handoff` type, distillation, structural fallback, storage |
| `resume.rs` | Verb `zirv ctx resume` |
| `hook.rs` | Verb `zirv ctx hook <stop\|prompt\|pre-compact\|notify>` |
| `status.rs` | Verb `zirv ctx status` |
| `run_loop.rs` | Verb `zirv ctx loop` (`loop` is a Rust keyword, so the module is `run_loop`) |
| `exec.rs` | Verb `zirv ctx exec` |
| `wrap.rs` | Verb `zirv ctx wrap` (PTY supervisor) |
| `usage.rs` | Verb `zirv ctx usage` and its `tee` statusline hook (Phase E) |
| `window.rs` | Usage-window state file (collector) and the transcript-sum estimator (Phase E) |
| `pace.rs` | Pure pacing gate, limit-hit matcher, and the shared wait helper (Phase E) |

**Modified:** `src/main.rs` (intercept `ctx` before `Input::parse`), `src/commands/mod.rs` (declare `ctx`), `src/commands/help.rs` (exclude `ctx.toml`), `Cargo.toml` (deps, version), `README.md`, `CLAUDE.md`.

**New test fixtures (data files, not cargo test targets):** `tests/fixtures/README.md`, `tests/fixtures/claude-real-session.jsonl`, `tests/fixtures/claude-real-session.expected.json`, `tests/fixtures/fake-agent.sh`, `tests/fixtures/fake-model.sh`, `tests/fixtures/stub-tui.sh`, `tests/fixtures/statusline-with-limits.json`, `tests/fixtures/statusline-no-limits.json`, `tests/fixtures/fake-statusline.sh`.

---

# Phase A: Core

Ships: config, state dir, normalized events, adapter layer, rot engine, `score`, `handoff`, `resume`, `hook`, `status`. Independently useful without any supervisor: hooks give advisories and `resume` gives manual recovery.

### Task A1: `zirv ctx` command family skeleton

**Files:**
- Create: `src/commands/ctx/mod.rs`
- Modify: `src/commands/mod.rs:1-4`
- Modify: `src/main.rs:16-19`
- Modify: `Cargo.toml:13-26`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub type CtxResult<T> = Result<T, Box<dyn std::error::Error>>;`
  - `pub struct CtxCli { pub verb: CtxVerb }`
  - `pub enum CtxVerb { Score(score::ScoreArgs), Handoff(handoff::HandoffArgs), Resume(resume::ResumeArgs), Hook(hook::HookArgs), Status(status::StatusArgs), Loop(run_loop::LoopArgs), Exec(exec::ExecArgs), Wrap(wrap::WrapArgs) }`
  - `pub fn dispatch(args: &[String]) -> i32` — `args[0]` is `"ctx"`; returns the process exit code.
  - Convention every later verb task follows: `pub fn run<W: std::io::Write>(args: &XArgs, w: &mut W) -> CtxResult<i32>`.

- [ ] **Step 1: Write the failing test**

Tests come first, so the only thing in the new file at this point is its test module. Create `src/commands/ctx/mod.rs` with exactly this content:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_score_verb() {
        let cli = CtxCli::try_parse_from(["zirv ctx", "score", "--transcript", "/tmp/t.jsonl"])
            .expect("score should parse");
        match cli.verb {
            CtxVerb::Score(args) => {
                assert_eq!(args.transcript, std::path::PathBuf::from("/tmp/t.jsonl"));
                assert_eq!(args.agent, None);
            }
            other => panic!("expected Score, got {other:?}"),
        }
    }

    #[test]
    fn loop_verb_keeps_its_cli_name() {
        let cli = CtxCli::try_parse_from(["zirv ctx", "loop", "--prompt", "go"])
            .expect("loop should parse");
        assert!(matches!(cli.verb, CtxVerb::Loop(_)));
    }

    #[test]
    fn unknown_verb_exits_two() {
        let code = dispatch(&["ctx".to_string(), "nope".to_string()]);
        assert_eq!(code, 2, "clap parse failure must map to exit code 2");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p zirv ctx::tests 2>&1 | tail -20`
Expected: FAIL to compile with `cannot find type CtxCli in this scope` and `module ctx not found` (until `src/commands/mod.rs` declares it).

- [ ] **Step 3: Write minimal implementation**

Add `pub mod ctx;` to `src/commands/mod.rs` (alphabetically first, before `create`). Then put this above the test module in `src/commands/ctx/mod.rs`:

```rust
use clap::{Args, Parser, Subcommand};

pub mod adapters;
pub mod config;
pub mod event;
pub mod exec;
pub mod handoff;
pub mod hook;
pub mod log;
pub mod resume;
pub mod rot;
pub mod run_loop;
pub mod score;
pub mod signal;
pub mod state;
pub mod status;
pub mod supervise;
pub mod term;
pub mod wrap;

/// Every ctx entry point returns this. Matches the error style used by the
/// rest of the crate (`Box<dyn std::error::Error>`).
pub type CtxResult<T> = Result<T, Box<dyn std::error::Error>>;

#[derive(Debug, Parser)]
#[command(
    name = "zirv ctx",
    about = "Autonomous context management for AI coding agents",
    disable_help_subcommand = true
)]
pub struct CtxCli {
    #[command(subcommand)]
    pub verb: CtxVerb,
}

#[derive(Debug, Subcommand)]
pub enum CtxVerb {
    /// Rot-score a session transcript and print JSON.
    Score(score::ScoreArgs),
    /// Distill a handoff from a transcript.
    Handoff(handoff::HandoffArgs),
    /// Start a clean interactive session with the latest handoff injected.
    Resume(resume::ResumeArgs),
    /// Agent hook entrypoints.
    Hook(hook::HookArgs),
    /// Show supervised sessions, scores and handoffs.
    Status(status::StatusArgs),
    /// Stateless loop runner: a fresh headless session per cycle.
    #[command(name = "loop")]
    Loop(run_loop::LoopArgs),
    /// Supervise one headless run.
    Exec(exec::ExecArgs),
    /// Supervise an interactive TUI through a PTY.
    Wrap(wrap::WrapArgs),
}

/// `args[0]` is the literal "ctx" as it appeared in argv.
pub fn dispatch(args: &[String]) -> i32 {
    let argv = std::iter::once("zirv ctx".to_string()).chain(args.iter().skip(1).cloned());
    let cli = match CtxCli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            return 2;
        }
    };

    let mut out = std::io::stdout();
    let result = match &cli.verb {
        CtxVerb::Score(a) => score::run(a, &mut out),
        CtxVerb::Handoff(a) => handoff::run(a, &mut out),
        CtxVerb::Resume(a) => resume::run(a, &mut out),
        CtxVerb::Hook(a) => hook::run(a, &mut out),
        CtxVerb::Status(a) => status::run(a, &mut out),
        CtxVerb::Loop(a) => run_loop::run(a, &mut out),
        CtxVerb::Exec(a) => exec::run(a, &mut out),
        CtxVerb::Wrap(a) => wrap::run(a, &mut out),
    };

    match result {
        Ok(code) => code,
        Err(e) => {
            crate::output::error(e);
            1
        }
    }
}

/// Placeholder arg struct shared by verbs that are implemented in later tasks.
/// Each later task replaces its own struct and `run` with the real thing.
#[derive(Debug, Args)]
pub struct Unimplemented {
    /// Accepts and ignores any trailing arguments.
    #[arg(num_args = 0.., allow_hyphen_values = true)]
    pub rest: Vec<String>,
}
```

Create one stub file per verb module so the tree compiles. Each stub is replaced by its own task later. For `score.rs` write the real args struct now (the test asserts on it):

```rust
// src/commands/ctx/score.rs
use std::io::Write;
use std::path::PathBuf;

use super::CtxResult;

#[derive(Debug, clap::Args)]
pub struct ScoreArgs {
    /// Path to the agent transcript (JSONL).
    #[arg(long)]
    pub transcript: PathBuf,
    /// Adapter name: claude or codex. Defaults to config, then claude.
    #[arg(long)]
    pub agent: Option<String>,
}

pub fn run<W: Write>(_args: &ScoreArgs, _w: &mut W) -> CtxResult<i32> {
    Err("zirv ctx score is not implemented yet".into())
}
```

For the other seven verbs write the same shape, e.g.:

```rust
// src/commands/ctx/run_loop.rs
use std::io::Write;

use super::CtxResult;

#[derive(Debug, clap::Args)]
pub struct LoopArgs {
    /// Prompt to run each cycle.
    #[arg(long)]
    pub prompt: Option<String>,
}

pub fn run<W: Write>(_args: &LoopArgs, _w: &mut W) -> CtxResult<i32> {
    Err("zirv ctx loop is not implemented yet".into())
}
```

Repeat verbatim for `handoff.rs` (`HandoffArgs`), `resume.rs` (`ResumeArgs`), `hook.rs` (`HookArgs`), `status.rs` (`StatusArgs`), `exec.rs` (`ExecArgs`), `wrap.rs` (`WrapArgs`), each with a single `#[arg(num_args = 0.., allow_hyphen_values = true)] pub rest: Vec<String>` field for now. Also create empty-but-compiling `config.rs`, `event.rs`, `log.rs`, `rot.rs`, `signal.rs`, `state.rs`, `supervise.rs`, `term.rs` and `adapters/mod.rs` (each containing only `// filled in by a later task`), plus `adapters/claude.rs` and `adapters/codex.rs` declared from `adapters/mod.rs` with `pub mod claude; pub mod codex;`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test ctx::tests 2>&1 | tail -20`
Expected: PASS, 3 tests.

- [ ] **Step 5: Write the failing interception test**

Add to the same test module in `src/commands/ctx/mod.rs`:

```rust
    #[test]
    fn ctx_is_intercepted_before_script_lookup() {
        // A repo with .zirv/ctx.toml must still route `zirv ctx ...` to the
        // built-in, never to a YAML/TOML script named "ctx".
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".zirv")).expect("mkdir");
        std::fs::write(dir.path().join(".zirv/ctx.toml"), "not = \"a script\"\n").expect("write");

        let exe = std::env::current_exe().expect("current_exe");
        let bin = exe
            .parent()
            .and_then(|p| p.parent())
            .expect("target/debug")
            .join("zirv");

        let out = std::process::Command::new(&bin)
            .args(["ctx", "score", "--help"])
            .current_dir(dir.path())
            .output()
            .expect("run zirv");

        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains("--transcript"),
            "built-in ctx help expected, got: {text}"
        );
    }
```

- [ ] **Step 6: Run it and see it fail**

Run: `cargo test ctx::tests::ctx_is_intercepted 2>&1 | tail -20`
Expected: FAIL. `main.rs` still sends `ctx` through `Input::parse`, so stdout contains a script-lookup error instead of clap help.

- [ ] **Step 7: Intercept in `main.rs`**

Insert at the top of `main`, before `let input = Input::parse();`:

```rust
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("ctx") {
        std::process::exit(commands::ctx::dispatch(&argv[1..]));
    }
```

And add `ctx` to the import list at the top: `use commands::{ctx, create::create_script_interactive, help::show_help, init::init_zirv, version::get_version};` (keep `ctx` referenced via `commands::ctx::dispatch` if the unused-import lint complains; prefer the fully qualified call and no new import).

- [ ] **Step 8: Add the dependencies**

In `Cargo.toml` under `[dependencies]` add:

```toml
portable-pty = "0.9.0"
uuid = { version = "1.24.0", features = ["v4"] }
```

and after the `[dev-dependencies]` block add:

```toml
[target.'cfg(unix)'.dependencies]
libc = "0.2.183"
```

- [ ] **Step 9: Run the full suite plus lints**

Run: `cargo test --verbose -- --test-threads=1 2>&1 | tail -20`
Expected: PASS, including the four ctx tests.
Run: `cargo clippy --all-targets -- -D warnings 2>&1 | tail -20`
Expected: no warnings.

- [ ] **Step 10: Commit**

```bash
git add Cargo.toml Cargo.lock src/main.rs src/commands/mod.rs src/commands/ctx
git commit -m "feat(ctx): add zirv ctx command family skeleton"
```

---

### Task A2: Config layering

**Files:**
- Modify: `src/commands/ctx/config.rs`
- Modify: `src/commands/help.rs:7-37`

**Interfaces:**
- Consumes: `CtxResult` from Task A1; `crate::utils::home_dir()` (reused, searched first: `grep -rn "home_dir" src/` shows `utils::home_dir` used by `input.rs`, `create.rs`, `help.rs`; nothing else resolves config paths, so nothing to extend).
- Produces:
  - `pub struct CtxConfig { pub agent: Option<String>, pub agent_bin: Option<String>, pub score: ScoreConfig, pub wrap: WrapConfig, pub supervise: SuperviseConfig, pub handoff: HandoffConfig }`
  - `pub struct ScoreConfig { pub window: usize, pub min_turns: usize, pub token_floor: u64, pub token_ceiling: u64, pub weight_tool_failure: f64, pub weight_repetition: f64, pub weight_marker: f64, pub repetition_threshold: usize, pub advise_at: u32, pub compact_at: u32, pub restart_at: u32, pub marker: String }`
  - `pub struct WrapConfig { pub debounce_ms: u64, pub inject_timeout_ms: u64 }`
  - `pub struct SuperviseConfig { pub max_restarts: u32, pub poll_ms: u64, pub interval_secs: u64, pub max_cycle_secs: u64, pub max_failures: u32, pub backoff_base_secs: u64, pub on_failure: Option<String> }`
  - `pub struct HandoffConfig { pub model: String, pub tail_items: usize }`
  - `pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;`
  - `pub fn env_from_process() -> impl Fn(&str) -> Option<String>`
  - `impl CtxConfig { pub fn load(repo: &std::path::Path, env: EnvLookup<'_>) -> CtxResult<Self> }`
  - `pub const DEFAULT_MARKER: &str = "[zirv]";`
  - `pub const CTX_CONFIG_FILE: &str = "ctx.toml";`

- [ ] **Step 1: Write the failing defaults and layering test**

Put this test module at the bottom of `src/commands/ctx/config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn defaults_match_the_spec() {
        let cfg = ScoreConfig::default();
        assert_eq!(cfg.window, 10);
        assert_eq!(cfg.min_turns, 10);
        assert_eq!(cfg.token_floor, 100_000);
        assert_eq!(cfg.token_ceiling, 160_000);
        assert_eq!(cfg.advise_at, 40);
        assert_eq!(cfg.compact_at, 60);
        assert_eq!(cfg.restart_at, 80);
        assert_eq!(cfg.marker, "[zirv]");
        assert_eq!(cfg.repetition_threshold, 3);
        assert_eq!(
            cfg.weight_tool_failure + cfg.weight_repetition + cfg.weight_marker,
            100.0,
            "weights must sum to 100 so an all-signals session can reach restart"
        );
        assert_eq!(WrapConfig::default().debounce_ms, 3000);
        assert_eq!(SuperviseConfig::default().max_restarts, 2);
        assert_eq!(HandoffConfig::default().model, "haiku");
        assert_eq!(HandoffConfig::default().tail_items, 5);
    }

    #[test]
    fn repo_file_overrides_defaults_and_env_overrides_repo() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[score]\nwindow = 4\ntoken_floor = 50000\nmarker = \"[repo]\"\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.score.window, 4);
        assert_eq!(cfg.score.token_floor, 50_000);
        assert_eq!(cfg.score.marker, "[repo]");
        assert_eq!(cfg.score.token_ceiling, 160_000, "untouched keys keep defaults");

        let env = env_map(&[("ZIRV_CTX_WINDOW", "7"), ("ZIRV_CTX_MARKER", "[env]")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.score.window, 7);
        assert_eq!(cfg.score.marker, "[env]");
        assert_eq!(cfg.score.token_floor, 50_000, "repo layer still applies");
    }

    #[test]
    fn numeric_looking_marker_stays_a_string() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_MARKER", "42")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.score.marker, "42");
    }

    #[test]
    fn unknown_config_key_is_rejected_loudly() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[score]\nwindwo = 4\n",
        )
        .expect("write");
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("typo must not be silently ignored");
        assert!(err.to_string().contains("windwo"), "got: {err}");
    }

    #[test]
    fn missing_files_are_not_an_error() {
        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.score.window, 10);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test ctx::config 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find struct ScoreConfig`.

- [ ] **Step 3: Write minimal implementation**

Replace the placeholder comment in `src/commands/ctx/config.rs` with:

```rust
use std::path::Path;

use serde::Deserialize;

use super::CtxResult;

pub const DEFAULT_MARKER: &str = "[zirv]";
pub const CTX_CONFIG_FILE: &str = "ctx.toml";

pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Wraps process env access so callers can pass a closure in tests instead of
/// mutating global state.
pub fn env_from_process() -> impl Fn(&str) -> Option<String> {
    |key: &str| std::env::var(key).ok()
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScoreConfig {
    pub window: usize,
    pub min_turns: usize,
    pub token_floor: u64,
    pub token_ceiling: u64,
    pub weight_tool_failure: f64,
    pub weight_repetition: f64,
    pub weight_marker: f64,
    pub repetition_threshold: usize,
    pub advise_at: u32,
    pub compact_at: u32,
    pub restart_at: u32,
    pub marker: String,
}

impl Default for ScoreConfig {
    fn default() -> Self {
        Self {
            window: 10,
            min_turns: 10,
            token_floor: 100_000,
            token_ceiling: 160_000,
            weight_tool_failure: 40.0,
            weight_repetition: 30.0,
            weight_marker: 30.0,
            repetition_threshold: 3,
            advise_at: 40,
            compact_at: 60,
            restart_at: 80,
            marker: DEFAULT_MARKER.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WrapConfig {
    pub debounce_ms: u64,
    pub inject_timeout_ms: u64,
}

impl Default for WrapConfig {
    fn default() -> Self {
        Self {
            debounce_ms: 3000,
            inject_timeout_ms: 20_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SuperviseConfig {
    pub max_restarts: u32,
    pub poll_ms: u64,
    pub interval_secs: u64,
    pub max_cycle_secs: u64,
    pub max_failures: u32,
    pub backoff_base_secs: u64,
    pub on_failure: Option<String>,
}

impl Default for SuperviseConfig {
    fn default() -> Self {
        Self {
            max_restarts: 2,
            poll_ms: 2000,
            interval_secs: 900,
            max_cycle_secs: 3600,
            max_failures: 5,
            backoff_base_secs: 60,
            on_failure: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HandoffConfig {
    pub model: String,
    /// How many trailing items of each kind the handoff context keeps: user
    /// messages, assistant texts and tool errors. One knob, because
    /// `structural_context` applies one limit to all three.
    pub tail_items: usize,
}

impl Default for HandoffConfig {
    fn default() -> Self {
        Self {
            model: "haiku".to_string(),
            tail_items: 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CtxConfig {
    pub agent: Option<String>,
    pub agent_bin: Option<String>,
    pub score: ScoreConfig,
    pub wrap: WrapConfig,
    pub supervise: SuperviseConfig,
    pub handoff: HandoffConfig,
}

#[derive(Debug, Clone, Copy)]
enum EnvKind {
    Int,
    Str,
}

const ENV_MAP: &[(&str, &[&str], EnvKind)] = &[
    ("ZIRV_CTX_AGENT", &["agent"], EnvKind::Str),
    ("ZIRV_CTX_AGENT_BIN", &["agent_bin"], EnvKind::Str),
    ("ZIRV_CTX_WINDOW", &["score", "window"], EnvKind::Int),
    ("ZIRV_CTX_MIN_TURNS", &["score", "min_turns"], EnvKind::Int),
    ("ZIRV_CTX_TOKEN_FLOOR", &["score", "token_floor"], EnvKind::Int),
    ("ZIRV_CTX_TOKEN_CEILING", &["score", "token_ceiling"], EnvKind::Int),
    ("ZIRV_CTX_MARKER", &["score", "marker"], EnvKind::Str),
    ("ZIRV_CTX_DEBOUNCE_MS", &["wrap", "debounce_ms"], EnvKind::Int),
    ("ZIRV_CTX_INJECT_TIMEOUT_MS", &["wrap", "inject_timeout_ms"], EnvKind::Int),
    ("ZIRV_CTX_MAX_RESTARTS", &["supervise", "max_restarts"], EnvKind::Int),
    ("ZIRV_CTX_POLL_MS", &["supervise", "poll_ms"], EnvKind::Int),
    ("ZIRV_CTX_INTERVAL_SECS", &["supervise", "interval_secs"], EnvKind::Int),
    ("ZIRV_CTX_MAX_CYCLE_SECS", &["supervise", "max_cycle_secs"], EnvKind::Int),
    ("ZIRV_CTX_MAX_FAILURES", &["supervise", "max_failures"], EnvKind::Int),
    ("ZIRV_CTX_ON_FAILURE", &["supervise", "on_failure"], EnvKind::Str),
    ("ZIRV_CTX_MODEL", &["handoff", "model"], EnvKind::Str),
];

fn merge(base: &mut toml::Table, over: toml::Table) {
    for (key, value) in over {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(existing)), toml::Value::Table(incoming)) => {
                merge(existing, incoming);
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

fn insert_path(table: &mut toml::Table, path: &[&str], value: toml::Value) {
    let Some((head, rest)) = path.split_first() else {
        return;
    };
    if rest.is_empty() {
        table.insert((*head).to_string(), value);
        return;
    }
    let entry = table
        .entry((*head).to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if !entry.is_table() {
        *entry = toml::Value::Table(toml::Table::new());
    }
    if let Some(child) = entry.as_table_mut() {
        insert_path(child, rest, value);
    }
}

fn env_value(raw: &str, kind: EnvKind) -> CtxResult<toml::Value> {
    match kind {
        EnvKind::Str => Ok(toml::Value::String(raw.to_string())),
        EnvKind::Int => raw
            .parse::<i64>()
            .map(toml::Value::Integer)
            .map_err(|_| format!("expected an integer, got '{raw}'").into()),
    }
}

fn read_layer(path: &Path, into: &mut toml::Table) -> CtxResult<()> {
    if !path.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(path)?;
    let layer: toml::Table = toml::from_str(&text)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    merge(into, layer);
    Ok(())
}

impl CtxConfig {
    /// Layers `~/.zirv/ctx.toml`, then `<repo>/.zirv/ctx.toml`, then
    /// `ZIRV_CTX_*`. Flags are applied by each verb after loading.
    pub fn load(repo: &Path, env: EnvLookup<'_>) -> CtxResult<Self> {
        let mut merged = toml::Table::new();

        if let Ok(home) = crate::utils::home_dir() {
            read_layer(
                &home.join(crate::utils::SCRIPT_DIR_NAME).join(CTX_CONFIG_FILE),
                &mut merged,
            )?;
        }
        read_layer(
            &repo.join(crate::utils::SCRIPT_DIR_NAME).join(CTX_CONFIG_FILE),
            &mut merged,
        )?;

        for (var, path, kind) in ENV_MAP {
            if let Some(raw) = env(var) {
                let value = env_value(&raw, *kind).map_err(|e| format!("{var}: {e}"))?;
                insert_path(&mut merged, path, value);
            }
        }

        toml::Value::Table(merged)
            .try_into()
            .map_err(|e| format!("invalid ctx config: {e}").into())
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test ctx::config 2>&1 | tail -20`
Expected: PASS, 5 tests.

- [ ] **Step 5: Write the failing `zirv help` regression test**

Add to the existing test module in `src/commands/help.rs`:

```rust
    /// `.zirv/ctx.toml` is a ctx config file, not a script. Parsing it as a
    /// Script used to make `zirv help` fail for the whole directory.
    #[test]
    fn test_show_help_ignores_ctx_config() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let temp_path = temp_dir.path().to_path_buf();
        let zirv_dir = setup_zirv_dir(&temp_path);

        write(
            zirv_dir.join("test.yaml"),
            "name: \"Test Script\"\ncommands: []\n",
        )?;
        write(zirv_dir.join("ctx.toml"), "[score]\nwindow = 4\n")?;

        let original_dir = env::current_dir()?;
        env::set_current_dir(&temp_path)?;

        let mut buffer = Cursor::new(Vec::new());
        let result = show_help(&mut buffer);

        env::set_current_dir(original_dir)?;

        result?;
        let output = String::from_utf8(buffer.into_inner())?;
        assert!(output.contains("Test Script"));
        assert!(!output.contains("ctx.toml"));

        Ok(())
    }
```

- [ ] **Step 6: Run it and see it fail**

Run: `cargo test help::tests::test_show_help_ignores_ctx_config 2>&1 | tail -20`
Expected: FAIL with a TOML deserialize error (`missing field \`name\``) propagated out of `show_help`.

- [ ] **Step 7: Exclude the config file**

In `src/commands/help.rs::write_scripts`, extend the filename guard:

```rust
        if path.is_file()
            && let Some(ext) = path.extension().and_then(|s| s.to_str())
            && SUPPORTED_EXTENSIONS.contains(&ext)
            && path.file_name().unwrap() != ".shortcuts.yaml"
            && path.file_name().unwrap() != crate::commands::ctx::config::CTX_CONFIG_FILE
        {
```

- [ ] **Step 8: Run it and see it pass**

Run: `cargo test help:: 2>&1 | tail -20`
Expected: PASS, 3 tests.

- [ ] **Step 9: Commit**

```bash
git add src/commands/ctx/config.rs src/commands/help.rs
git commit -m "feat(ctx): layered ctx config with env overrides"
```

---

### Task A3: State directory and decision log

**Files:**
- Modify: `src/commands/ctx/state.rs`
- Modify: `src/commands/ctx/log.rs`

**Interfaces:**
- Consumes: `CtxResult`.
- Produces:
  - `pub struct StateDir(PathBuf)` with `pub fn resolve(env: EnvLookup<'_>) -> CtxResult<Self>`, `pub fn from_root(root: PathBuf) -> Self`, `pub fn root(&self) -> &Path`, `pub fn handoffs(&self) -> PathBuf`, `pub fn sockets(&self) -> PathBuf`, `pub fn logs(&self) -> PathBuf`, `pub fn socket_for(&self, session: &str) -> PathBuf`, `pub fn ensure(&self) -> CtxResult<()>`
  - `pub fn repo_slug(path: &Path) -> String`
  - `pub const STATE_ENV: &str = "ZIRV_CTX_STATE_DIR";`
  - `pub fn now_secs() -> u64`
  - `pub struct Decision<'a> { pub ts: u64, pub session: &'a str, pub verb: &'a str, pub verdict: &'a str, pub score: u32, pub action: &'a str, pub detail: &'a str }`
  - `pub fn append(state: &StateDir, decision: &Decision<'_>) -> CtxResult<()>`
  - `pub fn tail(state: &StateDir, count: usize) -> CtxResult<Vec<String>>`

- [ ] **Step 1: Write the failing state test**

Bottom of `src/commands/ctx/state.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn env_override_wins_and_paths_hang_off_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let env: HashMap<String, String> =
            [(STATE_ENV.to_string(), tmp.path().display().to_string())].into();
        let state = StateDir::resolve(&|k| env.get(k).cloned()).expect("resolve");

        assert_eq!(state.root(), tmp.path());
        assert_eq!(state.handoffs(), tmp.path().join("handoffs"));
        assert_eq!(state.sockets(), tmp.path().join("s"));
        assert_eq!(state.logs(), tmp.path().join("logs"));

        state.ensure().expect("ensure");
        assert!(state.handoffs().is_dir());
        assert!(state.sockets().is_dir());
        assert!(state.logs().is_dir());
    }

    #[test]
    fn default_root_ends_with_zirv_ctx() {
        let env: HashMap<String, String> = HashMap::new();
        let state = StateDir::resolve(&|k| env.get(k).cloned()).expect("resolve");
        assert!(
            state.root().ends_with("zirv/ctx"),
            "got {}",
            state.root().display()
        );
    }

    #[test]
    fn socket_paths_stay_short_enough_for_macos() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let path = state.socket_for("00000000-0000-4000-8000-000000000001");
        assert!(
            path.to_string_lossy().ends_with("/s/00000000.sock"),
            "got {}",
            path.display()
        );
    }

    #[test]
    fn repo_slug_is_filesystem_safe() {
        assert_eq!(
            repo_slug(std::path::Path::new("/Users/x/Documents/my repo.git")),
            "-Users-x-Documents-my-repo-git"
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test ctx::state 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find struct StateDir`.

- [ ] **Step 3: Write minimal implementation**

```rust
use std::path::{Path, PathBuf};

use super::CtxResult;
use super::config::EnvLookup;

pub const STATE_ENV: &str = "ZIRV_CTX_STATE_DIR";

/// Seconds since the unix epoch. Zero-padded decimal seconds sort
/// lexicographically in chronological order, which is how handoffs and log
/// lines stay ordered without a date library.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Replaces every character outside `[A-Za-z0-9-]` with `-`, the same rule the
/// claude adapter uses for transcript directories.
pub fn repo_slug(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDir(PathBuf);

impl StateDir {
    pub fn from_root(root: PathBuf) -> Self {
        Self(root)
    }

    /// `ZIRV_CTX_STATE_DIR`, else the platform state dir, else the platform
    /// local data dir (macOS and Windows have no state dir), plus `zirv/ctx`.
    pub fn resolve(env: EnvLookup<'_>) -> CtxResult<Self> {
        if let Some(raw) = env(STATE_ENV) {
            return Ok(Self(PathBuf::from(raw)));
        }
        let base = dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .ok_or("could not determine a platform state directory")?;
        Ok(Self(base.join("zirv").join("ctx")))
    }

    pub fn root(&self) -> &Path {
        &self.0
    }

    pub fn handoffs(&self) -> PathBuf {
        self.0.join("handoffs")
    }

    /// Short on purpose: unix socket paths are capped near 104 bytes on macOS.
    pub fn sockets(&self) -> PathBuf {
        self.0.join("s")
    }

    pub fn logs(&self) -> PathBuf {
        self.0.join("logs")
    }

    /// First 8 hex characters of the session id keep the socket path short.
    pub fn socket_for(&self, session: &str) -> PathBuf {
        let short: String = session.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
        self.sockets().join(format!("{short}.sock"))
    }

    pub fn ensure(&self) -> CtxResult<()> {
        std::fs::create_dir_all(self.handoffs())?;
        std::fs::create_dir_all(self.sockets())?;
        std::fs::create_dir_all(self.logs())?;
        Ok(())
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test ctx::state 2>&1 | tail -20`
Expected: PASS, 4 tests.

- [ ] **Step 5: Write the failing decision-log test**

Bottom of `src/commands/ctx/log.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::state::StateDir;

    #[test]
    fn decisions_append_as_jsonl_and_tail_returns_newest_last() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        state.ensure().expect("ensure");

        for (i, action) in ["observe", "advise", "compact"].iter().enumerate() {
            append(
                &state,
                &Decision {
                    ts: 1_700_000_000 + i as u64,
                    session: "abc",
                    verb: "wrap",
                    verdict: "compact",
                    score: 64,
                    action,
                    detail: "",
                },
            )
            .expect("append");
        }

        let lines = tail(&state, 2).expect("tail");
        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains("\"action\":\"compact\""), "got {:?}", lines[1]);
        assert!(lines[0].contains("\"action\":\"advise\""), "got {:?}", lines[0]);

        let all = std::fs::read_to_string(state.logs().join("decisions.jsonl")).expect("read");
        assert_eq!(all.lines().count(), 3);
    }

    #[test]
    fn append_creates_the_log_dir_when_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("not-yet"));
        append(
            &state,
            &Decision {
                ts: 1,
                session: "s",
                verb: "hook",
                verdict: "healthy",
                score: 0,
                action: "observe",
                detail: "",
            },
        )
        .expect("append must create its directory");
        assert!(state.logs().join("decisions.jsonl").is_file());
    }
}
```

- [ ] **Step 6: Run it and see it fail**

Run: `cargo test ctx::log 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find struct Decision`.

- [ ] **Step 7: Write minimal implementation**

```rust
use std::io::Write;

use serde::Serialize;

use super::CtxResult;
use super::state::StateDir;

pub const LOG_FILE: &str = "decisions.jsonl";

#[derive(Debug, Serialize)]
pub struct Decision<'a> {
    pub ts: u64,
    pub session: &'a str,
    pub verb: &'a str,
    pub verdict: &'a str,
    pub score: u32,
    pub action: &'a str,
    pub detail: &'a str,
}

pub fn append(state: &StateDir, decision: &Decision<'_>) -> CtxResult<()> {
    let dir = state.logs();
    std::fs::create_dir_all(&dir)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(LOG_FILE))?;
    writeln!(file, "{}", serde_json::to_string(decision)?)?;
    Ok(())
}

pub fn tail(state: &StateDir, count: usize) -> CtxResult<Vec<String>> {
    let path = state.logs().join(LOG_FILE);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path)?;
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    let from = lines.len().saturating_sub(count);
    Ok(lines[from..].to_vec())
}
```

- [ ] **Step 8: Run it and see it pass**

Run: `cargo test ctx::log 2>&1 | tail -20`
Expected: PASS, 2 tests.

- [ ] **Step 9: Commit**

```bash
git add src/commands/ctx/state.rs src/commands/ctx/log.rs
git commit -m "feat(ctx): state directory resolution and decision log"
```

---

### Task A4: Normalized events and session types

**Files:**
- Modify: `src/commands/ctx/event.rs`

**Interfaces:**
- Consumes: `uuid` from Task A1.
- Produces:
  - `pub struct SessionId(String)` with `pub fn new_v4() -> Self`, `pub fn parse(s: &str) -> Self`, `pub fn as_str(&self) -> &str`, `impl std::fmt::Display`
  - `pub struct SessionRef { pub id: SessionId, pub cwd: std::path::PathBuf }`
  - `pub enum NormalizedEvent { TurnStart, AssistantFinal { text: String, input_tokens: u64 }, ToolCall { name: String, input_hash: u64 }, ToolResult { is_error: bool }, Compaction }`
  - `pub struct Capabilities { pub marker_signal: bool, pub token_usage: bool, pub turn_signal: bool }`
  - `pub struct StructuralContext { pub user_messages: Vec<String>, pub assistant_texts: Vec<String>, pub files_touched: Vec<String>, pub tool_errors: Vec<String> }`
  - `pub fn input_hash(input: &str) -> u64`

- [ ] **Step 1: Write the failing test**

Bottom of `src/commands/ctx/event.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_hash_is_fnv1a_64_and_stable() {
        assert_eq!(input_hash(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(input_hash("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(input_hash("foobar"), 0x8594_4171_f739_67e8);
        assert_eq!(input_hash("{\"command\":\"ls\"}"), input_hash("{\"command\":\"ls\"}"));
        assert_ne!(input_hash("{\"command\":\"ls\"}"), input_hash("{\"command\":\"ls -l\"}"));
    }

    #[test]
    fn session_ids_are_unique_uuids() {
        let a = SessionId::new_v4();
        let b = SessionId::new_v4();
        assert_ne!(a.as_str(), b.as_str());
        assert_eq!(a.as_str().len(), 36, "canonical hyphenated uuid");
        assert_eq!(a.to_string(), a.as_str());
    }

    #[test]
    fn capabilities_default_to_nothing_available() {
        let caps = Capabilities::default();
        assert!(!caps.marker_signal);
        assert!(!caps.token_usage);
        assert!(!caps.turn_signal);
    }

    #[test]
    fn events_compare_by_value() {
        assert_eq!(
            NormalizedEvent::ToolResult { is_error: true },
            NormalizedEvent::ToolResult { is_error: true }
        );
        assert_ne!(
            NormalizedEvent::ToolResult { is_error: true },
            NormalizedEvent::ToolResult { is_error: false }
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test ctx::event 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find function input_hash`.

- [ ] **Step 3: Write minimal implementation**

```rust
use std::path::PathBuf;

/// FNV-1a 64. Hand-rolled rather than `DefaultHasher` because the rot engine
/// must be deterministic across compiler versions.
pub fn input_hash(input: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    pub fn new_v4() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn parse(raw: &str) -> Self {
        Self(raw.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone)]
pub struct SessionRef {
    pub id: SessionId,
    pub cwd: PathBuf,
}

/// The only currency the rot engine and supervisors understand.
///
/// `AssistantFinal` is emitted for every assistant message: `text` holds the
/// concatenated text blocks and is empty for tool-only or thinking-only
/// messages. The marker signal groups by turn and takes the last non-empty
/// text; the token gate takes the most recent event's `input_tokens`
/// regardless of text, so mid-turn token growth is visible.
#[derive(Debug, Clone, PartialEq)]
pub enum NormalizedEvent {
    TurnStart,
    AssistantFinal { text: String, input_tokens: u64 },
    ToolCall { name: String, input_hash: u64 },
    ToolResult { is_error: bool },
    Compaction,
}

/// Which rot signals an adapter can actually feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    pub marker_signal: bool,
    pub token_usage: bool,
    pub turn_signal: bool,
}

/// Raw material for handoffs, extracted per-agent because it needs fields the
/// normalized stream deliberately drops (message text, tool inputs).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StructuralContext {
    pub user_messages: Vec<String>,
    pub assistant_texts: Vec<String>,
    pub files_touched: Vec<String>,
    pub tool_errors: Vec<String>,
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test ctx::event 2>&1 | tail -20`
Expected: PASS, 4 tests.

- [ ] **Step 5: Commit**

```bash
git add src/commands/ctx/event.rs
git commit -m "feat(ctx): normalized event and session types"
```

---

### Task A5: Record real claude transcript fixtures

**Files:**
- Create: `tests/fixtures/README.md`
- Create: `tests/fixtures/claude-real-session.jsonl`
- Create: `tests/fixtures/claude-real-session.expected.json`
- Create: `scripts/record-claude-fixture.py`

**Interfaces:**
- Consumes: nothing.
- Produces: fixture files consumed by Task A7 and Task A19. The expectations file has exactly these keys: `{"turn_start":u64,"assistant":u64,"tool_call":u64,"tool_result_error":u64,"tool_result_ok":u64,"compaction":u64,"last_context_tokens":u64,"files_touched_min":u64}`.

The spec requires real behavior, not assumptions, so the parser in Task A7 is written against a scrubbed copy of a genuine transcript rather than hand-written JSON.

- [ ] **Step 1: Find a real transcript that contains a compaction**

Run:

```bash
for f in $(ls -S ~/.claude/projects/*/*.jsonl | head -40); do
  n=$(grep -c '"subtype":"compact_boundary"' "$f" 2>/dev/null || true)
  [ "${n:-0}" -gt 0 ] && echo "$n $f"
done
```

Expected: at least one path printed. Note it as `<SOURCE>`. If nothing prints, widen `head -40` to `head -200`.

- [ ] **Step 2: Write the recorder script**

Create `scripts/record-claude-fixture.py`:

```python
#!/usr/bin/env python3
"""Copy + scrub a real claude transcript into tests/fixtures/.

Usage: python3 scripts/record-claude-fixture.py <source.jsonl>

Keeps a window around the first compact_boundary so the fixture exercises
compaction, sidechain filtering, tool errors and token usage. Rewrites
identifying paths, redacts credential-shaped strings, truncates long tool
output, and pins the session uuid. Also writes the expectations file the
Rust parser test asserts against.
"""
import json
import pathlib
import re
import sys

BEFORE, AFTER = 70, 110
FIXTURE_UUID = "00000000-0000-4000-8000-000000000001"
SECRET = re.compile(
    r"(sk-[A-Za-z0-9_\-]{8,}|gh[pousr]_[A-Za-z0-9]{8,}|ApiKey\s+\S+|Bearer\s+\S+|eyJ[A-Za-z0-9_\-]{10,})"
)
ROOT = pathlib.Path(__file__).resolve().parents[1]
OUT = ROOT / "tests" / "fixtures" / "claude-real-session.jsonl"
EXPECTED = ROOT / "tests" / "fixtures" / "claude-real-session.expected.json"


def scrub(node):
    if isinstance(node, dict):
        return {k: scrub(v) for k, v in node.items()}
    if isinstance(node, list):
        return [scrub(v) for v in node]
    if isinstance(node, str):
        text = node.replace("/Users/jonathansolskov", "/home/testuser")
        text = text.replace("jonathansolskov", "testuser")
        text = SECRET.sub("REDACTED", text)
        return text[:200] + ("..." if len(text) > 200 else "")
    return node


def tokens(usage):
    keys = ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens")
    return sum(int(usage.get(k) or 0) for k in keys)


def main():
    src = pathlib.Path(sys.argv[1])
    rows = [json.loads(line) for line in src.read_text().splitlines() if line.strip()]

    boundary = next(
        (i for i, r in enumerate(rows) if r.get("subtype") == "compact_boundary"), None
    )
    if boundary is None:
        sys.exit("source has no compact_boundary; pick another transcript")

    window = rows[max(0, boundary - BEFORE) : boundary + AFTER]
    kept = []
    for row in window:
        row = scrub(row)
        for key in ("sessionId", "session_id"):
            if key in row:
                row[key] = FIXTURE_UUID
        kept.append(row)

    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text("".join(json.dumps(r, separators=(",", ":")) + "\n" for r in kept))

    exp = dict.fromkeys(
        (
            "turn_start",
            "assistant",
            "tool_call",
            "tool_result_error",
            "tool_result_ok",
            "compaction",
        ),
        0,
    )
    last_tokens = 0
    files = set()
    for row in kept:
        if row.get("isSidechain") is True:
            continue
        kind = row.get("type")
        msg = row.get("message") or {}
        content = msg.get("content")
        if kind == "user":
            if row.get("isMeta") is True:
                continue
            results = [
                b
                for b in (content or [])
                if isinstance(b, dict) and b.get("type") == "tool_result"
            ]
            if not results:
                exp["turn_start"] += 1
            for block in results:
                key = "tool_result_error" if block.get("is_error") else "tool_result_ok"
                exp[key] += 1
        elif kind == "assistant":
            exp["assistant"] += 1
            last_tokens = tokens(msg.get("usage") or {})
            for block in content or []:
                if isinstance(block, dict) and block.get("type") == "tool_use":
                    exp["tool_call"] += 1
                    raw = block.get("input")
                    if isinstance(raw, dict):
                        for k in ("file_path", "notebook_path", "path"):
                            if isinstance(raw.get(k), str):
                                files.add(raw[k])
        elif kind == "system" and row.get("subtype") == "compact_boundary":
            exp["compaction"] += 1

    exp["last_context_tokens"] = last_tokens
    exp["files_touched_min"] = len(files)
    EXPECTED.write_text(json.dumps(exp, indent=2, sort_keys=True) + "\n")

    print(f"wrote {OUT} ({len(kept)} lines)")
    print(EXPECTED.read_text())


if __name__ == "__main__":
    main()
```

- [ ] **Step 3: Record the fixture**

Run: `python3 scripts/record-claude-fixture.py <SOURCE>`
Expected: prints the line count and the expectations JSON. Every count except `compaction` should be greater than zero; `compaction` must be exactly `1`. If `tool_result_error` is `0`, re-run against a different `<SOURCE>` (Task A7 asserts an error is present) or widen `AFTER` to `200`.

- [ ] **Step 4: Verify the scrub by hand**

Run: `grep -c 'jonathansolskov\|/Users/\|sk-ant\|ghp_' tests/fixtures/claude-real-session.jsonl`
Expected: `0`.

Run: `python3 -c "import json;[json.loads(l) for l in open('tests/fixtures/claude-real-session.jsonl')];print('valid jsonl')"`
Expected: `valid jsonl`.

- [ ] **Step 5: Write the provenance README**

Create `tests/fixtures/README.md`:

```markdown
# ctx test fixtures

Data files only. Cargo compiles nothing here; the Rust tests that read these
live inline in `src/commands/ctx/`.

## claude-real-session.jsonl

A scrubbed slice of a genuine Claude Code transcript, recorded with
`scripts/record-claude-fixture.py`. It deliberately contains a
`compact_boundary` system event, assistant messages with real `usage` fields,
`tool_use` blocks, `tool_result` blocks with and without `is_error`, and at
least one sidechain event.

Scrub rules applied by the recorder:

- `/Users/jonathansolskov` becomes `/home/testuser`, `jonathansolskov` becomes `testuser`
- credential-shaped strings (`sk-*`, `gh*_*`, `ApiKey ...`, `Bearer ...`, `eyJ*`) become `REDACTED`
- every string is truncated to 200 characters
- `sessionId` and `session_id` are pinned to `00000000-0000-4000-8000-000000000001`

To re-record: `python3 scripts/record-claude-fixture.py <path-to-transcript>`,
then re-run `cargo test ctx::adapters::claude`. Both the fixture and
`claude-real-session.expected.json` must be committed together.

## claude-real-session.expected.json

Event counts derived from the fixture by the recorder. The Rust parser test
asserts its own counts equal these, which pins parser regressions against real
data.

## fake-agent.sh, fake-model.sh, stub-tui.sh

Executable stand-ins used by the supervisor tests. See the header comment in
each script.
```

- [ ] **Step 6: Write the failing scrub guard test**

Add to `src/commands/ctx/adapters/claude.rs` (the file exists as a placeholder from Task A1):

```rust
#[cfg(test)]
mod tests {
    pub(crate) fn fixture_path(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    #[test]
    fn recorded_fixture_carries_no_personal_data() {
        let text = std::fs::read_to_string(fixture_path("claude-real-session.jsonl"))
            .expect("fixture must be committed");
        for needle in ["jonathansolskov", "/Users/", "sk-ant", "ghp_", "Bearer "] {
            assert!(
                !text.contains(needle),
                "fixture leaks '{needle}'; re-run scripts/record-claude-fixture.py"
            );
        }
        assert!(text.contains("compact_boundary"), "fixture must include a compaction");
        assert!(text.lines().count() >= 50, "fixture is too small to be representative");
    }
}
```

- [ ] **Step 7: Run it and see it pass**

Run: `cargo test ctx::adapters::claude::tests::recorded_fixture 2>&1 | tail -20`
Expected: PASS. (This one passes immediately; the guard exists to fail loudly if anyone re-records carelessly.)

- [ ] **Step 8: Commit**

```bash
git add scripts/record-claude-fixture.py tests/fixtures src/commands/ctx/adapters/claude.rs
git commit -m "test(ctx): record scrubbed real claude transcript fixture"
```

---

### Task A6: AgentAdapter trait and registry

**Files:**
- Modify: `src/commands/ctx/adapters/mod.rs`

**Interfaces:**
- Consumes: `NormalizedEvent`, `SessionId`, `SessionRef`, `Capabilities`, `StructuralContext` (A4); `CtxResult` (A1).
- Produces:

```rust
pub struct TurnSignalSetup { pub env: Vec<(String, String)>, pub instructions: String }

pub trait AgentAdapter {
    fn name(&self) -> &'static str;
    fn ready(&self) -> CtxResult<()>;
    fn detect(&self, command: &[String]) -> bool;
    fn headless_cmd(&self, prompt: &str, session: &SessionId, extra: &[String]) -> Command;
    fn interactive_cmd(&self, initial_prompt: Option<&str>, extra: &[String]) -> Command;
    fn distiller_cmd(&self, model: &str) -> Command;
    fn transcript_path(&self, session: &SessionRef) -> PathBuf;
    fn parse_events(&self, jsonl: &str) -> Vec<NormalizedEvent>;
    fn structural_context(&self, jsonl: &str, last_n: usize) -> StructuralContext;
    fn compact_command(&self) -> Option<&'static str>;
    fn quit_sequence(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    fn register_turn_signal(&self, session: &SessionRef, socket: &Path) -> TurnSignalSetup;
}

pub fn select(name: Option<&str>, command: &[String], bin: Option<&str>) -> CtxResult<Box<dyn AgentAdapter>>;
pub fn all(bin: Option<&str>) -> Vec<Box<dyn AgentAdapter>>;
```

`Command` is `std::process::Command`: supervisors are thread-based (portable-pty is blocking) so tokio's process type would buy nothing and force async through the whole call tree.

- [ ] **Step 1: Write the failing test**

Bottom of `src/commands/ctx/adapters/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_name_wins() {
        let adapter = select(Some("claude"), &[], None).expect("claude selects");
        assert_eq!(adapter.name(), "claude");
    }

    #[test]
    fn detection_reads_the_wrapped_argv() {
        let cmd = vec!["/opt/homebrew/bin/claude".to_string(), "--resume".to_string()];
        let adapter = select(None, &cmd, None).expect("detect claude");
        assert_eq!(adapter.name(), "claude");
    }

    #[test]
    fn empty_command_defaults_to_claude() {
        let adapter = select(None, &[], None).expect("default");
        assert_eq!(adapter.name(), "claude");
    }

    #[test]
    fn unknown_name_is_an_error_that_lists_the_options() {
        let err = select(Some("gemini"), &[], None).expect_err("unknown agent");
        let msg = err.to_string();
        assert!(msg.contains("gemini"), "got {msg}");
        assert!(msg.contains("claude"), "error should list known adapters: {msg}");
    }

    #[test]
    fn registry_exposes_both_v1_adapters() {
        let names: Vec<&str> = all(None).iter().map(|a| a.name()).collect();
        assert_eq!(names, vec!["claude", "codex"]);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test ctx::adapters::tests 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find function select`.

- [ ] **Step 3: Write minimal implementation**

Replace the placeholder in `src/commands/ctx/adapters/mod.rs`:

```rust
use std::path::{Path, PathBuf};
use std::process::Command;

pub mod claude;
pub mod codex;

use super::CtxResult;
use super::event::{Capabilities, NormalizedEvent, SessionId, SessionRef, StructuralContext};

/// How an adapter arranges for turn-boundary events to reach a supervisor's
/// socket. `env` is injected into the launched agent so the hook that runs
/// inside it can find the socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSignalSetup {
    pub env: Vec<(String, String)>,
    pub instructions: String,
}

pub const SOCKET_ENV: &str = "ZIRV_CTX_SOCKET";
pub const SESSION_ENV: &str = "ZIRV_CTX_SESSION";

pub trait AgentAdapter {
    fn name(&self) -> &'static str;

    /// `Err` when the adapter exists but is not safe to use yet, so callers
    /// fail loudly instead of scoring garbage.
    fn ready(&self) -> CtxResult<()>;

    fn detect(&self, command: &[String]) -> bool;

    fn headless_cmd(&self, prompt: &str, session: &SessionId, extra: &[String]) -> Command;
    fn interactive_cmd(&self, initial_prompt: Option<&str>, extra: &[String]) -> Command;
    fn distiller_cmd(&self, model: &str) -> Command;

    fn transcript_path(&self, session: &SessionRef) -> PathBuf;
    fn parse_events(&self, jsonl: &str) -> Vec<NormalizedEvent>;
    fn structural_context(&self, jsonl: &str, last_n: usize) -> StructuralContext;

    fn compact_command(&self) -> Option<&'static str>;
    fn quit_sequence(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    fn register_turn_signal(&self, session: &SessionRef, socket: &Path) -> TurnSignalSetup;
}

pub fn all(bin: Option<&str>) -> Vec<Box<dyn AgentAdapter>> {
    vec![
        Box::new(claude::ClaudeAdapter::new(bin)),
        Box::new(codex::CodexAdapter::new(bin)),
    ]
}

/// Explicit `--agent` name, else detection from the wrapped argv, else claude.
pub fn select(
    name: Option<&str>,
    command: &[String],
    bin: Option<&str>,
) -> CtxResult<Box<dyn AgentAdapter>> {
    let adapters = all(bin);

    if let Some(name) = name {
        let found = adapters.into_iter().find(|a| a.name() == name);
        let adapter = found.ok_or_else(|| {
            format!(
                "unknown agent '{name}'; known adapters: {}",
                all(None).iter().map(|a| a.name()).collect::<Vec<_>>().join(", ")
            )
        })?;
        adapter.ready()?;
        return Ok(adapter);
    }

    if let Some(adapter) = adapters.into_iter().find(|a| a.detect(command)) {
        adapter.ready()?;
        return Ok(adapter);
    }

    let adapter: Box<dyn AgentAdapter> = Box::new(claude::ClaudeAdapter::new(bin));
    adapter.ready()?;
    Ok(adapter)
}
```

Add the minimal `ClaudeAdapter` and `CodexAdapter` shells needed to compile (Tasks A7 to A10 fill them in). In `adapters/claude.rs`, above the existing test module:

```rust
use std::path::{Path, PathBuf};
use std::process::Command;

use super::super::CtxResult;
use super::super::event::{Capabilities, NormalizedEvent, SessionId, SessionRef, StructuralContext};
use super::{AgentAdapter, TurnSignalSetup};

// Scaffold only: Task A8 replaces this struct with the real one, which splits a
// multi-word bin into `program` plus `bin_args`.
#[derive(Debug, Clone)]
pub struct ClaudeAdapter {
    bin: String,
}

impl ClaudeAdapter {
    pub fn new(bin: Option<&str>) -> Self {
        Self {
            bin: bin.unwrap_or("claude").to_string(),
        }
    }
}

impl AgentAdapter for ClaudeAdapter {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn ready(&self) -> CtxResult<()> {
        Ok(())
    }

    fn detect(&self, command: &[String]) -> bool {
        command
            .first()
            .and_then(|p| Path::new(p).file_name())
            .map(|f| f.to_string_lossy() == "claude")
            .unwrap_or(false)
    }

    fn headless_cmd(&self, _prompt: &str, _session: &SessionId, _extra: &[String]) -> Command {
        Command::new(&self.bin)
    }

    fn interactive_cmd(&self, _initial_prompt: Option<&str>, _extra: &[String]) -> Command {
        Command::new(&self.bin)
    }

    fn distiller_cmd(&self, _model: &str) -> Command {
        Command::new(&self.bin)
    }

    fn transcript_path(&self, _session: &SessionRef) -> PathBuf {
        PathBuf::new()
    }

    fn parse_events(&self, _jsonl: &str) -> Vec<NormalizedEvent> {
        Vec::new()
    }

    fn structural_context(&self, _jsonl: &str, _last_n: usize) -> StructuralContext {
        StructuralContext::default()
    }

    fn compact_command(&self) -> Option<&'static str> {
        Some("/compact")
    }

    fn quit_sequence(&self) -> &'static str {
        "/exit\r"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            marker_signal: true,
            token_usage: true,
            turn_signal: true,
        }
    }

    fn register_turn_signal(&self, _session: &SessionRef, _socket: &Path) -> TurnSignalSetup {
        TurnSignalSetup {
            env: Vec::new(),
            instructions: String::new(),
        }
    }
}
```

In `adapters/codex.rs` write the same shape with `bin` defaulting to `"codex"`, `name()` returning `"codex"`, `detect` matching the `codex` file name, `quit_sequence()` returning `"/quit\r"`, `capabilities()` all `false`, and:

```rust
    fn ready(&self) -> CtxResult<()> {
        Err("the codex adapter is not verified yet (see plan task A9/A10); \
             pass --agent claude or wait for the codex parser"
            .into())
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test ctx::adapters 2>&1 | tail -20`
Expected: PASS, 6 tests (5 registry tests plus the fixture guard from A5).

- [ ] **Step 5: Commit**

```bash
git add src/commands/ctx/adapters
git commit -m "feat(ctx): AgentAdapter trait and adapter registry"
```

---

### Task A7: Claude adapter event parsing

**Files:**
- Modify: `src/commands/ctx/adapters/claude.rs`
- Test: same file, inline `mod tests`

**Interfaces:**
- Consumes: `NormalizedEvent`, `input_hash` (A4); fixtures (A5); `AgentAdapter` (A6).
- Produces: a working `ClaudeAdapter::parse_events`, plus the free functions `pub fn parse_events(jsonl: &str) -> Vec<NormalizedEvent>` and `pub fn context_tokens_of(usage: &serde_json::Value) -> u64` for direct testing.

- [ ] **Step 1: Write the failing semantic tests**

Add to the `mod tests` in `src/commands/ctx/adapters/claude.rs`:

```rust
    use super::*;
    use crate::commands::ctx::event::{NormalizedEvent, input_hash};

    #[test]
    fn context_tokens_sum_the_cache_fields() {
        // Verified against a real transcript: input_tokens alone is 2 in a
        // 110k-token session, so the cache fields carry the real size.
        let usage = serde_json::json!({
            "input_tokens": 2,
            "cache_creation_input_tokens": 457,
            "cache_read_input_tokens": 108_427,
            "output_tokens": 577
        });
        assert_eq!(context_tokens_of(&usage), 108_886);
    }

    #[test]
    fn a_real_prompt_starts_a_turn_but_a_tool_result_does_not() {
        let jsonl = concat!(
            r#"{"type":"user","message":{"content":"do the thing"}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#,
            "\n"
        );
        let events = parse_events(jsonl);
        assert_eq!(
            events,
            vec![
                NormalizedEvent::TurnStart,
                NormalizedEvent::ToolResult { is_error: false },
            ]
        );
    }

    #[test]
    fn missing_is_error_counts_as_success() {
        let jsonl = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#;
        assert_eq!(parse_events(jsonl), vec![NormalizedEvent::ToolResult { is_error: false }]);
    }

    #[test]
    fn assistant_yields_text_tokens_and_tool_calls() {
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"content":["#,
            r#"{"type":"thinking","thinking":"hmm"},"#,
            r#"{"type":"text","text":"[zirv] on it"},"#,
            r#"{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}"#,
            r#"],"usage":{"input_tokens":10,"cache_read_input_tokens":90}}}"#
        );
        let events = parse_events(jsonl);
        assert_eq!(
            events,
            vec![
                NormalizedEvent::AssistantFinal {
                    text: "[zirv] on it".to_string(),
                    input_tokens: 100,
                },
                NormalizedEvent::ToolCall {
                    name: "Bash".to_string(),
                    input_hash: input_hash("{\"command\":\"ls\"}"),
                },
            ]
        );
    }

    #[test]
    fn tool_only_assistant_messages_still_report_tokens() {
        let jsonl = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t","name":"Read","input":{}}],"usage":{"input_tokens":5}}}"#;
        let events = parse_events(jsonl);
        assert_eq!(
            events[0],
            NormalizedEvent::AssistantFinal {
                text: String::new(),
                input_tokens: 5
            }
        );
    }

    #[test]
    fn compact_boundary_becomes_a_compaction_event() {
        let jsonl = r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"manual"}}"#;
        assert_eq!(parse_events(jsonl), vec![NormalizedEvent::Compaction]);
    }

    #[test]
    fn sidechain_meta_and_garbage_lines_are_skipped() {
        let jsonl = concat!(
            r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"sub"}],"usage":{}}}"#,
            "\n",
            r#"{"type":"user","isMeta":true,"message":{"content":"hook noise"}}"#,
            "\n",
            "not json at all\n",
            "\n",
            r#"{"type":"pr-link","prNumber":7}"#,
            "\n",
            r#"{"type":"user","message":{"content":"real prompt"}}"#,
            "\n"
        );
        assert_eq!(parse_events(jsonl), vec![NormalizedEvent::TurnStart]);
    }

    #[test]
    fn real_fixture_matches_recorded_expectations() {
        let jsonl = std::fs::read_to_string(fixture_path("claude-real-session.jsonl"))
            .expect("fixture");
        let expected: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(fixture_path("claude-real-session.expected.json"))
                .expect("expectations"),
        )
        .expect("valid json");

        let events = parse_events(&jsonl);
        let count = |pred: &dyn Fn(&NormalizedEvent) -> bool| {
            events.iter().filter(|e| pred(e)).count() as u64
        };
        let want = |key: &str| expected[key].as_u64().unwrap_or_else(|| panic!("{key} missing"));

        assert_eq!(count(&|e| matches!(e, NormalizedEvent::TurnStart)), want("turn_start"));
        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::AssistantFinal { .. })),
            want("assistant")
        );
        assert_eq!(count(&|e| matches!(e, NormalizedEvent::ToolCall { .. })), want("tool_call"));
        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::ToolResult { is_error: true })),
            want("tool_result_error")
        );
        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::ToolResult { is_error: false })),
            want("tool_result_ok")
        );
        assert_eq!(count(&|e| matches!(e, NormalizedEvent::Compaction)), want("compaction"));

        let last_tokens = events
            .iter()
            .rev()
            .find_map(|e| match e {
                NormalizedEvent::AssistantFinal { input_tokens, .. } => Some(*input_tokens),
                _ => None,
            })
            .expect("fixture has assistant events");
        assert_eq!(last_tokens, want("last_context_tokens"));
        assert!(want("tool_result_error") >= 1, "fixture must contain a tool error");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ctx::adapters::claude 2>&1 | tail -30`
Expected: FAIL to compile, `cannot find function parse_events` / `context_tokens_of`.

- [ ] **Step 3: Write minimal implementation**

Add to `src/commands/ctx/adapters/claude.rs` (above the tests) and delegate the trait method to it:

```rust
use serde_json::Value;

use super::super::event::input_hash;

fn text_of(message: &Value) -> String {
    message
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Real context size is `input_tokens` plus both cache fields; the bare
/// `input_tokens` field is near zero once prompt caching kicks in.
pub fn context_tokens_of(usage: &Value) -> u64 {
    ["input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"]
        .iter()
        .filter_map(|key| usage.get(*key).and_then(Value::as_u64))
        .sum()
}

pub fn parse_events(jsonl: &str) -> Vec<NormalizedEvent> {
    let mut events = Vec::new();

    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if row.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }

        match row.get("type").and_then(Value::as_str) {
            Some("user") => {
                if row.get("isMeta").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let message = row.get("message").cloned().unwrap_or(Value::Null);
                let results: Vec<&Value> = message
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                            .collect()
                    })
                    .unwrap_or_default();

                if results.is_empty() {
                    events.push(NormalizedEvent::TurnStart);
                    continue;
                }
                for block in results {
                    events.push(NormalizedEvent::ToolResult {
                        is_error: block.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                    });
                }
            }
            Some("assistant") => {
                let message = row.get("message").cloned().unwrap_or(Value::Null);
                let input_tokens = message.get("usage").map(context_tokens_of).unwrap_or(0);
                events.push(NormalizedEvent::AssistantFinal {
                    text: text_of(&message),
                    input_tokens,
                });

                if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                    for block in blocks
                        .iter()
                        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                    {
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_string();
                        let raw = block.get("input").map(Value::to_string).unwrap_or_default();
                        events.push(NormalizedEvent::ToolCall {
                            name,
                            input_hash: input_hash(&raw),
                        });
                    }
                }
            }
            Some("system") if row.get("subtype").and_then(Value::as_str) == Some("compact_boundary") => {
                events.push(NormalizedEvent::Compaction);
            }
            _ => {}
        }
    }

    events
}
```

Then change the trait method body to `parse_events(jsonl)`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test ctx::adapters::claude 2>&1 | tail -30`
Expected: PASS, 9 tests.

- [ ] **Step 5: Commit**

```bash
git add src/commands/ctx/adapters/claude.rs
git commit -m "feat(ctx): parse claude transcripts into normalized events"
```

---

### Task A8: Claude adapter commands, paths and structural context

**Files:**
- Modify: `src/commands/ctx/adapters/claude.rs`

**Interfaces:**
- Consumes: `SessionId`, `SessionRef`, `StructuralContext` (A4); `TurnSignalSetup`, `SOCKET_ENV`, `SESSION_ENV` (A6).
- Produces: real bodies for `headless_cmd`, `interactive_cmd`, `distiller_cmd`, `transcript_path`, `structural_context`, `register_turn_signal`, plus `pub fn project_slug(cwd: &Path) -> String` and `pub fn structural_context(jsonl: &str, last_n: usize) -> StructuralContext`.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` in `src/commands/ctx/adapters/claude.rs`:

```rust
    use crate::commands::ctx::adapters::{AgentAdapter, SESSION_ENV, SOCKET_ENV};
    use crate::commands::ctx::event::{SessionId, SessionRef};

    #[test]
    fn project_slug_matches_on_disk_evidence() {
        assert_eq!(
            project_slug(std::path::Path::new("/Users/x/Documents/Privat/zirv-fitness-tracking")),
            "-Users-x-Documents-Privat-zirv-fitness-tracking"
        );
        // A dot becomes a dash, which is why worktrees show up as `--claude-worktrees`.
        assert_eq!(
            project_slug(std::path::Path::new("/Users/x/repo/.claude-worktrees/b")),
            "-Users-x-repo--claude-worktrees-b"
        );
    }

    #[test]
    fn transcript_path_is_derived_from_home_and_cwd() {
        let home = tempfile::tempdir().expect("tempdir");
        let adapter = ClaudeAdapter::new(None).with_home(home.path().to_path_buf());
        let session = SessionRef {
            id: SessionId::parse("11111111-2222-4333-8444-555555555555"),
            cwd: std::path::PathBuf::from("/work/repo"),
        };
        assert_eq!(
            adapter.transcript_path(&session),
            home.path()
                .join(".claude/projects/-work-repo/11111111-2222-4333-8444-555555555555.jsonl")
        );
    }

    #[test]
    fn transcript_path_falls_back_to_scanning_when_the_slug_misses() {
        let home = tempfile::tempdir().expect("tempdir");
        let real = home.path().join(".claude/projects/some-other-slug");
        std::fs::create_dir_all(&real).expect("mkdir");
        let actual = real.join("11111111-2222-4333-8444-555555555555.jsonl");
        std::fs::write(&actual, "").expect("write");

        let adapter = ClaudeAdapter::new(None).with_home(home.path().to_path_buf());
        let session = SessionRef {
            id: SessionId::parse("11111111-2222-4333-8444-555555555555"),
            cwd: std::path::PathBuf::from("/work/repo"),
        };
        assert_eq!(adapter.transcript_path(&session), actual);
    }

    #[test]
    fn headless_cmd_pins_the_session_id() {
        let adapter = ClaudeAdapter::new(Some("/tmp/fake-claude"));
        let cmd = adapter.headless_cmd(
            "do the work",
            &SessionId::parse("abc"),
            &["--model".to_string(), "sonnet".to_string()],
        );
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();
        assert_eq!(cmd.get_program().to_string_lossy(), "/tmp/fake-claude");
        assert_eq!(
            args,
            vec![
                "-p".to_string(),
                "do the work".to_string(),
                "--session-id".to_string(),
                "abc".to_string(),
                "--model".to_string(),
                "sonnet".to_string(),
            ]
        );
    }

    #[test]
    fn interactive_cmd_passes_the_initial_prompt_positionally() {
        let adapter = ClaudeAdapter::new(None);
        let with = adapter.interactive_cmd(Some("resume this"), &[]);
        let args: Vec<String> = with.get_args().map(|a| a.to_string_lossy().to_string()).collect();
        assert_eq!(args, vec!["resume this".to_string()]);

        let without = adapter.interactive_cmd(None, &["--continue".to_string()]);
        let args: Vec<String> = without.get_args().map(|a| a.to_string_lossy().to_string()).collect();
        assert_eq!(args, vec!["--continue".to_string()]);
    }

    #[test]
    fn distiller_cmd_uses_a_cheap_model_and_reads_stdin() {
        let adapter = ClaudeAdapter::new(None);
        let cmd = adapter.distiller_cmd("haiku");
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();
        assert_eq!(
            args,
            vec![
                "-p".to_string(),
                "--model".to_string(),
                "haiku".to_string(),
                "--output-format".to_string(),
                "text".to_string(),
            ]
        );
    }

    /// A multi-word agent bin (`ZIRV_CTX_AGENT_BIN="sh /tmp/stub.sh"`) has to work
    /// for all three invocation kinds: exec restarts build headless commands,
    /// handoff distillation builds a distiller command, and wrap restarts build an
    /// interactive one.
    #[test]
    fn a_multi_word_agent_bin_is_split_across_every_command_kind() {
        let adapter = ClaudeAdapter::new(Some("sh /tmp/stub.sh"));

        let headless = adapter.headless_cmd("go", &SessionId::parse("abc"), &[]);
        assert_eq!(headless.get_program().to_string_lossy(), "sh");
        let args: Vec<String> = headless
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec![
                "/tmp/stub.sh".to_string(),
                "-p".to_string(),
                "go".to_string(),
                "--session-id".to_string(),
                "abc".to_string(),
            ],
            "the bin arguments come before the agent flags"
        );

        let interactive = adapter.interactive_cmd(Some("resume"), &[]);
        assert_eq!(interactive.get_program().to_string_lossy(), "sh");
        let args: Vec<String> = interactive
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args, vec!["/tmp/stub.sh".to_string(), "resume".to_string()]);

        let distiller = adapter.distiller_cmd("haiku");
        assert_eq!(distiller.get_program().to_string_lossy(), "sh");
        let args: Vec<String> = distiller
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec![
                "/tmp/stub.sh".to_string(),
                "-p".to_string(),
                "--model".to_string(),
                "haiku".to_string(),
                "--output-format".to_string(),
                "text".to_string(),
            ]
        );
    }

    #[test]
    fn a_single_word_bin_and_extra_whitespace_still_work() {
        let adapter = ClaudeAdapter::new(Some("  /opt/homebrew/bin/claude  "));
        let cmd = adapter.interactive_cmd(None, &[]);
        assert_eq!(
            cmd.get_program().to_string_lossy(),
            "/opt/homebrew/bin/claude"
        );
        assert_eq!(cmd.get_args().count(), 0);
    }

    #[test]
    fn turn_signal_setup_exports_socket_and_session() {
        let adapter = ClaudeAdapter::new(None);
        let session = SessionRef {
            id: SessionId::parse("sess-1"),
            cwd: std::path::PathBuf::from("/work/repo"),
        };
        let setup = adapter.register_turn_signal(&session, std::path::Path::new("/tmp/s/ab.sock"));
        assert!(setup.env.contains(&(SOCKET_ENV.to_string(), "/tmp/s/ab.sock".to_string())));
        assert!(setup.env.contains(&(SESSION_ENV.to_string(), "sess-1".to_string())));
        assert!(
            setup.instructions.contains("zirv ctx hook stop"),
            "instructions should name the hook command: {}",
            setup.instructions
        );
    }

    #[test]
    fn structural_context_extracts_prompts_files_and_errors() {
        let jsonl = concat!(
            r#"{"type":"user","message":{"content":"first prompt"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"a","name":"Read","input":{"file_path":"/work/src/lib.rs"}}],"usage":{}}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"boom: file missing","is_error":true}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"[zirv] fixed it"}],"usage":{}}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"text","text":"second prompt"}]}}"#,
            "\n"
        );
        let ctx = structural_context(jsonl, 5);
        assert_eq!(ctx.user_messages, vec!["first prompt", "second prompt"]);
        assert_eq!(ctx.assistant_texts, vec!["[zirv] fixed it"]);
        assert_eq!(ctx.files_touched, vec!["/work/src/lib.rs"]);
        assert_eq!(ctx.tool_errors.len(), 1);
        assert!(ctx.tool_errors[0].contains("boom"));
    }

    #[test]
    fn structural_context_keeps_only_the_last_n_and_dedupes_files() {
        let mut jsonl = String::new();
        for i in 0..6 {
            jsonl.push_str(&format!(
                "{{\"type\":\"user\",\"message\":{{\"content\":\"p{i}\"}}}}\n"
            ));
            jsonl.push_str(
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"a\",\"name\":\"Read\",\"input\":{\"file_path\":\"/same.rs\"}}],\"usage\":{}}}\n",
            );
        }
        let ctx = structural_context(&jsonl, 2);
        assert_eq!(ctx.user_messages, vec!["p4", "p5"]);
        assert_eq!(ctx.files_touched, vec!["/same.rs"]);
    }

    #[test]
    fn structural_context_survives_the_real_fixture() {
        let jsonl = std::fs::read_to_string(fixture_path("claude-real-session.jsonl"))
            .expect("fixture");
        let expected: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(fixture_path("claude-real-session.expected.json"))
                .expect("expectations"),
        )
        .expect("valid json");
        let ctx = structural_context(&jsonl, 5);
        assert!(ctx.user_messages.len() <= 5);
        assert!(
            ctx.files_touched.len() as u64 >= expected["files_touched_min"].as_u64().unwrap_or(0),
            "files_touched should find at least the recorded count"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ctx::adapters::claude 2>&1 | tail -30`
Expected: FAIL to compile, `no method named with_home`, `cannot find function project_slug`.

- [ ] **Step 3: Write minimal implementation**

Extend `ClaudeAdapter` and replace the stub bodies:

```rust
#[derive(Debug, Clone)]
pub struct ClaudeAdapter {
    program: String,
    bin_args: Vec<String>,
    home: Option<PathBuf>,
}

impl ClaudeAdapter {
    /// `bin` may carry arguments, so `"sh /tmp/stub.sh"` and
    /// `"/usr/bin/env claude"` both work. The first token is the program and the
    /// rest lead every command this adapter builds.
    pub fn new(bin: Option<&str>) -> Self {
        let raw = bin.unwrap_or("claude").trim();
        let mut parts = raw.split_whitespace().map(str::to_string);
        let program = parts.next().unwrap_or_else(|| "claude".to_string());
        Self {
            program,
            bin_args: parts.collect(),
            home: None,
        }
    }

    /// Test seam: pins the home directory the transcript path is built from.
    pub fn with_home(mut self, home: PathBuf) -> Self {
        self.home = Some(home);
        self
    }

    /// Every command starts here so the program and its leading arguments are
    /// applied uniformly to headless, interactive and distiller invocations.
    fn base(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.bin_args);
        cmd
    }

    fn home_dir(&self) -> PathBuf {
        self.home
            .clone()
            .or_else(|| crate::utils::home_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

/// Claude stores transcripts under a slug of the cwd with every character
/// outside `[A-Za-z0-9-]` replaced by `-`.
pub fn project_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect()
}
```

Trait bodies:

```rust
    fn headless_cmd(&self, prompt: &str, session: &SessionId, extra: &[String]) -> Command {
        let mut cmd = self.base();
        cmd.arg("-p")
            .arg(prompt)
            .arg("--session-id")
            .arg(session.as_str())
            .args(extra);
        cmd
    }

    fn interactive_cmd(&self, initial_prompt: Option<&str>, extra: &[String]) -> Command {
        let mut cmd = self.base();
        if let Some(prompt) = initial_prompt {
            cmd.arg(prompt);
        }
        cmd.args(extra);
        cmd
    }

    /// The distillation prompt is piped to stdin so a long transcript tail
    /// never hits argv length limits.
    fn distiller_cmd(&self, model: &str) -> Command {
        let mut cmd = self.base();
        cmd.arg("-p")
            .arg("--model")
            .arg(model)
            .arg("--output-format")
            .arg("text");
        cmd
    }

    fn transcript_path(&self, session: &SessionRef) -> PathBuf {
        let projects = self.home_dir().join(".claude").join("projects");
        let computed = projects
            .join(project_slug(&session.cwd))
            .join(format!("{}.jsonl", session.id));
        if computed.exists() {
            return computed;
        }

        // The slug rule is verified for `/` and `.` but not every character,
        // so fall back to finding the session file wherever it landed.
        let wanted = format!("{}.jsonl", session.id);
        if let Ok(entries) = std::fs::read_dir(&projects) {
            for entry in entries.flatten() {
                let candidate = entry.path().join(&wanted);
                if candidate.exists() {
                    return candidate;
                }
            }
        }
        computed
    }

    fn structural_context(&self, jsonl: &str, last_n: usize) -> StructuralContext {
        structural_context(jsonl, last_n)
    }

    fn register_turn_signal(&self, session: &SessionRef, socket: &Path) -> TurnSignalSetup {
        TurnSignalSetup {
            env: vec![
                (super::SOCKET_ENV.to_string(), socket.display().to_string()),
                (super::SESSION_ENV.to_string(), session.id.to_string()),
            ],
            instructions: "register a Stop hook running `zirv ctx hook stop` in \
                           ~/.claude/settings.json so turn boundaries reach the supervisor"
                .to_string(),
        }
    }
```

And the extraction function:

```rust
const FILE_KEYS: &[&str] = &["file_path", "notebook_path", "path"];
const ERROR_SNIPPET: usize = 200;

pub fn structural_context(jsonl: &str, last_n: usize) -> StructuralContext {
    let mut out = StructuralContext::default();

    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if row.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let message = row.get("message").cloned().unwrap_or(Value::Null);

        match row.get("type").and_then(Value::as_str) {
            Some("user") => {
                if row.get("isMeta").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let content = message.get("content");
                if let Some(text) = content.and_then(Value::as_str) {
                    out.user_messages.push(text.to_string());
                    continue;
                }
                let Some(blocks) = content.and_then(Value::as_array) else {
                    continue;
                };
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                out.user_messages.push(text.to_string());
                            }
                        }
                        Some("tool_result")
                            if block.get("is_error").and_then(Value::as_bool) == Some(true) =>
                        {
                            let detail = block
                                .get("content")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| block.get("content").map(Value::to_string).unwrap_or_default());
                            out.tool_errors.push(detail.chars().take(ERROR_SNIPPET).collect());
                        }
                        _ => {}
                    }
                }
            }
            Some("assistant") => {
                let text = text_of(&message);
                if !text.trim().is_empty() {
                    out.assistant_texts.push(text);
                }
                let Some(blocks) = message.get("content").and_then(Value::as_array) else {
                    continue;
                };
                for block in blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                {
                    let Some(input) = block.get("input") else { continue };
                    for key in FILE_KEYS {
                        if let Some(path) = input.get(*key).and_then(Value::as_str)
                            && !out.files_touched.iter().any(|p| p == path)
                        {
                            out.files_touched.push(path.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }

    keep_last(&mut out.user_messages, last_n);
    keep_last(&mut out.assistant_texts, last_n);
    keep_last(&mut out.tool_errors, last_n);
    out
}

fn keep_last(items: &mut Vec<String>, last_n: usize) {
    if items.len() > last_n {
        items.drain(..items.len() - last_n);
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test ctx::adapters::claude 2>&1 | tail -30`
Expected: PASS, 21 tests.

- [ ] **Step 5: Check lints**

Run: `cargo clippy --all-targets -- -D warnings 2>&1 | tail -20`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add src/commands/ctx/adapters/claude.rs
git commit -m "feat(ctx): claude adapter commands, transcript paths and structural context"
```

---

### Task A9: Verify the real codex CLI, then write the codex adapter shell

**Files:**
- Create: `docs/superpowers/notes/2026-07-31-codex-cli-facts.md`
- Modify: `src/commands/ctx/adapters/codex.rs`
- Create (only if codex is installed): `tests/fixtures/codex-real-session.jsonl`, `tests/fixtures/codex-real-session.expected.json`

**Interfaces:**
- Consumes: `AgentAdapter` (A6).
- Produces: `pub struct CodexAdapter { program: String, bin_args: Vec<String>, home: Option<PathBuf> }` with `pub fn new(bin: Option<&str>) -> Self` and `pub fn with_home(self, home: PathBuf) -> Self`, mirroring `ClaudeAdapter`'s multi-word bin split from Task A8; verified command shapes and session-file layout recorded in the notes file; `ready()` still returning `Err` until Task A10.

**Context the implementer needs:** `codex` was **not installed** when this plan was written (`which codex` printed nothing, `~/.codex` did not exist). The spec is explicit that the codex adapter is written from real behavior, not from the spec's own table. Do not write a parser from assumptions.

- [ ] **Step 1: Establish whether codex is available**

Run: `command -v codex || echo MISSING`

If it prints `MISSING`, try, in order, and stop at the first that succeeds:

```bash
brew install codex
npm install -g @openai/codex
```

Run: `codex --version`
Expected: a version string.

If **neither** install works (no network, no npm, not published under that name), skip to Step 7 and record the adapter as BLOCKED. Do not guess at file formats.

- [ ] **Step 2: Capture the CLI surface**

Run each and save the output into the notes file created in Step 6:

```bash
codex --help
codex exec --help
codex --help 2>&1 | grep -iE 'session|resume|json|model|notify|config'
```

What you are looking for, precisely:
- the exact headless invocation (the spec's guess is `codex exec <prompt> ...`)
- whether a caller-supplied session id is accepted, and under which flag
- whether there is a machine-readable output format flag
- the model selection flag and the cheap-model alias to use for distillation
- the interactive quit command (`/quit`, `/exit`, or something else)

- [ ] **Step 3: Produce a real session and locate its file**

Run:

```bash
cd /tmp && mkdir -p codex-probe && cd codex-probe
codex exec "print the word ping and stop" || true
find ~/.codex -type f -newermt '-10 minutes' | head -20
```

Expected: at least one rollout file path. Record the **full path template** (is the cwd in the path? a date directory? a uuid file name?).

- [ ] **Step 4: Record the event shapes**

Run against the file found in Step 3 (`<ROLLOUT>`):

```bash
python3 -c "
import json,sys,collections
rows=[json.loads(l) for l in open('<ROLLOUT>') if l.strip()]
print('types:', collections.Counter(r.get('type') for r in rows))
keys=collections.defaultdict(set)
for r in rows: keys[r.get('type')].update(r.keys())
for t,k in keys.items(): print(t, sorted(k))
print(json.dumps(rows[:3], indent=1)[:2000])
"
```

Record: which event marks a turn boundary, which carries assistant text, which carries tool calls and tool results (and how an error result is flagged), and which field(s) carry token usage.

- [ ] **Step 5: Verify the notify contract**

Run:

```bash
cat ~/.codex/config.toml 2>/dev/null || echo "no config.toml"
codex --help 2>&1 | grep -A5 -i notify
```

Record precisely: whether the notify program receives its payload as **argv[1]** or on **stdin**, and **every field name in the payload**, especially the one holding the rollout or session file path. Task A16's `notify_payload_to_hook` maps that field onto `HookPayload::transcript_path`; if the real name is not in `NOTIFY_TRANSCRIPT_KEYS`, every codex turn signal is dropped, so this is the one fact that must not be guessed.

To capture a real payload rather than a documented one, point notify at a recorder and run a turn:

```bash
printf '#!/bin/sh\nprintf "argv1=%%s\\n" "$1" >> /tmp/codex-notify.log\ncat >> /tmp/codex-notify.log\n' \
  > /tmp/notify-probe.sh && chmod +x /tmp/notify-probe.sh
printf 'notify = ["/tmp/notify-probe.sh"]\n' >> ~/.codex/config.toml
cd /tmp/codex-probe && codex exec "print pong and stop" || true
cat /tmp/codex-notify.log
```

Expected: the log shows whether the payload arrived in `argv1=` or on stdin, and its exact JSON. Copy that JSON verbatim into the notes file: Task A10 pastes it into the `CODEX_NOTIFY_SAMPLE` constant.

- [ ] **Step 6: Write the notes file**

Create `docs/superpowers/notes/2026-07-31-codex-cli-facts.md` with these exact headings, filled from Steps 1 to 5, each line marked `verified:` or `BLOCKED:`:

```markdown
# codex CLI facts (verified 2026-07-31)

codex version:
Install method:

## Headless invocation
## Session id handling
## Session file path template
## Event types and shapes
### Turn boundary
### Assistant text
### Tool call
### Tool result / error flag
### Token usage
## notify contract (argv vs stdin, payload fields)
## Interactive quit command
## Cheap model alias for distillation
## Capabilities conclusion (marker_signal / token_usage / turn_signal)
```

- [ ] **Step 7: Write the failing adapter-shell test**

Bottom of `src/commands/ctx/adapters/codex.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::adapters::{AgentAdapter, select};

    #[test]
    fn codex_detects_its_own_binary() {
        let adapter = CodexAdapter::new(None);
        assert!(adapter.detect(&["/usr/local/bin/codex".to_string()]));
        assert!(!adapter.detect(&["/usr/local/bin/claude".to_string()]));
    }

    #[test]
    fn codex_has_no_marker_signal() {
        let caps = CodexAdapter::new(None).capabilities();
        assert!(!caps.marker_signal, "the spec gives codex no marker signal");
    }

    #[test]
    fn selecting_codex_before_it_is_verified_fails_loudly() {
        // Replaced by a success assertion in Task A10 once the parser exists.
        let err = select(Some("codex"), &[], None).expect_err("unverified adapter");
        assert!(err.to_string().contains("codex"), "got {err}");
    }

    #[test]
    fn detecting_codex_argv_does_not_silently_fall_back_to_claude() {
        let cmd = vec!["codex".to_string(), "exec".to_string(), "do it".to_string()];
        let err = select(None, &cmd, None).expect_err("must not misroute to claude");
        assert!(err.to_string().contains("codex"), "got {err}");
    }
}
```

- [ ] **Step 8: Run tests to verify they fail**

Run: `cargo test ctx::adapters::codex 2>&1 | tail -20`
Expected: FAIL. `detecting_codex_argv_does_not_silently_fall_back_to_claude` fails because `select` currently tries claude last without checking a detected-but-unready adapter, and the `CodexAdapter` shell has no `home` field yet.

- [ ] **Step 9: Write the codex shell using only verified facts**

In `src/commands/ctx/adapters/codex.rs`, give `CodexAdapter` the same shape as `ClaudeAdapter` (`program`, `bin_args`, `home`, `new`, `with_home`, `base`, `home_dir`), including the multi-word bin split, so `ZIRV_CTX_AGENT_BIN` behaves the same for both adapters. Fill `headless_cmd`, `interactive_cmd`, `distiller_cmd`, `transcript_path`, `compact_command`, `quit_sequence` and `capabilities` from the notes file. Where a fact is `BLOCKED`, leave the method returning the neutral value it already has and keep `ready()` returning `Err`. Keep `parse_events` and `structural_context` returning empty for now: `ready()` prevents them being reached.

If Step 1 ended in `MISSING`, the whole of `headless_cmd` and friends stay as written in A6 and the only change in this task is the notes file plus the tests above.

- [ ] **Step 10: Run tests to verify they pass**

Run: `cargo test ctx::adapters 2>&1 | tail -20`
Expected: PASS, including the four codex tests. `select` already calls `ready()` on a detected adapter (Task A6), so the fourth test passes once `detect` is right.

- [ ] **Step 11: Commit**

```bash
git add docs/superpowers/notes/2026-07-31-codex-cli-facts.md src/commands/ctx/adapters/codex.rs
git commit -m "feat(ctx): codex adapter shell from verified CLI behavior"
```

---

### Task A10: Codex transcript parsing

**Files:**
- Modify: `src/commands/ctx/adapters/codex.rs`
- Modify: `src/commands/ctx/hook.rs` (`NOTIFY_TRANSCRIPT_KEYS` and `CODEX_NOTIFY_SAMPLE`)

**Interfaces:**
- Consumes: the notes file from A9; `NormalizedEvent`, `input_hash`, `StructuralContext` (A4); `notify_payload_to_hook` (A16).
- Produces: `pub fn parse_events(jsonl: &str) -> Vec<NormalizedEvent>` and `pub fn structural_context(jsonl: &str, last_n: usize) -> StructuralContext` for codex, `ready()` returning `Ok(())`, and a `NOTIFY_TRANSCRIPT_KEYS` list carrying codex's real field name.

**Gate:** If `docs/superpowers/notes/2026-07-31-codex-cli-facts.md` records `BLOCKED` for the session file path or the event shapes, **skip this task**. Leave `ready()` returning `Err`, leave `NOTIFY_TRANSCRIPT_KEYS` and `CODEX_NOTIFY_SAMPLE` at their Task A16 placeholder values with the `PLACEHOLDER PAYLOAD` comment intact, open a follow-up issue titled "codex adapter: parse rollout events and map notify payload once the CLI is installable", and move to Task A11. Shipping a speculative parser is worse than shipping an honest error, because a wrong parser silently scores every codex session as healthy and a wrong notify field silently drops every codex turn signal. With `ready()` returning `Err` nothing can select the codex adapter, so the placeholders stay unreachable rather than becoming wrong behavior.

- [ ] **Step 1: Record a scrubbed codex fixture**

Adapt `scripts/record-claude-fixture.py` into `scripts/record-codex-fixture.py`: same scrub rules and the same expectations keys, but reading the codex rollout path from argv and detecting codex's own event types. Run it against the `<ROLLOUT>` file from A9 Step 3 and commit `tests/fixtures/codex-real-session.jsonl` plus `tests/fixtures/codex-real-session.expected.json`.

Run: `grep -c 'jonathansolskov\|/Users/\|sk-' tests/fixtures/codex-real-session.jsonl`
Expected: `0`.

- [ ] **Step 2: Write the failing tests**

Add to `mod tests` in `src/commands/ctx/adapters/codex.rs`, replacing the `selecting_codex_before_it_is_verified_fails_loudly` test from A9 (it asserted the pre-parser state and is now wrong):

```rust
    use crate::commands::ctx::event::NormalizedEvent;

    fn fixture_path(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    #[test]
    fn codex_is_selectable_once_the_parser_exists() {
        let adapter = select(Some("codex"), &[], None).expect("codex is verified now");
        assert_eq!(adapter.name(), "codex");
    }

    #[test]
    fn real_codex_fixture_matches_recorded_expectations() {
        let jsonl = std::fs::read_to_string(fixture_path("codex-real-session.jsonl"))
            .expect("fixture");
        let expected: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(fixture_path("codex-real-session.expected.json"))
                .expect("expectations"),
        )
        .expect("valid json");

        let events = parse_events(&jsonl);
        let count = |pred: &dyn Fn(&NormalizedEvent) -> bool| {
            events.iter().filter(|e| pred(e)).count() as u64
        };
        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::TurnStart)),
            expected["turn_start"].as_u64().expect("turn_start")
        );
        assert_eq!(
            count(&|e| matches!(e, NormalizedEvent::ToolCall { .. })),
            expected["tool_call"].as_u64().expect("tool_call")
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, NormalizedEvent::AssistantFinal { input_tokens, .. } if *input_tokens > 0)),
            "codex usage fields must reach the token gate"
        );
    }

    #[test]
    fn codex_parser_ignores_malformed_lines() {
        assert_eq!(parse_events("not json\n\n"), Vec::new());
    }
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test ctx::adapters::codex 2>&1 | tail -20`
Expected: FAIL. `real_codex_fixture_matches_recorded_expectations` gets `0` events from the stub parser, and `codex_is_selectable_once_the_parser_exists` fails on `ready()`.

- [ ] **Step 4: Write the parser from the recorded shapes**

Implement `parse_events` and `structural_context` in `src/commands/ctx/adapters/codex.rs` following the same structure as `adapters/claude.rs` (line-by-line `serde_json::Value` walk, skip unparseable lines, `input_hash` over the serialized tool input), mapping codex's real event types onto `NormalizedEvent` exactly as recorded in the notes file. Set `capabilities()` to `Capabilities { marker_signal: false, token_usage: <as recorded>, turn_signal: <as recorded> }` and flip `ready()` to `Ok(())`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test ctx::adapters 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 6: Replace the placeholder notify payload with the verified one**

Task A16 left two things pinned to a placeholder. Fix both from the notes file:

1. In `src/commands/ctx/hook.rs`, set `NOTIFY_TRANSCRIPT_KEYS` to codex's real field name first, keeping `"transcript_path"` last so a claude-registered hook still works. Delete any speculative names that the recorded payload disproved.
2. Replace the `CODEX_NOTIFY_SAMPLE` constant in the test module with the payload copied verbatim from `docs/superpowers/notes/2026-07-31-codex-cli-facts.md`, and delete the `PLACEHOLDER PAYLOAD` comment above it.

Then extend the mapping test with the real session-file assertion:

```rust
    #[test]
    fn the_verified_notify_payload_maps_to_a_readable_transcript() {
        let mapped = notify_payload_to_hook(CODEX_NOTIFY_SAMPLE).expect("verified shape maps");
        assert!(
            !mapped.transcript_path.is_empty(),
            "the recorded payload must carry a rollout path"
        );
        assert!(
            mapped.transcript_path.ends_with(".jsonl"),
            "codex rollouts are JSONL: {}",
            mapped.transcript_path
        );
    }
```

Run: `cargo test ctx::hook -- --test-threads=1 2>&1 | tail -20`
Expected: PASS. A failure here means the payload in the notes file and `NOTIFY_TRANSCRIPT_KEYS` disagree, which is exactly the silent-drop bug this gate exists to catch.

- [ ] **Step 7: Verify the notify hook end to end against the real CLI**

Run:

```bash
printf 'notify = ["zirv", "ctx", "hook", "notify"]\n' >> ~/.codex/config.toml
cd /tmp/codex-probe && codex exec "print pong and stop"
ZIRV_CTX_STATE_DIR="${ZIRV_CTX_STATE_DIR:-$HOME/.local/state/zirv/ctx}" zirv ctx status --decisions 5
```

Expected: a decision-log line with `"verb":"hook"` and an action of `advise` or `forward`, and **not** `notify-unmapped`. If it says `notify-unmapped`, the field name is still wrong.

- [ ] **Step 8: Commit**

```bash
git add scripts/record-codex-fixture.py tests/fixtures/codex-real-session.jsonl tests/fixtures/codex-real-session.expected.json src/commands/ctx/adapters/codex.rs src/commands/ctx/hook.rs
git commit -m "feat(ctx): parse codex rollout transcripts and map its notify payload"
```

---

### Task A11: Rot engine signals

**Files:**
- Modify: `src/commands/ctx/rot.rs`

**Interfaces:**
- Consumes: `NormalizedEvent`, `Capabilities`, `input_hash` (A4); `ScoreConfig` (A2).
- Produces:
  - `pub struct Signals { pub turns: usize, pub tool_failure_rate: f64, pub repetition_hits: usize, pub max_repeat: usize, pub marker_miss_rate: Option<f64> }` (serializes with `serde::Serialize`)
  - `pub fn signals(events: &[NormalizedEvent], caps: Capabilities, cfg: &ScoreConfig) -> Signals`
  - `pub fn has_marker(text: &str, marker: &str) -> bool`
  - `pub fn turn_final_texts(events: &[NormalizedEvent]) -> Vec<String>`
  - `pub fn context_tokens(events: &[NormalizedEvent]) -> u64`

The marker signal is inactive (`None`) when any of these hold: the adapter lacks the capability, the configured marker is empty, the marker never appeared anywhere in the transcript, or the session has fewer than `min_turns` turn-finals. Those last two port the canary's "never loaded" and "young" cases.

- [ ] **Step 1: Write the failing tests**

Bottom of `src/commands/ctx/rot.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::config::ScoreConfig;
    use crate::commands::ctx::event::{Capabilities, NormalizedEvent, input_hash};

    fn full_caps() -> Capabilities {
        Capabilities {
            marker_signal: true,
            token_usage: true,
            turn_signal: true,
        }
    }

    fn assistant(text: &str, tokens: u64) -> NormalizedEvent {
        NormalizedEvent::AssistantFinal {
            text: text.to_string(),
            input_tokens: tokens,
        }
    }

    fn tool(name: &str, input: &str) -> NormalizedEvent {
        NormalizedEvent::ToolCall {
            name: name.to_string(),
            input_hash: input_hash(input),
        }
    }

    /// One turn: prompt, a mid-turn tool-only assistant message, a tool call,
    /// its result, then the turn-final text. Mirrors the canary's synthetic
    /// turn builder. `tool_input` decides whether the turn feeds the repetition
    /// signal, so every test states that choice explicitly.
    fn turn_with(
        tool_input: &str,
        mid_text: &str,
        final_text: &str,
        is_error: bool,
        tokens: u64,
    ) -> Vec<NormalizedEvent> {
        vec![
            NormalizedEvent::TurnStart,
            assistant(mid_text, tokens),
            tool("Bash", tool_input),
            NormalizedEvent::ToolResult { is_error },
            assistant(final_text, tokens),
        ]
    }

    /// Distinct tool input per turn: the repetition signal stays at zero, so
    /// these fixtures isolate the marker and tool-failure signals.
    fn turns(count: usize, mid: &str, fin: &str, is_error: bool, tokens: u64) -> Vec<NormalizedEvent> {
        (0..count)
            .flat_map(|i| {
                turn_with(&format!("{{\"command\":\"ls {i}\"}}"), mid, fin, is_error, tokens)
            })
            .collect()
    }

    /// Identical tool input every turn: this is what a repetition loop looks
    /// like, so the repetition signal fires.
    fn looping_turns(
        count: usize,
        mid: &str,
        fin: &str,
        is_error: bool,
        tokens: u64,
    ) -> Vec<NormalizedEvent> {
        (0..count)
            .flat_map(|_| turn_with("{\"command\":\"ls\"}", mid, fin, is_error, tokens))
            .collect()
    }

    #[test]
    fn marker_detection_tolerates_leading_markdown() {
        assert!(has_marker("[zirv] done", "[zirv]"));
        assert!(has_marker("  > **[zirv]** done", "[zirv]"));
        assert!(has_marker("- [zirv] done", "[zirv]"));
        assert!(!has_marker("done [zirv]", "[zirv]"));
        assert!(!has_marker("done", "[zirv]"));
    }

    #[test]
    fn turn_finals_take_the_last_text_per_turn_and_skip_textless_turns() {
        let events = vec![
            NormalizedEvent::TurnStart,
            assistant("mid", 1),
            assistant("final one", 1),
            NormalizedEvent::TurnStart,
            assistant("", 1),
            NormalizedEvent::TurnStart,
            assistant("final two", 1),
        ];
        assert_eq!(turn_final_texts(&events), vec!["final one", "final two"]);
    }

    #[test]
    fn context_tokens_come_from_the_most_recent_assistant_event() {
        let events = vec![assistant("a", 10), assistant("", 55_000), assistant("b", 120_000)];
        assert_eq!(context_tokens(&events), 120_000);
        assert_eq!(context_tokens(&[]), 0);
    }

    #[test]
    fn tool_failure_rate_is_measured_over_the_trailing_window() {
        let cfg = ScoreConfig { window: 2, ..ScoreConfig::default() };
        let mut events = turns(3, "", "[zirv] ok", false, 120_000);
        events.extend(turn_with("{\"command\":\"a\"}", "", "[zirv] ok", true, 120_000));
        events.extend(turn_with("{\"command\":\"b\"}", "", "[zirv] ok", true, 120_000));
        let s = signals(&events, full_caps(), &cfg);
        assert_eq!(s.tool_failure_rate, 1.0, "only the last two turns count");
    }

    #[test]
    fn no_tool_results_means_no_failures() {
        let events = vec![NormalizedEvent::TurnStart, assistant("[zirv] hi", 120_000)];
        let s = signals(&events, full_caps(), &ScoreConfig::default());
        assert_eq!(s.tool_failure_rate, 0.0);
    }

    #[test]
    fn identical_tool_calls_are_counted_and_distinct_ones_are_not() {
        let cfg = ScoreConfig::default();
        let mut repeated = vec![NormalizedEvent::TurnStart];
        for _ in 0..4 {
            repeated.push(tool("Bash", "{\"command\":\"ls\"}"));
        }
        let s = signals(&repeated, full_caps(), &cfg);
        assert_eq!(s.max_repeat, 4);
        assert_eq!(s.repetition_hits, 1);

        let mut distinct = vec![NormalizedEvent::TurnStart];
        for i in 0..4 {
            distinct.push(tool("Bash", &format!("{{\"command\":\"ls {i}\"}}")));
        }
        let s = signals(&distinct, full_caps(), &cfg);
        assert_eq!(s.max_repeat, 1);
        assert_eq!(s.repetition_hits, 0);
    }

    #[test]
    fn same_input_different_tool_is_not_a_repetition() {
        let events = vec![
            NormalizedEvent::TurnStart,
            tool("Read", "{\"file_path\":\"/a\"}"),
            tool("Write", "{\"file_path\":\"/a\"}"),
            tool("Edit", "{\"file_path\":\"/a\"}"),
        ];
        let s = signals(&events, full_caps(), &ScoreConfig::default());
        assert_eq!(s.max_repeat, 1);
    }

    #[test]
    fn marker_miss_rate_is_measured_over_the_last_window_of_turn_finals() {
        let cfg = ScoreConfig::default();
        let mut events = turns(2, "", "[zirv] ok", false, 120_000);
        events.extend(turns(10, "", "sloppy", false, 120_000));
        let s = signals(&events, full_caps(), &cfg);
        assert_eq!(s.turns, 12);
        assert_eq!(s.marker_miss_rate, Some(1.0), "last 10 finals all miss");
    }

    #[test]
    fn half_missing_markers_is_a_half_rate() {
        let cfg = ScoreConfig::default();
        let mut events = turns(6, "", "[zirv] ok", false, 120_000);
        events.extend(turns(4, "", "sloppy", false, 120_000));
        let s = signals(&events, full_caps(), &cfg);
        assert_eq!(s.marker_miss_rate, Some(0.4));
    }

    #[test]
    fn mid_turn_notes_never_count_against_the_marker() {
        let cfg = ScoreConfig::default();
        let events = turns(12, "no prefix here", "[zirv] ok", false, 120_000);
        let s = signals(&events, full_caps(), &cfg);
        assert_eq!(s.marker_miss_rate, Some(0.0));
    }

    #[test]
    fn marker_signal_is_inactive_for_immature_sessions() {
        let cfg = ScoreConfig::default();
        let mut events = turns(1, "", "[zirv] ok", false, 120_000);
        events.extend(turns(7, "", "sloppy", false, 120_000));
        let s = signals(&events, full_caps(), &cfg);
        assert_eq!(s.turns, 8);
        assert_eq!(s.marker_miss_rate, None, "8 turns is below min_turns");
    }

    #[test]
    fn marker_signal_is_inactive_when_the_marker_never_appears() {
        let cfg = ScoreConfig::default();
        let events = turns(12, "", "no marker anywhere", false, 120_000);
        let s = signals(&events, full_caps(), &cfg);
        assert_eq!(s.marker_miss_rate, None, "the hook is not installed");
    }

    #[test]
    fn marker_signal_is_inactive_without_the_capability() {
        let cfg = ScoreConfig::default();
        let mut events = turns(2, "", "[zirv] ok", false, 120_000);
        events.extend(turns(10, "", "sloppy", false, 120_000));
        let caps = Capabilities { marker_signal: false, token_usage: true, turn_signal: true };
        assert_eq!(signals(&events, caps, &cfg).marker_miss_rate, None);
    }

    #[test]
    fn marker_signal_is_inactive_when_configured_empty() {
        let cfg = ScoreConfig { marker: String::new(), ..ScoreConfig::default() };
        let events = turns(12, "", "anything", false, 120_000);
        assert_eq!(signals(&events, full_caps(), &cfg).marker_miss_rate, None);
    }

    #[test]
    fn signals_are_reported_even_below_the_token_floor() {
        let cfg = ScoreConfig::default();
        let mut events = turns(2, "", "[zirv] ok", false, 1_000);
        events.extend(turns(10, "", "sloppy", true, 1_000));
        let s = signals(&events, full_caps(), &cfg);
        assert_eq!(s.marker_miss_rate, Some(1.0));
        assert_eq!(s.tool_failure_rate, 1.0);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ctx::rot 2>&1 | tail -30`
Expected: FAIL to compile, `cannot find function signals`.

- [ ] **Step 3: Write minimal implementation**

Replace the placeholder in `src/commands/ctx/rot.rs`:

```rust
use hashbrown::HashMap;
use serde::Serialize;

use super::config::ScoreConfig;
use super::event::{Capabilities, NormalizedEvent};

/// Leading characters a model tends to put before a reply prefix. Ported from
/// the shell canary's `^[ \t>*_`#~-]*` allowance.
const MARKER_LEAD: [char; 10] = [' ', '\t', '\n', '\r', '>', '*', '_', '`', '#', '~'];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Signals {
    pub turns: usize,
    pub tool_failure_rate: f64,
    pub repetition_hits: usize,
    pub max_repeat: usize,
    /// `None` means the signal is unavailable, not that it scored zero.
    pub marker_miss_rate: Option<f64>,
}

pub fn has_marker(text: &str, marker: &str) -> bool {
    if marker.is_empty() {
        return false;
    }
    text.trim_start_matches(|c| MARKER_LEAD.contains(&c) || c == '-')
        .starts_with(marker)
}

/// One entry per turn that produced any assistant text, holding that turn's
/// last text message. Mid-turn notes are deliberately discarded: they are
/// missing the marker even in healthy sessions.
pub fn turn_final_texts(events: &[NormalizedEvent]) -> Vec<String> {
    let mut finals = Vec::new();
    let mut current: Option<String> = None;
    let mut in_turn = false;

    for event in events {
        match event {
            NormalizedEvent::TurnStart => {
                if in_turn && let Some(text) = current.take() {
                    finals.push(text);
                }
                in_turn = true;
                current = None;
            }
            NormalizedEvent::AssistantFinal { text, .. } if !text.trim().is_empty() => {
                current = Some(text.clone());
            }
            _ => {}
        }
    }
    if let Some(text) = current {
        finals.push(text);
    }
    finals
}

pub fn context_tokens(events: &[NormalizedEvent]) -> u64 {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            NormalizedEvent::AssistantFinal { input_tokens, .. } => Some(*input_tokens),
            _ => None,
        })
        .unwrap_or(0)
}

fn last_window<T>(items: &[T], window: usize) -> &[T] {
    if window == 0 || items.len() <= window {
        return items;
    }
    &items[items.len() - window..]
}

/// Events belonging to the last `window` turns, so tool signals share the
/// marker signal's horizon.
fn events_in_last_turns(events: &[NormalizedEvent], window: usize) -> &[NormalizedEvent] {
    let starts: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, NormalizedEvent::TurnStart))
        .map(|(i, _)| i)
        .collect();

    if window == 0 || starts.len() <= window {
        return events;
    }
    &events[starts[starts.len() - window]..]
}

pub fn signals(events: &[NormalizedEvent], caps: Capabilities, cfg: &ScoreConfig) -> Signals {
    let finals = turn_final_texts(events);
    let turns = finals.len();

    let marker_ever = finals.iter().any(|t| has_marker(t, &cfg.marker));
    let marker_active =
        caps.marker_signal && !cfg.marker.is_empty() && marker_ever && turns >= cfg.min_turns;

    let marker_miss_rate = if marker_active {
        let recent = last_window(&finals, cfg.window);
        let misses = recent.iter().filter(|t| !has_marker(t, &cfg.marker)).count();
        Some(misses as f64 / recent.len() as f64)
    } else {
        None
    };

    let tail = events_in_last_turns(events, cfg.window);

    let results: Vec<bool> = tail
        .iter()
        .filter_map(|e| match e {
            NormalizedEvent::ToolResult { is_error } => Some(*is_error),
            _ => None,
        })
        .collect();
    let tool_failure_rate = if results.is_empty() {
        0.0
    } else {
        results.iter().filter(|e| **e).count() as f64 / results.len() as f64
    };

    let mut counts: HashMap<(&str, u64), usize> = HashMap::new();
    for event in tail {
        if let NormalizedEvent::ToolCall { name, input_hash } = event {
            *counts.entry((name.as_str(), *input_hash)).or_insert(0) += 1;
        }
    }
    let max_repeat = counts.values().copied().max().unwrap_or(0);
    let repetition_hits = counts
        .values()
        .filter(|count| **count >= cfg.repetition_threshold)
        .count();

    Signals {
        turns,
        tool_failure_rate,
        repetition_hits,
        max_repeat,
        marker_miss_rate,
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test ctx::rot 2>&1 | tail -30`
Expected: PASS, 16 tests.

- [ ] **Step 5: Commit**

```bash
git add src/commands/ctx/rot.rs
git commit -m "feat(ctx): rot engine signal computation"
```

---

### Task A12: Rot engine scoring, verdicts and the ported canary cases

**Files:**
- Modify: `src/commands/ctx/rot.rs`

**Interfaces:**
- Consumes: `Signals`, `signals`, `context_tokens` (A11); `ScoreConfig` (A2).
- Produces:
  - `pub enum Verdict { Healthy, Advise, Compact, Restart }` deriving `Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize` with `#[serde(rename_all = "lowercase")]`, plus `pub fn as_str(&self) -> &'static str`
  - `pub struct Score { pub score: u32, pub verdict: Verdict, pub signals: Signals, pub context_tokens: u64 }` deriving `Debug, Clone, PartialEq, Serialize`
  - `pub fn score_events(events: &[NormalizedEvent], caps: Capabilities, cfg: &ScoreConfig) -> Score`
  - `pub fn repetition_component(max_repeat: usize, threshold: usize) -> f64`
  - `pub fn verdict_for(score: u32, tokens: u64, cfg: &ScoreConfig) -> Verdict`

The scoring formula, in full:

```
score = round(weight_tool_failure * tool_failure_rate
            + weight_repetition   * repetition_component
            + weight_marker       * marker_miss_rate_or_zero)   clamped to 0..=100

repetition_component = clamp((max_repeat + 1 - threshold) / threshold, 0, 1), zero below threshold
```

Weights are **not** redistributed when a signal is unavailable. That is deliberate: with the marker signal off (codex, or a claude session without the prompt hook) the maximum reachable score is 70, so behavioral signals alone can never force a restart. Restarts for those sessions come from the token ceiling rule instead. It also means the noisy marker signal can contribute at most 30 on its own, which is the whole reason the old single-signal canary is being replaced.

- [ ] **Step 1: Write the failing verdict and formula tests**

Add to the `mod tests` in `src/commands/ctx/rot.rs`:

```rust
    #[test]
    fn repetition_component_ramps_from_the_threshold() {
        assert_eq!(repetition_component(0, 3), 0.0);
        assert_eq!(repetition_component(2, 3), 0.0);
        assert!((repetition_component(3, 3) - 1.0 / 3.0).abs() < 1e-9);
        assert!((repetition_component(4, 3) - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(repetition_component(5, 3), 1.0);
        assert_eq!(repetition_component(50, 3), 1.0, "clamped");
        assert_eq!(repetition_component(5, 0), 0.0, "a zero threshold disables the signal");
    }

    #[test]
    fn thresholds_map_scores_to_verdicts_above_the_floor() {
        let cfg = ScoreConfig::default();
        let tokens = 120_000;
        assert_eq!(verdict_for(0, tokens, &cfg), Verdict::Healthy);
        assert_eq!(verdict_for(39, tokens, &cfg), Verdict::Healthy);
        assert_eq!(verdict_for(40, tokens, &cfg), Verdict::Advise);
        assert_eq!(verdict_for(59, tokens, &cfg), Verdict::Advise);
        assert_eq!(verdict_for(60, tokens, &cfg), Verdict::Compact);
        assert_eq!(verdict_for(79, tokens, &cfg), Verdict::Compact);
        assert_eq!(verdict_for(80, tokens, &cfg), Verdict::Restart);
        assert_eq!(verdict_for(100, tokens, &cfg), Verdict::Restart);
    }

    #[test]
    fn below_the_token_floor_the_verdict_is_always_healthy() {
        let cfg = ScoreConfig::default();
        assert_eq!(verdict_for(100, 99_999, &cfg), Verdict::Healthy);
        assert_eq!(verdict_for(0, 0, &cfg), Verdict::Healthy);
        assert_eq!(verdict_for(100, 100_000, &cfg), Verdict::Restart, "floor is inclusive");
    }

    #[test]
    fn at_the_ceiling_the_verdict_is_at_least_compact() {
        let cfg = ScoreConfig::default();
        assert_eq!(verdict_for(0, 160_000, &cfg), Verdict::Compact);
        assert_eq!(verdict_for(45, 200_000, &cfg), Verdict::Compact);
    }

    #[test]
    fn at_the_ceiling_a_compact_level_score_escalates_to_restart() {
        let cfg = ScoreConfig::default();
        assert_eq!(verdict_for(60, 160_000, &cfg), Verdict::Restart);
        assert_eq!(verdict_for(70, 170_000, &cfg), Verdict::Restart);
        assert_eq!(verdict_for(59, 170_000, &cfg), Verdict::Compact);
    }

    #[test]
    fn verdicts_are_ordered_for_escalation_comparisons() {
        assert!(Verdict::Restart > Verdict::Compact);
        assert!(Verdict::Compact > Verdict::Advise);
        assert!(Verdict::Advise > Verdict::Healthy);
    }

    #[test]
    fn verdict_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&Verdict::Restart).expect("serialize"),
            "\"restart\""
        );
        assert_eq!(Verdict::Compact.as_str(), "compact");
    }

    #[test]
    fn a_tool_failure_spike_alone_reaches_advise() {
        let cfg = ScoreConfig::default();
        let events = turns(12, "", "[zirv] ok", true, 120_000);
        let result = score_events(&events, full_caps(), &cfg);
        assert_eq!(result.signals.tool_failure_rate, 1.0);
        assert_eq!(result.score, 40);
        assert_eq!(result.verdict, Verdict::Advise);
    }

    #[test]
    fn tool_failures_plus_repetition_reach_compact() {
        let cfg = ScoreConfig::default();
        // Same tool and input every turn, every result an error, marker intact.
        let events = looping_turns(12, "", "[zirv] ok", true, 120_000);
        let result = score_events(&events, full_caps(), &cfg);
        // 40 (failures) + 30 (repetition maxed) + 0 (marker clean) = 70
        assert_eq!(result.signals.max_repeat, 10, "window bounded");
        assert_eq!(result.signals.marker_miss_rate, Some(0.0));
        assert_eq!(result.score, 70);
        assert_eq!(result.verdict, Verdict::Compact);
    }

    #[test]
    fn all_three_signals_together_reach_restart() {
        let cfg = ScoreConfig::default();
        let mut events = looping_turns(2, "", "[zirv] ok", true, 120_000);
        events.extend(looping_turns(10, "", "sloppy", true, 120_000));
        let result = score_events(&events, full_caps(), &cfg);
        assert_eq!(result.score, 100);
        assert_eq!(result.verdict, Verdict::Restart);
    }

    #[test]
    fn without_the_marker_signal_behavior_alone_caps_at_seventy() {
        let cfg = ScoreConfig::default();
        let caps = Capabilities { marker_signal: false, token_usage: true, turn_signal: true };
        let mut events = looping_turns(2, "", "[zirv] ok", true, 120_000);
        events.extend(looping_turns(10, "", "sloppy", true, 120_000));
        let result = score_events(&events, caps, &cfg);
        assert_eq!(result.score, 70, "weights are not redistributed");
        assert_eq!(result.verdict, Verdict::Compact, "never restart on behavior alone");
    }

    #[test]
    fn without_the_marker_signal_the_ceiling_still_forces_a_restart() {
        let cfg = ScoreConfig::default();
        let caps = Capabilities { marker_signal: false, token_usage: true, turn_signal: true };
        let events = looping_turns(12, "", "sloppy", true, 175_000);
        let result = score_events(&events, caps, &cfg);
        assert_eq!(result.score, 70);
        assert_eq!(result.context_tokens, 175_000);
        assert_eq!(result.verdict, Verdict::Restart);
    }

    #[test]
    fn scoring_is_deterministic() {
        let cfg = ScoreConfig::default();
        let mut events = looping_turns(2, "", "[zirv] ok", true, 165_000);
        events.extend(looping_turns(10, "", "sloppy", true, 165_000));
        let first = score_events(&events, full_caps(), &cfg);
        for _ in 0..20 {
            assert_eq!(score_events(&events, full_caps(), &cfg), first);
        }
    }

    #[test]
    fn an_empty_transcript_is_healthy() {
        let result = score_events(&[], full_caps(), &ScoreConfig::default());
        assert_eq!(result.score, 0);
        assert_eq!(result.verdict, Verdict::Healthy);
        assert_eq!(result.context_tokens, 0);
        assert_eq!(result.signals.turns, 0);
    }

    #[test]
    fn compaction_drops_the_reported_context_size() {
        let cfg = ScoreConfig::default();
        let mut events = turns(12, "", "[zirv] ok", false, 170_000);
        events.push(NormalizedEvent::Compaction);
        events.extend(turn_with("{\"command\":\"post\"}", "", "[zirv] ok", false, 12_000));
        let result = score_events(&events, full_caps(), &cfg);
        assert_eq!(result.context_tokens, 12_000);
        assert_eq!(result.verdict, Verdict::Healthy, "post-compaction sessions are healthy again");
    }

    // The eight cases from ~/.claude/hooks/canary-check.test.sh, ported. The
    // canary's warn tier maps to `advise` and its block tier to `restart`, but
    // the verdicts below follow zirv's own gate rules, which weight the noisy
    // marker signal far lower than the canary did. Case 7 (the stop_hook_active
    // guard) is not a scoring case and is covered in Task A15.
    #[test]
    fn ported_canary_case_1_bimodal_healthy() {
        let events = turns(12, "", "[zirv] ok", false, 120_000);
        let result = score_events(&events, full_caps(), &ScoreConfig::default());
        assert_eq!(result.signals.marker_miss_rate, Some(0.0));
        assert_eq!(result.verdict, Verdict::Healthy);
    }

    #[test]
    fn ported_canary_case_2_young_sloppy_session() {
        let mut events = turns(1, "", "[zirv] ok", false, 120_000);
        events.extend(turns(7, "", "sloppy", false, 120_000));
        let result = score_events(&events, full_caps(), &ScoreConfig::default());
        assert_eq!(result.signals.marker_miss_rate, None);
        assert_eq!(result.score, 0);
        assert_eq!(result.verdict, Verdict::Healthy);
    }

    #[test]
    fn ported_canary_case_3_sustained_misses_below_the_floor() {
        let mut events = turns(2, "", "[zirv] ok", false, 90_000);
        events.extend(turns(10, "", "sloppy", false, 90_000));
        let result = score_events(&events, full_caps(), &ScoreConfig::default());
        assert_eq!(result.signals.marker_miss_rate, Some(1.0), "signal still reported");
        assert_eq!(result.score, 30);
        assert_eq!(result.verdict, Verdict::Healthy, "the floor gate wins");
    }

    #[test]
    fn ported_canary_case_4_sustained_misses_above_the_ceiling() {
        let mut events = turns(2, "", "[zirv] ok", false, 170_000);
        events.extend(turns(10, "", "sloppy", false, 170_000));
        let result = score_events(&events, full_caps(), &ScoreConfig::default());
        assert_eq!(result.score, 30);
        assert_eq!(result.verdict, Verdict::Compact, "marker misses alone never restart");
    }

    #[test]
    fn ported_canary_case_5_egregious_but_low_context_is_never_escalated() {
        let mut events = looping_turns(2, "", "[zirv] ok", true, 40_000);
        events.extend(looping_turns(10, "", "sloppy", true, 40_000));
        let result = score_events(&events, full_caps(), &ScoreConfig::default());
        assert_eq!(result.score, 100, "every signal is firing");
        assert_eq!(result.verdict, Verdict::Healthy, "and it still must not intervene");
    }

    #[test]
    fn ported_canary_case_6_marker_never_loaded() {
        let events = turns(12, "", "no marker at all", false, 170_000);
        let result = score_events(&events, full_caps(), &ScoreConfig::default());
        assert_eq!(result.signals.marker_miss_rate, None);
        assert_eq!(result.score, 0);
        assert_eq!(result.verdict, Verdict::Compact, "the ceiling gate still applies");
    }

    #[test]
    fn ported_canary_case_8_half_missing_stays_below_advise() {
        let mut events = turns(6, "", "[zirv] ok", false, 120_000);
        events.extend(turns(4, "", "sloppy", false, 120_000));
        let result = score_events(&events, full_caps(), &ScoreConfig::default());
        assert_eq!(result.signals.marker_miss_rate, Some(0.4));
        assert_eq!(result.score, 12);
        assert_eq!(result.verdict, Verdict::Healthy);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ctx::rot 2>&1 | tail -30`
Expected: FAIL to compile, `cannot find function score_events` / `cannot find type Verdict`.

- [ ] **Step 3: Write minimal implementation**

Append to `src/commands/ctx/rot.rs`:

```rust
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Healthy,
    Advise,
    Compact,
    Restart,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Healthy => "healthy",
            Verdict::Advise => "advise",
            Verdict::Compact => "compact",
            Verdict::Restart => "restart",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Score {
    pub score: u32,
    pub verdict: Verdict,
    pub signals: Signals,
    pub context_tokens: u64,
}

/// Zero below the threshold, then a linear ramp that saturates at
/// `2 * threshold - 1` identical calls.
pub fn repetition_component(max_repeat: usize, threshold: usize) -> f64 {
    if threshold == 0 || max_repeat < threshold {
        return 0.0;
    }
    (((max_repeat + 1 - threshold) as f64) / threshold as f64).clamp(0.0, 1.0)
}

/// The token gate is a gate, not a vote: below the floor nothing escalates, at
/// or above the ceiling the verdict is at least `compact`, and at the ceiling a
/// compact-level score becomes a restart.
pub fn verdict_for(score: u32, tokens: u64, cfg: &ScoreConfig) -> Verdict {
    if tokens < cfg.token_floor {
        return Verdict::Healthy;
    }

    let base = if score >= cfg.restart_at {
        Verdict::Restart
    } else if score >= cfg.compact_at {
        Verdict::Compact
    } else if score >= cfg.advise_at {
        Verdict::Advise
    } else {
        Verdict::Healthy
    };

    if tokens < cfg.token_ceiling {
        return base;
    }
    if score >= cfg.compact_at {
        return Verdict::Restart;
    }
    base.max(Verdict::Compact)
}

pub fn score_events(events: &[NormalizedEvent], caps: Capabilities, cfg: &ScoreConfig) -> Score {
    let signals = signals(events, caps, cfg);
    let tokens = context_tokens(events);

    let raw = cfg.weight_tool_failure * signals.tool_failure_rate
        + cfg.weight_repetition * repetition_component(signals.max_repeat, cfg.repetition_threshold)
        + cfg.weight_marker * signals.marker_miss_rate.unwrap_or(0.0);
    let score = raw.round().clamp(0.0, 100.0) as u32;

    Score {
        score,
        verdict: verdict_for(score, tokens, cfg),
        signals,
        context_tokens: tokens,
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test ctx::rot 2>&1 | tail -30`
Expected: PASS, 39 tests. If `tool_failures_plus_repetition_reach_compact` reports `max_repeat` other than `10`, check `events_in_last_turns`: with `window = 10` and 12 turns it must start at the 3rd `TurnStart`.

- [ ] **Step 5: Check formatting and lints**

Run: `cargo fmt -- --check 2>&1 | tail -5`
Expected: no output.
Run: `cargo clippy --all-targets -- -D warnings 2>&1 | tail -20`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add src/commands/ctx/rot.rs
git commit -m "feat(ctx): deterministic rot scoring with token gates and canary case parity"
```

---

### Task A13: `zirv ctx score`

**Files:**
- Modify: `src/commands/ctx/score.rs`

**Interfaces:**
- Consumes: `ScoreArgs` (A1), `CtxConfig::load`, `env_from_process` (A2), `adapters::select` (A6), `rot::score_events` (A12).
- Produces: `pub fn run<W: Write>(args: &ScoreArgs, w: &mut W) -> CtxResult<i32>` printing one line of JSON with keys `score`, `verdict`, `signals`, `context_tokens`, and `pub fn score_transcript(transcript: &Path, agent: Option<&str>, repo: &Path, env: EnvLookup<'_>) -> CtxResult<Score>` reused by `hook`, `exec`, `loop` and `wrap`.

- [ ] **Step 1: Write the failing test**

Bottom of `src/commands/ctx/score.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn write_transcript(dir: &std::path::Path, turns: usize, marker: bool, tokens: u64) -> PathBuf {
        let mut text = String::new();
        for i in 0..turns {
            text.push_str("{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n");
            text.push_str(&format!(
                "{{\"type\":\"user\",\"message\":{{\"content\":[{{\"type\":\"tool_result\",\"content\":\"r\",\"is_error\":true}}]}}}}\n"
            ));
            let text_block = if marker || i < 2 { "[zirv] done" } else { "done" };
            text.push_str(&format!(
                "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{text_block}\"}}],\"usage\":{{\"input_tokens\":{tokens}}}}}}}\n"
            ));
        }
        let path = dir.join("t.jsonl");
        std::fs::write(&path, text).expect("write transcript");
        path
    }

    #[test]
    fn prints_one_line_of_json_with_the_documented_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = write_transcript(dir.path(), 12, false, 170_000);
        let args = ScoreArgs { transcript, agent: None };

        let mut out = Vec::new();
        let code = run_with(&args, &mut out, dir.path(), &|_| None).expect("score runs");
        assert_eq!(code, 0);

        let text = String::from_utf8(out).expect("utf8");
        assert_eq!(text.lines().count(), 1, "exactly one JSON line");
        let parsed: serde_json::Value = serde_json::from_str(text.trim()).expect("valid json");
        assert!(parsed["score"].is_u64());
        assert_eq!(parsed["verdict"], "restart");
        assert_eq!(parsed["context_tokens"], 170_000);
        assert_eq!(parsed["signals"]["turns"], 12);
        assert_eq!(parsed["signals"]["tool_failure_rate"], 1.0);
        assert_eq!(parsed["signals"]["marker_miss_rate"], 1.0);
    }

    #[test]
    fn an_inactive_marker_signal_serializes_as_null() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = write_transcript(dir.path(), 12, true, 120_000);
        let args = ScoreArgs { transcript, agent: None };

        let mut out = Vec::new();
        run_with(&args, &mut out, dir.path(), &|_| None).expect("score runs");
        let parsed: serde_json::Value =
            serde_json::from_str(String::from_utf8(out).expect("utf8").trim()).expect("json");
        assert_eq!(parsed["signals"]["marker_miss_rate"], 0.0);
    }

    #[test]
    fn repo_config_changes_the_verdict() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            dir.path().join(".zirv/ctx.toml"),
            "[score]\ntoken_floor = 500000\ntoken_ceiling = 900000\n",
        )
        .expect("write");
        let transcript = write_transcript(dir.path(), 12, false, 170_000);
        let args = ScoreArgs { transcript, agent: None };

        let mut out = Vec::new();
        run_with(&args, &mut out, dir.path(), &|_| None).expect("score runs");
        let parsed: serde_json::Value =
            serde_json::from_str(String::from_utf8(out).expect("utf8").trim()).expect("json");
        assert_eq!(parsed["verdict"], "healthy", "the raised floor gates everything");
    }

    #[test]
    fn a_missing_transcript_is_an_error_not_a_healthy_verdict() {
        let dir = tempfile::tempdir().expect("tempdir");
        let args = ScoreArgs {
            transcript: dir.path().join("nope.jsonl"),
            agent: None,
        };
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, dir.path(), &|_| None).expect_err("must fail");
        assert!(err.to_string().contains("nope.jsonl"), "got {err}");
    }

    #[test]
    fn env_overrides_reach_the_engine() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = write_transcript(dir.path(), 12, false, 170_000);
        let args = ScoreArgs { transcript, agent: None };
        let env: HashMap<String, String> =
            [("ZIRV_CTX_MARKER".to_string(), "[other]".to_string())].into();

        let mut out = Vec::new();
        run_with(&args, &mut out, dir.path(), &|k| env.get(k).cloned()).expect("score runs");
        let parsed: serde_json::Value =
            serde_json::from_str(String::from_utf8(out).expect("utf8").trim()).expect("json");
        assert!(
            parsed["signals"]["marker_miss_rate"].is_null(),
            "a marker that never appears deactivates the signal"
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ctx::score 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find function run_with`.

- [ ] **Step 3: Write minimal implementation**

Replace the stub `run` in `src/commands/ctx/score.rs`:

```rust
use std::io::Write;
use std::path::{Path, PathBuf};

use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::rot::{self, Score};
use super::{CtxResult, adapters};

#[derive(Debug, clap::Args)]
pub struct ScoreArgs {
    /// Path to the agent transcript (JSONL).
    #[arg(long)]
    pub transcript: PathBuf,
    /// Adapter name: claude or codex. Defaults to config, then claude.
    #[arg(long)]
    pub agent: Option<String>,
}

/// Shared by `hook`, `exec`, `loop` and `wrap`: read a transcript, parse it with
/// the selected adapter, score it.
pub fn score_transcript(
    transcript: &Path,
    agent: Option<&str>,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<Score> {
    let cfg = CtxConfig::load(repo, env)?;
    let adapter = adapters::select(
        agent.or(cfg.agent.as_deref()),
        &[],
        cfg.agent_bin.as_deref(),
    )?;
    let jsonl = std::fs::read_to_string(transcript)
        .map_err(|e| format!("{}: {e}", transcript.display()))?;
    let events = adapter.parse_events(&jsonl);
    Ok(rot::score_events(&events, adapter.capabilities(), &cfg.score))
}

pub fn run_with<W: Write>(
    args: &ScoreArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let score = score_transcript(&args.transcript, args.agent.as_deref(), repo, env)?;
    writeln!(w, "{}", serde_json::to_string(&score)?)?;
    Ok(0)
}

pub fn run<W: Write>(args: &ScoreArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &repo, &env)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test ctx::score 2>&1 | tail -20`
Expected: PASS, 5 tests.

- [ ] **Step 5: Verify by hand against a real transcript**

Run: `cargo run -- ctx score --transcript "$(ls -t ~/.claude/projects/*/*.jsonl | head -1)"`
Expected: one line of JSON with a plausible `context_tokens` (tens of thousands or more, not `2`) and a verdict consistent with that size.

- [ ] **Step 6: Commit**

```bash
git add src/commands/ctx/score.rs
git commit -m "feat(ctx): zirv ctx score verb"
```

---

### Task A14: Turn-signal unix socket

**Files:**
- Modify: `src/commands/ctx/signal.rs`

**Interfaces:**
- Consumes: `Verdict` (A12), `CtxResult` (A1).
- Produces:
  - `pub struct TurnSignal { pub session_id: String, pub turn: u64, pub score: u32, pub verdict: Verdict }` deriving `Debug, Clone, PartialEq, Serialize, Deserialize`
  - `pub struct SignalServer` with `pub fn bind(path: &Path) -> CtxResult<Self>`, `pub fn try_recv(&self) -> Option<TurnSignal>`, `pub fn path(&self) -> &Path`
  - `pub fn send(path: &Path, signal: &TurnSignal) -> CtxResult<()>`
  - `pub const MAX_SOCKET_PATH: usize = 100;`

macOS caps socket paths near 104 bytes, so `bind` rejects anything longer than `MAX_SOCKET_PATH` with a readable error rather than letting the OS return `EINVAL` from deep inside `wrap`. On Windows both `bind` and `send` return `Err`, and callers degrade.

- [ ] **Step 1: Write the failing test**

Bottom of `src/commands/ctx/signal.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::rot::Verdict;

    fn sample(turn: u64) -> TurnSignal {
        TurnSignal {
            session_id: "11111111-2222-4333-8444-555555555555".to_string(),
            turn,
            score: 64,
            verdict: Verdict::Compact,
        }
    }

    #[test]
    fn signals_round_trip_through_json() {
        let json = serde_json::to_string(&sample(3)).expect("serialize");
        assert!(json.contains("\"verdict\":\"compact\""), "got {json}");
        let back: TurnSignal = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, sample(3));
    }

    #[cfg(unix)]
    #[test]
    fn a_bound_server_receives_sent_signals_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.sock");
        let server = SignalServer::bind(&path).expect("bind");
        assert_eq!(server.path(), path.as_path());
        assert!(path.exists(), "the socket file is created");

        for turn in 1..=3 {
            send(&path, &sample(turn)).expect("send");
        }

        let mut received = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while received.len() < 3 && std::time::Instant::now() < deadline {
            if let Some(signal) = server.try_recv() {
                received.push(signal.turn);
            } else {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        assert_eq!(received, vec![1, 2, 3]);
    }

    #[cfg(unix)]
    #[test]
    fn try_recv_is_non_blocking_when_nothing_arrived() {
        let dir = tempfile::tempdir().expect("tempdir");
        let server = SignalServer::bind(&dir.path().join("q.sock")).expect("bind");
        assert!(server.try_recv().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn sending_to_a_dead_socket_is_an_error_not_a_hang() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = send(&dir.path().join("missing.sock"), &sample(1)).expect_err("no listener");
        assert!(!err.to_string().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn rebinding_replaces_a_stale_socket_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("stale.sock");
        std::fs::write(&path, "leftover").expect("write");
        let _server = SignalServer::bind(&path).expect("bind over a stale file");
        send(&path, &sample(9)).expect("send");
    }

    #[cfg(unix)]
    #[test]
    fn an_over_long_socket_path_fails_with_a_clear_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        let long = dir.path().join("x".repeat(MAX_SOCKET_PATH + 20));
        let err = SignalServer::bind(&long).expect_err("too long");
        assert!(
            err.to_string().contains("too long"),
            "message should say why: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dropping_the_server_removes_the_socket_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("gone.sock");
        {
            let _server = SignalServer::bind(&path).expect("bind");
            assert!(path.exists());
        }
        assert!(!path.exists());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ctx::signal 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find struct TurnSignal`.

- [ ] **Step 3: Write minimal implementation**

```rust
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::CtxResult;
use super::rot::Verdict;

/// macOS `sockaddr_un.sun_path` is 104 bytes. Fail early with a readable error
/// instead of an opaque OS error from inside a supervisor.
pub const MAX_SOCKET_PATH: usize = 100;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnSignal {
    pub session_id: String,
    pub turn: u64,
    pub score: u32,
    pub verdict: Verdict,
}

fn check_len(path: &Path) -> CtxResult<()> {
    let len = path.as_os_str().len();
    if len > MAX_SOCKET_PATH {
        return Err(format!(
            "socket path is too long ({len} bytes, limit {MAX_SOCKET_PATH}): {}",
            path.display()
        )
        .into());
    }
    Ok(())
}

#[cfg(unix)]
pub struct SignalServer {
    path: PathBuf,
    rx: std::sync::mpsc::Receiver<TurnSignal>,
}

#[cfg(unix)]
impl SignalServer {
    pub fn bind(path: &Path) -> CtxResult<Self> {
        use std::io::BufRead;

        check_len(path)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if path.exists() {
            std::fs::remove_file(path)?;
        }

        let listener = std::os::unix::net::UnixListener::bind(path)?;
        let (tx, rx) = std::sync::mpsc::channel();

        // The accept loop lives for the process. A foreground supervisor owns
        // the socket for its whole run, so there is nothing to join.
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let reader = std::io::BufReader::new(stream);
                for line in reader.lines().map_while(Result::ok) {
                    let Ok(signal) = serde_json::from_str::<TurnSignal>(&line) else {
                        continue;
                    };
                    if tx.send(signal).is_err() {
                        return;
                    }
                }
            }
        });

        Ok(Self {
            path: path.to_path_buf(),
            rx,
        })
    }

    pub fn try_recv(&self) -> Option<TurnSignal> {
        self.rx.try_recv().ok()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(unix)]
impl Drop for SignalServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
pub fn send(path: &Path, signal: &TurnSignal) -> CtxResult<()> {
    use std::io::Write;

    check_len(path)?;
    let mut stream = std::os::unix::net::UnixStream::connect(path)?;
    writeln!(stream, "{}", serde_json::to_string(signal)?)?;
    stream.flush()?;
    Ok(())
}

#[cfg(not(unix))]
pub struct SignalServer {
    path: PathBuf,
    rx: std::sync::mpsc::Receiver<TurnSignal>,
}

#[cfg(not(unix))]
impl SignalServer {
    pub fn bind(_path: &Path) -> CtxResult<Self> {
        Err("turn signals need unix domain sockets; supervision degrades to polling".into())
    }

    pub fn try_recv(&self) -> Option<TurnSignal> {
        self.rx.try_recv().ok()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(not(unix))]
pub fn send(_path: &Path, _signal: &TurnSignal) -> CtxResult<()> {
    Err("turn signals need unix domain sockets".into())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test ctx::signal 2>&1 | tail -20`
Expected: PASS, 7 tests on unix.

- [ ] **Step 5: Check the Windows branch compiles**

Run: `cargo clippy --all-targets -- -D warnings 2>&1 | tail -20`
Expected: no warnings. If `dead_code` fires on the `#[cfg(not(unix))]` fields, add `#[allow(dead_code)]` to that struct with a one-line comment saying the fields exist to keep the two branches type-identical.

- [ ] **Step 6: Commit**

```bash
git add src/commands/ctx/signal.rs
git commit -m "feat(ctx): unix socket turn signals"
```

---

### Task A15: `zirv ctx hook stop`

**Files:**
- Modify: `src/commands/ctx/hook.rs`

**Interfaces:**
- Consumes: `score_transcript` (A13), `signal::send`, `TurnSignal` (A14), `log::append`, `StateDir` (A3), `SOCKET_ENV`, `SESSION_ENV` (A6).
- Produces:
  - `pub struct HookArgs { pub event: HookEvent }` and `pub enum HookEvent { Stop, Prompt, PreCompact, Notify { payload: Option<String> } }`
  - `pub struct HookPayload { pub session_id: String, pub transcript_path: String, pub cwd: String, pub stop_hook_active: bool }` with `pub fn parse(raw: &str) -> CtxResult<HookPayload>`
  - `pub fn stop_output(payload: &HookPayload, score: &Score, socket: Option<&Path>) -> Option<String>` — the pure decision function, `None` meaning "print nothing"
  - `pub fn run<W: Write>(args: &HookArgs, w: &mut W) -> CtxResult<i32>` always returning `Ok(0)`

The hook is the one place where being wrong must never hurt: every failure path prints nothing and exits 0. That retires the old canary's blocking behavior.

- [ ] **Step 1: Write the failing test**

Bottom of `src/commands/ctx/hook.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::rot::{Score, Signals, Verdict};

    fn payload() -> HookPayload {
        HookPayload {
            session_id: "11111111-2222-4333-8444-555555555555".to_string(),
            transcript_path: "/tmp/t.jsonl".to_string(),
            cwd: "/work/repo".to_string(),
            stop_hook_active: false,
        }
    }

    fn score_of(verdict: Verdict, score: u32) -> Score {
        Score {
            score,
            verdict,
            signals: Signals {
                turns: 12,
                tool_failure_rate: 1.0,
                repetition_hits: 0,
                max_repeat: 1,
                marker_miss_rate: Some(1.0),
            },
            context_tokens: 170_000,
        }
    }

    #[test]
    fn payload_parsing_tolerates_missing_fields() {
        let parsed = HookPayload::parse("{\"session_id\":\"s\"}").expect("parse");
        assert_eq!(parsed.session_id, "s");
        assert_eq!(parsed.transcript_path, "");
        assert!(!parsed.stop_hook_active);

        let full = HookPayload::parse(
            "{\"session_id\":\"s\",\"transcript_path\":\"/t.jsonl\",\"cwd\":\"/c\",\"stop_hook_active\":true}",
        )
        .expect("parse");
        assert!(full.stop_hook_active);
        assert_eq!(full.cwd, "/c");
    }

    #[test]
    fn a_healthy_session_prints_nothing() {
        assert_eq!(stop_output(&payload(), &score_of(Verdict::Healthy, 10), None), None);
    }

    #[test]
    fn an_advisory_verdict_prints_a_non_blocking_system_message() {
        let out = stop_output(&payload(), &score_of(Verdict::Advise, 45), None)
            .expect("advisory expected");
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert!(parsed["systemMessage"].is_string());
        assert!(
            parsed.get("decision").is_none(),
            "the hook must never block the stop: {out}"
        );
        let text = parsed["systemMessage"].as_str().unwrap_or_default();
        assert!(text.contains("advise"), "verdict should be named: {text}");
        assert!(!text.contains('\u{2014}'), "no em dashes in user-facing copy");
    }

    #[test]
    fn a_restart_verdict_still_only_advises() {
        let out = stop_output(&payload(), &score_of(Verdict::Restart, 95), None)
            .expect("advisory expected");
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert!(parsed.get("decision").is_none());
        let text = parsed["systemMessage"].as_str().unwrap_or_default();
        assert!(text.contains("zirv ctx resume"), "point at recovery: {text}");
    }

    #[test]
    fn when_a_supervisor_owns_the_session_the_hook_stays_silent() {
        let out = stop_output(
            &payload(),
            &score_of(Verdict::Restart, 95),
            Some(std::path::Path::new("/tmp/s/ab.sock")),
        );
        assert_eq!(out, None, "the supervisor intervenes, not the hook");
    }

    /// Ported canary case 7: never fire twice in a row.
    #[test]
    fn stop_hook_active_short_circuits_everything() {
        let mut p = payload();
        p.stop_hook_active = true;
        assert_eq!(stop_output(&p, &score_of(Verdict::Restart, 95), None), None);
    }

    #[test]
    fn run_exits_zero_even_with_unparseable_stdin() {
        let mut out = Vec::new();
        let code = run_stop(&mut out, "this is not json", &|_| None).expect("never errors");
        assert_eq!(code, 0);
        assert!(out.is_empty(), "nothing on stdout: {out:?}");
    }

    #[test]
    fn run_exits_zero_when_the_transcript_is_gone() {
        let mut out = Vec::new();
        let code = run_stop(
            &mut out,
            "{\"session_id\":\"s\",\"transcript_path\":\"/nope/missing.jsonl\",\"cwd\":\"/tmp\"}",
            &|_| None,
        )
        .expect("never errors");
        assert_eq!(code, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn run_scores_a_real_transcript_and_advises() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = dir.path().join("t.jsonl");
        let mut text = String::new();
        for i in 0..12 {
            text.push_str("{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n");
            text.push_str("{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"r\",\"is_error\":true}]}}\n");
            let block = if i < 2 { "[zirv] ok" } else { "sloppy" };
            text.push_str(&format!(
                "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{block}\"}}],\"usage\":{{\"input_tokens\":170000}}}}}}\n"
            ));
        }
        std::fs::write(&transcript, text).expect("write");

        let state = dir.path().join("state");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state.display().to_string(),
        )]
        .into();

        let stdin = format!(
            "{{\"session_id\":\"s\",\"transcript_path\":\"{}\",\"cwd\":\"{}\"}}",
            transcript.display(),
            dir.path().display()
        );
        let mut out = Vec::new();
        let code = run_stop(&mut out, &stdin, &|k| env.get(k).cloned()).expect("runs");
        assert_eq!(code, 0);

        let text = String::from_utf8(out).expect("utf8");
        let parsed: serde_json::Value = serde_json::from_str(text.trim()).expect("json");
        assert!(parsed["systemMessage"].as_str().unwrap_or_default().contains("restart"));

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log written");
        assert!(log.contains("\"verb\":\"hook\""), "got {log}");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ctx::hook 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find struct HookPayload`.

- [ ] **Step 3: Write minimal implementation**

Replace the stub in `src/commands/ctx/hook.rs`:

```rust
use std::io::{Read, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::adapters::{SESSION_ENV, SOCKET_ENV};
use super::config::{EnvLookup, env_from_process};
use super::rot::{Score, Verdict};
use super::state::{StateDir, now_secs};
use super::{CtxResult, log, score, signal};

#[derive(Debug, clap::Args)]
pub struct HookArgs {
    #[command(subcommand)]
    pub event: HookEvent,
}

#[derive(Debug, clap::Subcommand)]
pub enum HookEvent {
    /// Claude Stop hook: score the turn and forward or advise.
    Stop,
    /// Claude UserPromptSubmit hook: install the reply marker instruction.
    Prompt,
    /// Claude PreCompact hook: record that a compaction is starting.
    PreCompact,
    /// Codex notify program: same role as Stop.
    Notify {
        /// Payload, when the agent passes it as an argument instead of stdin.
        payload: Option<String>,
    },
}

/// `stop_hook_active` is absent from the published field table but is delivered
/// in practice, so every field is optional with a zero default. `Serialize` is
/// needed because Task A16 maps a codex notify payload into this shape and hands
/// it back to `run_stop`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct HookPayload {
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    pub stop_hook_active: bool,
}

impl HookPayload {
    pub fn parse(raw: &str) -> CtxResult<Self> {
        Ok(serde_json::from_str(raw)?)
    }

    fn repo(&self) -> std::path::PathBuf {
        if self.cwd.is_empty() {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        } else {
            std::path::PathBuf::from(&self.cwd)
        }
    }
}

/// Decides what the Stop hook prints. `None` means print nothing, which is also
/// what every failure path does.
pub fn stop_output(payload: &HookPayload, score: &Score, socket: Option<&Path>) -> Option<String> {
    if payload.stop_hook_active {
        return None;
    }
    if socket.is_some() {
        return None;
    }
    if score.verdict == Verdict::Healthy {
        return None;
    }

    let advisory = format!(
        "zirv ctx: verdict {} (score {}, context {} tokens). Consider /compact, or run `zirv ctx resume` for a clean session with a handoff.",
        score.verdict.as_str(),
        score.score,
        score.context_tokens
    );
    serde_json::to_string(&serde_json::json!({ "systemMessage": advisory })).ok()
}

fn read_stdin() -> String {
    let mut buffer = String::new();
    let _ = std::io::stdin().read_to_string(&mut buffer);
    buffer
}

pub fn run_stop<W: Write>(w: &mut W, stdin: &str, env: EnvLookup<'_>) -> CtxResult<i32> {
    // Every early return is deliberate: a hook that errors must still exit 0.
    let Ok(payload) = HookPayload::parse(stdin) else {
        return Ok(0);
    };
    if payload.stop_hook_active || payload.transcript_path.is_empty() {
        return Ok(0);
    }
    let transcript = Path::new(&payload.transcript_path);
    if !transcript.is_file() {
        return Ok(0);
    }
    let repo = payload.repo();
    let Ok(score) = score::score_transcript(transcript, None, &repo, env) else {
        return Ok(0);
    };

    let socket = env(SOCKET_ENV).map(std::path::PathBuf::from);
    let session = env(SESSION_ENV).unwrap_or_else(|| payload.session_id.clone());

    if let Some(path) = socket.as_deref() {
        let turn = score.signals.turns as u64;
        let _ = signal::send(
            path,
            &signal::TurnSignal {
                session_id: session.clone(),
                turn,
                score: score.score,
                verdict: score.verdict,
            },
        );
    }

    if let Ok(state) = StateDir::resolve(env) {
        let _ = log::append(
            &state,
            &log::Decision {
                ts: now_secs(),
                session: &session,
                verb: "hook",
                verdict: score.verdict.as_str(),
                score: score.score,
                action: if socket.is_some() { "forward" } else { "advise" },
                detail: &payload.transcript_path,
            },
        );
    }

    if let Some(line) = stop_output(&payload, &score, socket.as_deref()) {
        let _ = writeln!(w, "{line}");
    }
    Ok(0)
}

pub fn run<W: Write>(args: &HookArgs, w: &mut W) -> CtxResult<i32> {
    let env = env_from_process();
    match &args.event {
        HookEvent::Stop => run_stop(w, &read_stdin(), &env),
        _ => Ok(0),
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test ctx::hook 2>&1 | tail -20`
Expected: PASS, 9 tests.

- [ ] **Step 5: Verify the hook end to end by hand**

Run:

```bash
T=$(ls -t ~/.claude/projects/*/*.jsonl | head -1)
printf '{"session_id":"probe","transcript_path":"%s","cwd":"%s","stop_hook_active":false}' "$T" "$PWD" \
  | cargo run -- ctx hook stop; echo "exit=$?"
```

Expected: `exit=0`, and either no output (healthy) or a single `{"systemMessage": ...}` line. Never a `decision` field.

- [ ] **Step 6: Commit**

```bash
git add src/commands/ctx/hook.rs
git commit -m "feat(ctx): non-blocking stop hook that scores and forwards turns"
```

---

### Task A16: `zirv ctx hook prompt`, `pre-compact` and `notify`

**Files:**
- Modify: `src/commands/ctx/hook.rs`

**Interfaces:**
- Consumes: `HookPayload`, `run_stop` (A15); `CtxConfig` (A2); the notify contract recorded in A9.
- Produces: `pub fn prompt_output(marker: &str) -> String`, `pub fn pre_compact_output() -> String`, `pub fn run_notify<W: Write>(w: &mut W, payload: &str, env: EnvLookup<'_>) -> CtxResult<i32>`, and `run` wired for all four events.

**Verified constraint:** `PreCompact` cannot inject compaction instructions. It honors only `decision`, `reason`, `continue`, `stopReason`, `suppressOutput` and `systemMessage`. The spec's table entry "compaction focus instructions" is therefore delivered by `wrap` as arguments to the injected `/compact <focus>` command (Task C4), and this hook only records the event and prints an advisory.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` in `src/commands/ctx/hook.rs`:

```rust
    #[test]
    fn prompt_hook_emits_the_documented_injection_shape() {
        let out = prompt_output("[zirv]");
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert_eq!(
            parsed["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit",
            "exact key casing matters: {out}"
        );
        let context = parsed["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("additionalContext");
        assert!(context.contains("[zirv]"), "the marker must appear: {context}");
        assert!(context.contains("final"), "only final answers carry the marker: {context}");
        assert!(parsed.get("decision").is_none(), "never block a prompt");
        assert!(!context.contains('\u{2014}'));
    }

    #[test]
    fn prompt_hook_uses_the_configured_marker() {
        let out = prompt_output("[acme]");
        assert!(out.contains("[acme]"));
        assert!(!out.contains("[zirv]"), "nothing user-specific is hardcoded");
    }

    #[test]
    fn pre_compact_only_advises_because_injection_is_unsupported() {
        let out = pre_compact_output();
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert!(parsed["systemMessage"].is_string());
        assert!(parsed.get("decision").is_none(), "never block a compaction");
        assert!(
            parsed.get("hookSpecificOutput").is_none(),
            "PreCompact honors no additionalContext"
        );
    }

    /// PLACEHOLDER PAYLOAD, REPLACE DURING A9/A10 EXECUTION. The literal below
    /// must be swapped for the real codex notify payload recorded in
    /// docs/superpowers/notes/2026-07-31-codex-cli-facts.md, and the field names
    /// in `notify_payload_to_hook` updated to match. Until then this test only
    /// proves the shape-mapping seam exists, not that it maps codex correctly.
    const CODEX_NOTIFY_SAMPLE: &str =
        "{\"type\":\"agent-turn-complete\",\"session_id\":\"s\",\"rollout_path\":\"/tmp/r.jsonl\",\"cwd\":\"/work\"}";

    #[test]
    fn notify_maps_the_codex_payload_onto_the_hook_payload() {
        let mapped = notify_payload_to_hook(CODEX_NOTIFY_SAMPLE).expect("mapping exists");
        assert_eq!(mapped.session_id, "s");
        assert_eq!(
            mapped.transcript_path, "/tmp/r.jsonl",
            "codex names the transcript differently from claude, so it must be mapped, not assumed"
        );
        assert_eq!(mapped.cwd, "/work");
        assert!(!mapped.stop_hook_active);
    }

    #[test]
    fn a_notify_payload_with_no_transcript_field_is_an_explicit_error() {
        // Silently scoring nothing is the failure mode this guards against: a
        // dropped turn signal with no diagnostic is worse than a loud mismatch.
        let err = notify_payload_to_hook("{\"session_id\":\"s\"}")
            .expect_err("an unmapped payload must not look like a healthy session");
        let msg = err.to_string();
        assert!(msg.contains("transcript"), "say what is missing: {msg}");
        assert!(msg.contains("codex-cli-facts"), "point at the verified notes: {msg}");
    }

    #[test]
    fn notify_accepts_an_argv_payload_and_exits_zero() {
        let mut out = Vec::new();
        let code = run_notify(&mut out, CODEX_NOTIFY_SAMPLE, &|_| None).expect("runs");
        assert_eq!(code, 0);
    }

    #[test]
    fn notify_survives_a_non_json_payload() {
        let mut out = Vec::new();
        let code = run_notify(&mut out, "agent-turn-complete", &|_| None).expect("runs");
        assert_eq!(code, 0);
        assert!(out.is_empty(), "no output and no panic: {out:?}");
    }

    #[test]
    fn notify_falls_back_to_the_claude_shape_when_that_is_what_arrives() {
        // The claude Stop payload already carries `transcript_path`, so a hook
        // registered on either agent keeps working.
        let mapped = notify_payload_to_hook(
            "{\"session_id\":\"s\",\"transcript_path\":\"/tmp/t.jsonl\",\"cwd\":\"/work\"}",
        )
        .expect("claude shape maps straight through");
        assert_eq!(mapped.transcript_path, "/tmp/t.jsonl");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ctx::hook 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find function prompt_output`.

- [ ] **Step 3: Write minimal implementation**

Add to `src/commands/ctx/hook.rs`:

```rust
/// UserPromptSubmit is the only hook that can add context to the model, which
/// is how the marker signal gets installed.
pub fn prompt_output(marker: &str) -> String {
    let context = format!(
        "Start every final answer in this session with the prefix {marker} on the first line. \
         Mid-turn status notes do not need it. This is a context-health marker read by zirv ctx."
    );
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": context
        }
    })
    .to_string()
}

/// PreCompact cannot add instructions to a compaction (verified against the
/// hook reference), so this records the event and says so. Focus instructions
/// ride along with wrap's injected `/compact <focus>` command instead.
pub fn pre_compact_output() -> String {
    serde_json::json!({
        "systemMessage": "zirv ctx: compaction starting. Preserve the current task, file paths and unresolved errors."
    })
    .to_string()
}

/// Field names codex uses for the rollout path, most specific first. Populate
/// from the verified notes file during Task A9/A10; the claude spelling stays
/// last so a hook registered on either agent keeps working.
const NOTIFY_TRANSCRIPT_KEYS: &[&str] = &["rollout_path", "session_file", "transcript_path"];

/// Maps an agent's notify payload onto the shape the scorer needs. Codex does
/// not use claude's field names, so this is a real mapping rather than an alias:
/// aliasing would let a renamed field parse as an empty transcript path and drop
/// every turn signal without a word.
pub fn notify_payload_to_hook(raw: &str) -> CtxResult<HookPayload> {
    let value: serde_json::Value = serde_json::from_str(raw)?;

    let transcript_path = NOTIFY_TRANSCRIPT_KEYS
        .iter()
        .find_map(|key| value.get(*key).and_then(serde_json::Value::as_str))
        .ok_or_else(|| {
            format!(
                "notify payload carries no known transcript field (tried {}); \
                 record the real field name in \
                 docs/superpowers/notes/2026-07-31-codex-cli-facts.md and add it to \
                 NOTIFY_TRANSCRIPT_KEYS",
                NOTIFY_TRANSCRIPT_KEYS.join(", ")
            )
        })?
        .to_string();

    let string_at = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };

    Ok(HookPayload {
        session_id: string_at("session_id"),
        transcript_path,
        cwd: string_at("cwd"),
        stop_hook_active: false,
    })
}

pub fn run_notify<W: Write>(w: &mut W, payload: &str, env: EnvLookup<'_>) -> CtxResult<i32> {
    // Codex passes its notify payload as an argument on some versions and on
    // stdin on others (see docs/superpowers/notes/2026-07-31-codex-cli-facts.md),
    // so both routes land here.
    let Ok(mapped) = notify_payload_to_hook(payload) else {
        // A hook never blocks the agent, so an unmapped payload is recorded
        // rather than surfaced. The decision log is where a silent mismatch
        // becomes visible.
        if let Ok(state) = StateDir::resolve(env) {
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: "unknown",
                    verb: "hook",
                    verdict: "n/a",
                    score: 0,
                    action: "notify-unmapped",
                    detail: payload.chars().take(200).collect::<String>().as_str(),
                },
            );
        }
        return Ok(0);
    };

    run_stop(w, &serde_json::to_string(&mapped)?, env)
}
```

`run_stop` takes JSON, so `HookPayload` needs `Serialize` alongside `Deserialize`. Add it to the derive list in Task A15's struct: `#[derive(Debug, Clone, Default, Deserialize, Serialize)]`, and add `use serde::Serialize;` to the imports.

Replace `run`:

```rust
pub fn run<W: Write>(args: &HookArgs, w: &mut W) -> CtxResult<i32> {
    let env = env_from_process();
    match &args.event {
        HookEvent::Stop => run_stop(w, &read_stdin(), &env),
        HookEvent::Prompt => {
            let repo = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let marker = super::config::CtxConfig::load(&repo, &env)
                .map(|cfg| cfg.score.marker)
                .unwrap_or_else(|_| super::config::DEFAULT_MARKER.to_string());
            if !marker.is_empty() {
                let _ = writeln!(w, "{}", prompt_output(&marker));
            }
            Ok(0)
        }
        HookEvent::PreCompact => {
            let _ = writeln!(w, "{}", pre_compact_output());
            Ok(0)
        }
        HookEvent::Notify { payload } => {
            let raw = match payload {
                Some(text) => text.clone(),
                None => read_stdin(),
            };
            run_notify(w, &raw, &env)
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test ctx::hook -- --test-threads=1 2>&1 | tail -20`
Expected: PASS, 17 tests.

- [ ] **Step 5: Verify each hook exits zero by hand**

Run:

```bash
for e in stop prompt pre-compact notify; do
  printf '{}' | cargo run --quiet -- ctx hook "$e" >/dev/null; echo "$e exit=$?"
done
```

Expected: `exit=0` for all four.

- [ ] **Step 6: Commit**

```bash
git add src/commands/ctx/hook.rs
git commit -m "feat(ctx): prompt, pre-compact and notify hook entrypoints"
```

---

### Task A17: Handoff type, markdown round-trip and structural fallback

**Files:**
- Modify: `src/commands/ctx/handoff.rs`

**Interfaces:**
- Consumes: `StructuralContext` (A4).
- Produces:
  - `pub struct Handoff { pub task: String, pub done: Vec<String>, pub remaining: Vec<String>, pub next_step: String, pub files_touched: Vec<String>, pub gotchas: Vec<String> }` deriving `Debug, Clone, Default, PartialEq`
  - `impl Handoff { pub fn to_markdown(&self) -> String; pub fn is_usable(&self) -> bool }`
  - `pub fn parse_markdown(md: &str) -> Handoff`
  - `pub fn structural(ctx: &StructuralContext) -> Handoff`
  - `pub const SECTIONS: [&str; 6] = ["Task", "Done", "Remaining", "Next step", "Files touched", "Gotchas learned"];`

- [ ] **Step 1: Write the failing test**

Bottom of `src/commands/ctx/handoff.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::event::StructuralContext;

    fn sample() -> Handoff {
        Handoff {
            task: "Wire the payments webhook".to_string(),
            done: vec!["Added the route".to_string(), "Wrote the parser".to_string()],
            remaining: vec!["Signature verification".to_string()],
            next_step: "Add a failing test for an invalid signature".to_string(),
            files_touched: vec!["src/routes/webhook.rs".to_string()],
            gotchas: vec!["The provider sends two events per charge".to_string()],
        }
    }

    #[test]
    fn markdown_uses_the_documented_section_order() {
        let md = sample().to_markdown();
        let positions: Vec<usize> = SECTIONS
            .iter()
            .map(|s| md.find(&format!("## {s}")).unwrap_or_else(|| panic!("{s} missing")))
            .collect();
        assert!(
            positions.windows(2).all(|w| w[0] < w[1]),
            "sections out of order in:\n{md}"
        );
    }

    #[test]
    fn markdown_round_trips() {
        let original = sample();
        assert_eq!(parse_markdown(&original.to_markdown()), original);
    }

    #[test]
    fn parsing_tolerates_extra_prose_and_missing_sections() {
        let md = "Here is the handoff you asked for.\n\n## Task\nShip the thing\n\n## Next step\nRun the tests\n";
        let parsed = parse_markdown(md);
        assert_eq!(parsed.task, "Ship the thing");
        assert_eq!(parsed.next_step, "Run the tests");
        assert!(parsed.done.is_empty());
        assert!(parsed.remaining.is_empty());
    }

    #[test]
    fn parsing_accepts_both_bullet_styles() {
        let md = "## Done\n- first\n* second\n1. third\n";
        assert_eq!(parse_markdown(md).done, vec!["first", "second", "third"]);
    }

    #[test]
    fn is_usable_requires_a_task_and_a_next_step() {
        assert!(sample().is_usable());
        assert!(!Handoff::default().is_usable());
        assert!(
            !Handoff {
                task: "something".to_string(),
                ..Handoff::default()
            }
            .is_usable(),
            "a handoff with no next step is not something to stand on"
        );
    }

    #[test]
    fn structural_fallback_uses_the_last_prompt_as_the_task() {
        let ctx = StructuralContext {
            user_messages: vec!["old request".to_string(), "fix the flaky test".to_string()],
            assistant_texts: vec!["[zirv] narrowed it to the timer".to_string()],
            files_touched: vec!["src/timer.rs".to_string()],
            tool_errors: vec!["assertion failed: expected 3".to_string()],
        };
        let handoff = structural(&ctx);
        assert_eq!(handoff.task, "fix the flaky test");
        assert_eq!(handoff.files_touched, vec!["src/timer.rs"]);
        assert!(handoff.done.iter().any(|d| d.contains("narrowed it")));
        assert!(handoff.remaining.iter().any(|r| r.contains("assertion failed")));
        assert!(!handoff.next_step.is_empty(), "always leave a next step");
        assert!(handoff.is_usable());
    }

    #[test]
    fn structural_fallback_survives_an_empty_context() {
        let handoff = structural(&StructuralContext::default());
        assert!(handoff.is_usable(), "a restart must always have something to stand on");
        assert!(handoff.to_markdown().contains("## Task"));
    }

    #[test]
    fn structural_markdown_has_no_em_dashes() {
        let ctx = StructuralContext {
            user_messages: vec!["do it".to_string()],
            ..StructuralContext::default()
        };
        assert!(!structural(&ctx).to_markdown().contains('\u{2014}'));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ctx::handoff 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find struct Handoff`.

- [ ] **Step 3: Write minimal implementation**

Replace the stub in `src/commands/ctx/handoff.rs`:

```rust
use std::io::Write;

use super::CtxResult;
use super::event::StructuralContext;

pub const SECTIONS: [&str; 6] = [
    "Task",
    "Done",
    "Remaining",
    "Next step",
    "Files touched",
    "Gotchas learned",
];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Handoff {
    pub task: String,
    pub done: Vec<String>,
    pub remaining: Vec<String>,
    pub next_step: String,
    pub files_touched: Vec<String>,
    pub gotchas: Vec<String>,
}

fn write_list(out: &mut String, heading: &str, items: &[String]) {
    out.push_str(&format!("## {heading}\n"));
    for item in items {
        out.push_str(&format!("- {item}\n"));
    }
    out.push('\n');
}

impl Handoff {
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("## Task\n{}\n\n", self.task));
        write_list(&mut out, "Done", &self.done);
        write_list(&mut out, "Remaining", &self.remaining);
        out.push_str(&format!("## Next step\n{}\n\n", self.next_step));
        write_list(&mut out, "Files touched", &self.files_touched);
        write_list(&mut out, "Gotchas learned", &self.gotchas);
        out
    }

    /// A handoff without a task or a next step is not worth restarting on.
    pub fn is_usable(&self) -> bool {
        !self.task.trim().is_empty() && !self.next_step.trim().is_empty()
    }
}

fn strip_bullet(line: &str) -> Option<String> {
    let trimmed = line.trim();
    for prefix in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some(rest.trim().to_string());
        }
    }
    // Numbered lists: "1. item"
    let mut chars = trimmed.chars();
    let digits: String = chars.by_ref().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() && trimmed[digits.len()..].starts_with(". ") {
        return Some(trimmed[digits.len() + 2..].trim().to_string());
    }
    None
}

pub fn parse_markdown(md: &str) -> Handoff {
    let mut handoff = Handoff::default();
    let mut section: Option<&str> = None;

    for line in md.lines() {
        if let Some(rest) = line.trim().strip_prefix("## ") {
            let name = rest.trim();
            section = SECTIONS.iter().find(|s| s.eq_ignore_ascii_case(name)).copied();
            continue;
        }
        let Some(current) = section else { continue };
        let bullet = strip_bullet(line);
        let plain = line.trim();

        match current {
            "Task" => {
                if handoff.task.is_empty() && !plain.is_empty() {
                    handoff.task = bullet.unwrap_or_else(|| plain.to_string());
                }
            }
            "Next step" => {
                if handoff.next_step.is_empty() && !plain.is_empty() {
                    handoff.next_step = bullet.unwrap_or_else(|| plain.to_string());
                }
            }
            "Done" => handoff.done.extend(bullet),
            "Remaining" => handoff.remaining.extend(bullet),
            "Files touched" => handoff.files_touched.extend(bullet),
            "Gotchas learned" => handoff.gotchas.extend(bullet),
            _ => {}
        }
    }
    handoff
}

/// Mechanical extraction used when the distiller is unavailable or unusable.
/// Never fails and never returns something unusable.
pub fn structural(ctx: &StructuralContext) -> Handoff {
    let task = ctx
        .user_messages
        .last()
        .map(|m| m.lines().next().unwrap_or(m).trim().to_string())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "Unknown task (no user prompt found in the transcript)".to_string());

    let done: Vec<String> = ctx
        .assistant_texts
        .iter()
        .map(|t| t.lines().next().unwrap_or(t).trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();

    let remaining: Vec<String> = ctx
        .tool_errors
        .iter()
        .map(|e| format!("Unresolved error: {}", e.lines().next().unwrap_or(e).trim()))
        .collect();

    Handoff {
        task,
        done,
        remaining,
        next_step: "Re-read the files listed below, then continue the task above from where the previous session stopped.".to_string(),
        files_touched: ctx.files_touched.clone(),
        gotchas: vec!["This handoff was extracted mechanically, so it may be incomplete.".to_string()],
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test ctx::handoff 2>&1 | tail -20`
Expected: PASS, 8 tests. If `parsing_accepts_both_bullet_styles` fails on the numbered case, check the `trimmed[digits.len()..]` slice: `take_while` on a `by_ref` iterator consumes the delimiter, so build `digits` with `trimmed.chars().take_while(...)` on a fresh iterator instead.

- [ ] **Step 5: Commit**

```bash
git add src/commands/ctx/handoff.rs
git commit -m "feat(ctx): handoff type, markdown round-trip and structural fallback"
```

---

### Task A18: Handoff distillation through a fresh model call

**Files:**
- Modify: `src/commands/ctx/handoff.rs`
- Create: `tests/fixtures/fake-model.sh`

**Interfaces:**
- Consumes: `Handoff`, `parse_markdown`, `structural` (A17); `AgentAdapter::distiller_cmd` (A8).
- Produces:
  - `pub const DISTILL_PROMPT_VERSION: &str = "v1";`
  - `pub fn distill_prompt(ctx: &StructuralContext) -> String`
  - `pub fn distill(adapter: &dyn AgentAdapter, model: &str, ctx: &StructuralContext) -> CtxResult<Handoff>`
  - `pub fn distill_or_structural(adapter: &dyn AgentAdapter, model: &str, ctx: &StructuralContext) -> (Handoff, &'static str)` where the second element is `"distilled"` or `"structural"`

The distiller is a **fresh** process on a cheap model. The rotted session is never asked to summarize itself.

- [ ] **Step 1: Write the fake model binary**

Create `tests/fixtures/fake-model.sh` and `chmod +x` it:

```sh
#!/bin/sh
# Stands in for `claude -p --model haiku` during handoff tests.
# Reads the distillation prompt on stdin and answers per FAKE_MODEL_MODE:
#   good    (default) a well-formed handoff
#   partial a handoff with no next step, so callers must fall back
#   garbage prose with no sections
#   fail    non-zero exit
#   echo    dumps the prompt it received to $FAKE_MODEL_PROMPT_LOG and answers good
set -eu
prompt=$(cat)
[ -z "${FAKE_MODEL_PROMPT_LOG:-}" ] || printf '%s' "$prompt" > "$FAKE_MODEL_PROMPT_LOG"

case "${FAKE_MODEL_MODE:-good}" in
  fail) exit 4 ;;
  garbage) printf 'I had a look and things seem mostly fine.\n' ;;
  partial)
    printf '## Task\nShip the webhook\n\n## Done\n- wrote the route\n'
    ;;
  *)
    printf '## Task\nShip the webhook\n\n'
    printf '## Done\n- wrote the route\n- wrote the parser\n\n'
    printf '## Remaining\n- signature verification\n\n'
    printf '## Next step\nAdd a failing test for an invalid signature\n\n'
    printf '## Files touched\n- src/routes/webhook.rs\n\n'
    printf '## Gotchas learned\n- the provider sends two events per charge\n'
    ;;
esac
```

Run: `chmod +x tests/fixtures/fake-model.sh && FAKE_MODEL_MODE=good sh -c 'printf "" | ./tests/fixtures/fake-model.sh' | head -3`
Expected: the `## Task` heading and its body.

- [ ] **Step 2: Write the failing test**

Add to the `mod tests` in `src/commands/ctx/handoff.rs`:

```rust
    use crate::commands::ctx::adapters::claude::ClaudeAdapter;
    use crate::commands::ctx::adapters::AgentAdapter;

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    fn fake_model_adapter() -> ClaudeAdapter {
        ClaudeAdapter::new(Some(
            fixture("fake-model.sh").to_str().expect("utf8 path"),
        ))
    }

    fn ctx_sample() -> StructuralContext {
        StructuralContext {
            user_messages: vec!["ship the webhook".to_string()],
            assistant_texts: vec!["[zirv] wrote the route".to_string()],
            files_touched: vec!["src/routes/webhook.rs".to_string()],
            tool_errors: vec!["401 from the provider".to_string()],
        }
    }

    #[test]
    fn the_prompt_carries_the_context_and_asks_for_the_documented_sections() {
        let prompt = distill_prompt(&ctx_sample());
        for section in SECTIONS {
            assert!(prompt.contains(section), "prompt must name '{section}': {prompt}");
        }
        assert!(prompt.contains("ship the webhook"));
        assert!(prompt.contains("src/routes/webhook.rs"));
        assert!(prompt.contains("401 from the provider"));
        assert!(prompt.contains(DISTILL_PROMPT_VERSION), "version the template");
    }

    #[test]
    fn distillation_parses_a_well_formed_answer() {
        let adapter = fake_model_adapter();
        let handoff = distill(&adapter, "haiku", &ctx_sample()).expect("distills");
        assert_eq!(handoff.task, "Ship the webhook");
        assert_eq!(handoff.next_step, "Add a failing test for an invalid signature");
        assert_eq!(handoff.done.len(), 2);
        assert!(handoff.is_usable());
    }

    #[test]
    fn the_distiller_receives_the_prompt_on_stdin() {
        let log = tempfile::NamedTempFile::new().expect("tempfile");
        // SAFETY: CI runs tests single-threaded (`--test-threads=1`).
        unsafe {
            std::env::set_var("FAKE_MODEL_PROMPT_LOG", log.path());
        }
        let adapter = fake_model_adapter();
        distill(&adapter, "haiku", &ctx_sample()).expect("distills");
        unsafe {
            std::env::remove_var("FAKE_MODEL_PROMPT_LOG");
        }

        let seen = std::fs::read_to_string(log.path()).expect("log");
        assert!(seen.contains("ship the webhook"), "got: {seen}");
    }

    #[test]
    fn a_failing_distiller_is_an_error() {
        unsafe {
            std::env::set_var("FAKE_MODEL_MODE", "fail");
        }
        let adapter = fake_model_adapter();
        let result = distill(&adapter, "haiku", &ctx_sample());
        unsafe {
            std::env::remove_var("FAKE_MODEL_MODE");
        }
        let err = result.expect_err("non-zero exit must surface");
        assert!(err.to_string().contains("4"), "report the exit code: {err}");
    }

    #[test]
    fn an_unusable_answer_is_an_error_so_callers_can_fall_back() {
        for mode in ["garbage", "partial"] {
            unsafe {
                std::env::set_var("FAKE_MODEL_MODE", mode);
            }
            let adapter = fake_model_adapter();
            let result = distill(&adapter, "haiku", &ctx_sample());
            unsafe {
                std::env::remove_var("FAKE_MODEL_MODE");
            }
            assert!(result.is_err(), "mode {mode} should not produce a usable handoff");
        }
    }

    #[test]
    fn distill_or_structural_falls_back_and_reports_which_path_it_took() {
        let adapter = fake_model_adapter();
        let (handoff, source) = distill_or_structural(&adapter, "haiku", &ctx_sample());
        assert_eq!(source, "distilled");
        assert_eq!(handoff.task, "Ship the webhook");

        unsafe {
            std::env::set_var("FAKE_MODEL_MODE", "garbage");
        }
        let (handoff, source) = distill_or_structural(&adapter, "haiku", &ctx_sample());
        unsafe {
            std::env::remove_var("FAKE_MODEL_MODE");
        }
        assert_eq!(source, "structural");
        assert_eq!(handoff.task, "ship the webhook", "from the last user prompt");
        assert!(handoff.is_usable());
    }

    #[test]
    fn a_missing_distiller_binary_falls_back_instead_of_panicking() {
        let adapter = ClaudeAdapter::new(Some("/nonexistent/model-binary"));
        let (handoff, source) = distill_or_structural(&adapter, "haiku", &ctx_sample());
        assert_eq!(source, "structural");
        assert!(handoff.is_usable());
    }
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test ctx::handoff 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find function distill_prompt`.

- [ ] **Step 4: Write minimal implementation**

Add to `src/commands/ctx/handoff.rs`:

```rust
use std::process::Stdio;

use super::adapters::AgentAdapter;

pub const DISTILL_PROMPT_VERSION: &str = "v1";

fn bullets(items: &[String]) -> String {
    if items.is_empty() {
        return "(none)\n".to_string();
    }
    items.iter().map(|i| format!("- {i}\n")).collect()
}

pub fn distill_prompt(ctx: &StructuralContext) -> String {
    format!(
        "You are writing a handoff note ({DISTILL_PROMPT_VERSION}) so a fresh session can \
continue this work with no other context. Answer with markdown only, using exactly these \
sections in this order: {sections}. Use `## ` headings. Task and Next step are single lines; \
the rest are bullet lists. Be concrete: real file paths, real commands, real error text. Do \
not invent progress that is not evidenced below.\n\n\
### Recent user requests\n{requests}\n\
### Recent assistant replies\n{replies}\n\
### Files the session touched\n{files}\n\
### Unresolved tool errors\n{errors}",
        sections = SECTIONS.join(", "),
        requests = bullets(&ctx.user_messages),
        replies = bullets(&ctx.assistant_texts),
        files = bullets(&ctx.files_touched),
        errors = bullets(&ctx.tool_errors),
    )
}

/// Runs a fresh, cheap model over the context. The rotted session is never
/// asked to summarize itself.
pub fn distill(
    adapter: &dyn AgentAdapter,
    model: &str,
    ctx: &StructuralContext,
) -> CtxResult<Handoff> {
    let mut command = adapter.distiller_cmd(model);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = command.spawn()?;
    {
        let stdin = child.stdin.as_mut().ok_or("distiller stdin unavailable")?;
        stdin.write_all(distill_prompt(ctx).as_bytes())?;
    }
    let output = child.wait_with_output()?;

    if !output.status.success() {
        return Err(format!(
            "distiller exited with status {}",
            output.status.code().unwrap_or(-1)
        )
        .into());
    }

    let handoff = parse_markdown(&String::from_utf8_lossy(&output.stdout));
    if !handoff.is_usable() {
        return Err("distiller produced no usable Task and Next step".into());
    }
    Ok(handoff)
}

/// Never fails: a restart always has something to stand on.
pub fn distill_or_structural(
    adapter: &dyn AgentAdapter,
    model: &str,
    ctx: &StructuralContext,
) -> (Handoff, &'static str) {
    match distill(adapter, model, ctx) {
        Ok(handoff) => (handoff, "distilled"),
        Err(_) => (structural(ctx), "structural"),
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test ctx::handoff -- --test-threads=1 2>&1 | tail -20`
Expected: PASS, 16 tests. These tests mutate process env, so they must run single-threaded, which is how CI runs anyway.

- [ ] **Step 6: Commit**

```bash
git add src/commands/ctx/handoff.rs tests/fixtures/fake-model.sh
git commit -m "feat(ctx): distill handoffs with a fresh cheap model call"
```

---

### Task A19: `zirv ctx handoff` verb and handoff storage

**Files:**
- Modify: `src/commands/ctx/handoff.rs`

**Interfaces:**
- Consumes: `distill_or_structural` (A18); `StateDir`, `repo_slug`, `now_secs` (A3); `adapters::select` (A6); `CtxConfig` (A2); `log::append` (A3).
- Produces:
  - `pub struct HandoffArgs { pub transcript: PathBuf, pub agent: Option<String>, pub session_id: Option<String>, pub stdout: bool, pub no_model: bool }`
  - `pub fn store(state: &StateDir, repo: &Path, session: &str, handoff: &Handoff) -> CtxResult<PathBuf>`
  - `pub fn latest_for_repo(state: &StateDir, repo: &Path) -> CtxResult<Option<(PathBuf, Handoff)>>`
  - `pub fn run<W: Write>(args: &HandoffArgs, w: &mut W) -> CtxResult<i32>` and `pub fn run_with<W: Write>(args: &HandoffArgs, w: &mut W, repo: &Path, env: EnvLookup<'_>) -> CtxResult<i32>`

Stored at `<state>/handoffs/<repo-slug>/<unix-secs>-<session-prefix>.md`. Ten-digit unix seconds sort lexicographically in chronological order, so "latest" is the greatest file name.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` in `src/commands/ctx/handoff.rs`:

```rust
    use crate::commands::ctx::state::StateDir;

    fn transcript_with(dir: &std::path::Path, prompt: &str) -> std::path::PathBuf {
        let path = dir.join("t.jsonl");
        let mut text = String::new();
        text.push_str(&format!(
            "{{\"type\":\"user\",\"message\":{{\"content\":\"{prompt}\"}}}}\n"
        ));
        text.push_str("{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"a\",\"name\":\"Read\",\"input\":{\"file_path\":\"/work/src/lib.rs\"}}],\"usage\":{\"input_tokens\":9}}}\n");
        text.push_str("{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"[zirv] read it\"}],\"usage\":{\"input_tokens\":9}}}\n");
        std::fs::write(&path, text).expect("write");
        path
    }

    #[test]
    fn storing_writes_markdown_under_the_repo_slug() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = std::path::Path::new("/work/my-repo");

        let path = store(&state, repo, "11111111-2222", &sample()).expect("store");
        assert!(path.starts_with(state.handoffs().join("-work-my-repo")));
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("md"));

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("## Task"));
        assert!(text.contains("Wire the payments webhook"));
    }

    #[test]
    fn latest_for_repo_returns_the_newest_handoff() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = std::path::Path::new("/work/my-repo");
        state.ensure().expect("ensure");

        let dir = state.handoffs().join("-work-my-repo");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("1700000000-aaaa.md"), "## Task\nold\n\n## Next step\nold step\n")
            .expect("write");
        std::fs::write(dir.join("1700000900-bbbb.md"), "## Task\nnew\n\n## Next step\nnew step\n")
            .expect("write");

        let (path, handoff) = latest_for_repo(&state, repo).expect("lookup").expect("some");
        assert!(path.ends_with("1700000900-bbbb.md"));
        assert_eq!(handoff.task, "new");
    }

    #[test]
    fn latest_for_repo_is_none_when_nothing_was_stored() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        assert!(
            latest_for_repo(&state, std::path::Path::new("/work/other"))
                .expect("lookup")
                .is_none()
        );
    }

    #[test]
    fn latest_for_repo_does_not_leak_across_repos() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        store(&state, std::path::Path::new("/work/a"), "s", &sample()).expect("store");
        assert!(
            latest_for_repo(&state, std::path::Path::new("/work/b"))
                .expect("lookup")
                .is_none()
        );
    }

    #[test]
    fn the_verb_stores_a_handoff_and_prints_its_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let transcript = transcript_with(tmp.path(), "ship the webhook");
        let state = tmp.path().join("state");
        let env: std::collections::HashMap<String, String> = [
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state.display().to_string(),
            ),
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                fixture("fake-model.sh").display().to_string(),
            ),
        ]
        .into();

        let args = HandoffArgs {
            transcript,
            agent: None,
            session_id: Some("11111111-2222".to_string()),
            stdout: false,
            no_model: false,
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned()).expect("runs");
        assert_eq!(code, 0);

        let printed = String::from_utf8(out).expect("utf8").trim().to_string();
        assert!(printed.ends_with(".md"), "should print the stored path: {printed}");
        let text = std::fs::read_to_string(&printed).expect("stored file");
        assert!(text.contains("Ship the webhook"), "the distilled task: {text}");
    }

    #[test]
    fn no_model_skips_distillation_and_uses_the_structural_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let transcript = transcript_with(tmp.path(), "ship the webhook");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            tmp.path().join("state").display().to_string(),
        )]
        .into();

        let args = HandoffArgs {
            transcript,
            agent: None,
            session_id: None,
            stdout: true,
            no_model: true,
        };
        let mut out = Vec::new();
        run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned()).expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("ship the webhook"), "structural task: {text}");
        assert!(text.contains("/work/src/lib.rs"), "files from tool calls: {text}");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ctx::handoff 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find function store`.

- [ ] **Step 3: Write minimal implementation**

Add to `src/commands/ctx/handoff.rs`:

```rust
use std::path::{Path, PathBuf};

use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::state::{StateDir, now_secs, repo_slug};
use super::{adapters, log};

#[derive(Debug, clap::Args)]
pub struct HandoffArgs {
    /// Transcript to distill.
    #[arg(long)]
    pub transcript: PathBuf,
    /// Adapter name: claude or codex.
    #[arg(long)]
    pub agent: Option<String>,
    /// Session id recorded in the stored file name.
    #[arg(long)]
    pub session_id: Option<String>,
    /// Print the handoff markdown instead of the stored path.
    #[arg(long, default_value_t = false)]
    pub stdout: bool,
    /// Skip the model call and extract mechanically.
    #[arg(long, default_value_t = false)]
    pub no_model: bool,
}

pub fn store(
    state: &StateDir,
    repo: &Path,
    session: &str,
    handoff: &Handoff,
) -> CtxResult<PathBuf> {
    let dir = state.handoffs().join(repo_slug(repo));
    std::fs::create_dir_all(&dir)?;

    let short: String = session.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
    let path = dir.join(format!("{}-{}.md", now_secs(), short));
    std::fs::write(&path, handoff.to_markdown())?;
    Ok(path)
}

pub fn latest_for_repo(state: &StateDir, repo: &Path) -> CtxResult<Option<(PathBuf, Handoff)>> {
    let dir = state.handoffs().join(repo_slug(repo));
    if !dir.is_dir() {
        return Ok(None);
    }

    let mut names: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();
    names.sort();

    let Some(path) = names.pop() else {
        return Ok(None);
    };
    let handoff = parse_markdown(&std::fs::read_to_string(&path)?);
    Ok(Some((path, handoff)))
}

pub fn run_with<W: Write>(
    args: &HandoffArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load(repo, env)?;
    let adapter = adapters::select(
        args.agent.as_deref().or(cfg.agent.as_deref()),
        &[],
        cfg.agent_bin.as_deref(),
    )?;
    let jsonl = std::fs::read_to_string(&args.transcript)
        .map_err(|e| format!("{}: {e}", args.transcript.display()))?;
    let ctx = adapter.structural_context(&jsonl, cfg.handoff.tail_items);

    let (handoff, source) = if args.no_model {
        (structural(&ctx), "structural")
    } else {
        distill_or_structural(adapter.as_ref(), &cfg.handoff.model, &ctx)
    };

    if args.stdout {
        write!(w, "{}", handoff.to_markdown())?;
        return Ok(0);
    }

    let state = StateDir::resolve(env)?;
    let session = args.session_id.clone().unwrap_or_else(|| "unknown".to_string());
    let path = store(&state, repo, &session, &handoff)?;

    let _ = log::append(
        &state,
        &log::Decision {
            ts: now_secs(),
            session: &session,
            verb: "handoff",
            verdict: "n/a",
            score: 0,
            action: source,
            detail: &path.display().to_string(),
        },
    );

    writeln!(w, "{}", path.display())?;
    Ok(0)
}

pub fn run<W: Write>(args: &HandoffArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &repo, &env)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test ctx::handoff -- --test-threads=1 2>&1 | tail -20`
Expected: PASS, 22 tests.

- [ ] **Step 5: Commit**

```bash
git add src/commands/ctx/handoff.rs
git commit -m "feat(ctx): zirv ctx handoff verb with per-repo storage"
```

---

### Task A20: `zirv ctx resume`

**Files:**
- Modify: `src/commands/ctx/resume.rs`

**Interfaces:**
- Consumes: `latest_for_repo`, `Handoff` (A19); `adapters::select`, `AgentAdapter::interactive_cmd` (A6/A8); `StateDir` (A3).
- Produces:
  - `pub struct ResumeArgs { pub agent: Option<String>, pub print_prompt: bool, pub extra: Vec<String> }`
  - `pub fn resume_prompt(handoff: &Handoff) -> String`
  - `pub fn run<W: Write>(args: &ResumeArgs, w: &mut W) -> CtxResult<i32>` and `run_with(..., repo, env)`

On unix the fresh agent **replaces** this process (`CommandExt::exec`) so the TUI owns the terminal with no zirv in the middle; `resume` is not a supervisor. On Windows it spawns and waits.

- [ ] **Step 1: Write the failing test**

Bottom of `src/commands/ctx/resume.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::handoff::Handoff;
    use crate::commands::ctx::state::{STATE_ENV, StateDir};

    fn handoff() -> Handoff {
        Handoff {
            task: "Wire the payments webhook".to_string(),
            done: vec!["Added the route".to_string()],
            remaining: vec!["Signature verification".to_string()],
            next_step: "Add a failing test for an invalid signature".to_string(),
            files_touched: vec!["src/routes/webhook.rs".to_string()],
            gotchas: vec![],
        }
    }

    #[test]
    fn the_prompt_frames_the_handoff_as_continuation_work() {
        let prompt = resume_prompt(&handoff());
        assert!(prompt.contains("Wire the payments webhook"));
        assert!(prompt.contains("Add a failing test"));
        assert!(prompt.contains("src/routes/webhook.rs"));
        assert!(
            prompt.to_lowercase().contains("previous session"),
            "say where this came from: {prompt}"
        );
        assert!(!prompt.contains('\u{2014}'));
    }

    #[test]
    fn print_prompt_shows_the_prompt_without_launching_anything() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        crate::commands::ctx::handoff::store(&state, tmp.path(), "sess", &handoff()).expect("store");

        let env: std::collections::HashMap<String, String> = [
            (STATE_ENV.to_string(), state.root().display().to_string()),
            ("ZIRV_CTX_AGENT_BIN".to_string(), "/nonexistent/agent".to_string()),
        ]
        .into();

        let args = ResumeArgs {
            agent: None,
            print_prompt: true,
            extra: Vec::new(),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned()).expect("runs");
        assert_eq!(code, 0);
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("Wire the payments webhook"), "got {text}");
    }

    #[test]
    fn a_repo_with_no_handoff_reports_that_clearly() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let env: std::collections::HashMap<String, String> = [(
            STATE_ENV.to_string(),
            tmp.path().join("state").display().to_string(),
        )]
        .into();

        let args = ResumeArgs {
            agent: None,
            print_prompt: true,
            extra: Vec::new(),
        };
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("nothing to resume");
        let msg = err.to_string();
        assert!(msg.contains("no handoff"), "got {msg}");
        assert!(msg.contains("zirv ctx handoff"), "point at the fix: {msg}");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ctx::resume 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find function resume_prompt`.

- [ ] **Step 3: Write minimal implementation**

Replace the stub in `src/commands/ctx/resume.rs`:

```rust
use std::io::Write;
use std::path::Path;

use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::handoff::{Handoff, latest_for_repo};
use super::state::StateDir;
use super::{CtxResult, adapters};

#[derive(Debug, clap::Args)]
pub struct ResumeArgs {
    /// Adapter name: claude or codex.
    #[arg(long)]
    pub agent: Option<String>,
    /// Print the composed initial prompt instead of launching the agent.
    #[arg(long, default_value_t = false)]
    pub print_prompt: bool,
    /// Extra arguments passed through to the agent.
    #[arg(long)]
    pub extra: Vec<String>,
}

pub fn resume_prompt(handoff: &Handoff) -> String {
    format!(
        "You are picking up work from a previous session that ran out of usable context. \
Continue from the handoff below. Re-read the listed files before changing them, and do not \
redo work marked as done.\n\n{}",
        handoff.to_markdown()
    )
}

pub fn run_with<W: Write>(
    args: &ResumeArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load(repo, env)?;
    let state = StateDir::resolve(env)?;

    let (path, handoff) = latest_for_repo(&state, repo)?.ok_or_else(|| {
        format!(
            "no handoff stored for {}; run `zirv ctx handoff --transcript <path>` first",
            repo.display()
        )
    })?;

    let prompt = resume_prompt(&handoff);
    if args.print_prompt {
        writeln!(w, "{prompt}")?;
        return Ok(0);
    }

    let adapter = adapters::select(
        args.agent.as_deref().or(cfg.agent.as_deref()),
        &[],
        cfg.agent_bin.as_deref(),
    )?;
    let mut command = adapter.interactive_cmd(Some(&prompt), &args.extra);
    command.current_dir(repo);
    writeln!(w, "resuming from {}", path.display())?;
    w.flush()?;

    // Replace this process so the TUI owns the terminal directly: resume hands
    // over, it does not supervise.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = command.exec();
        Err(format!("could not start {}: {err}", adapter.name()).into())
    }
    #[cfg(not(unix))]
    {
        let status = command.status()?;
        Ok(status.code().unwrap_or(1))
    }
}

pub fn run<W: Write>(args: &ResumeArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &repo, &env)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test ctx::resume 2>&1 | tail -20`
Expected: PASS, 3 tests.

- [ ] **Step 5: Check lints on both branches**

Run: `cargo clippy --all-targets -- -D warnings 2>&1 | tail -20`
Expected: no warnings. The unix branch ends in `Err(...)` after `exec`, which never returns on success, so no unreachable-code warning should fire.

- [ ] **Step 6: Commit**

```bash
git add src/commands/ctx/resume.rs
git commit -m "feat(ctx): zirv ctx resume launches a clean session from the latest handoff"
```

---

### Task A21: `zirv ctx status`

**Files:**
- Modify: `src/commands/ctx/status.rs`

**Interfaces:**
- Consumes: `StateDir` (A3), `log::tail` (A3), `latest_for_repo` (A19).
- Produces: `pub struct StatusArgs { pub decisions: usize }` and `pub fn run<W: Write>(args: &StatusArgs, w: &mut W) -> CtxResult<i32>` plus `run_with(args, w, repo, env)`.

Follows the `show_help<W: Write>` pattern so the output is asserted directly.

- [ ] **Step 1: Write the failing test**

Bottom of `src/commands/ctx/status.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::handoff::Handoff;
    use crate::commands::ctx::state::{STATE_ENV, StateDir};
    use crate::commands::ctx::{log, signal};

    fn env_for(state: &std::path::Path) -> std::collections::HashMap<String, String> {
        [(STATE_ENV.to_string(), state.display().to_string())].into()
    }

    #[test]
    fn an_empty_state_dir_reports_nothing_supervised() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let env = env_for(&state);

        let mut out = Vec::new();
        let code = run_with(&StatusArgs { decisions: 10 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        assert_eq!(code, 0);

        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains(&state.display().to_string()), "name the state dir: {text}");
        assert!(text.contains("no supervised sessions"), "got {text}");
        assert!(text.contains("no handoff"), "got {text}");
    }

    #[test]
    fn it_lists_sockets_decisions_and_the_latest_handoff() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());

        log::append(
            &state,
            &log::Decision {
                ts: 1_700_000_000,
                session: "11111111-2222",
                verb: "wrap",
                verdict: "compact",
                score: 64,
                action: "inject",
                detail: "cooldown armed",
            },
        )
        .expect("append");

        crate::commands::ctx::handoff::store(
            &state,
            tmp.path(),
            "11111111-2222",
            &Handoff {
                task: "Wire the webhook".to_string(),
                next_step: "Write the test".to_string(),
                ..Handoff::default()
            },
        )
        .expect("store");

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 10 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");

        assert!(text.contains("compact"), "verdict in the decision list: {text}");
        assert!(text.contains("inject"));
        assert!(text.contains("Wire the webhook"), "latest handoff task: {text}");
    }

    #[cfg(unix)]
    #[test]
    fn a_live_socket_shows_up_as_a_supervised_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());

        let _server = signal::SignalServer::bind(&state.socket_for("abcdef12-3456")).expect("bind");

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("abcdef12"), "session prefix listed: {text}");
        assert!(!text.contains("no supervised sessions"));
    }

    #[test]
    fn the_decision_limit_is_honored() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());

        for i in 0..5 {
            log::append(
                &state,
                &log::Decision {
                    ts: 1_700_000_000 + i,
                    session: "s",
                    verb: "exec",
                    verdict: "healthy",
                    score: 0,
                    action: &format!("tick{i}"),
                    detail: "",
                },
            )
            .expect("append");
        }

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 2 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("tick4"));
        assert!(text.contains("tick3"));
        assert!(!text.contains("tick0"));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ctx::status 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find function run_with`.

- [ ] **Step 3: Write minimal implementation**

Replace the stub in `src/commands/ctx/status.rs`:

```rust
use std::io::Write;
use std::path::Path;

use super::config::{EnvLookup, env_from_process};
use super::handoff::latest_for_repo;
use super::state::StateDir;
use super::{CtxResult, log};

#[derive(Debug, clap::Args)]
pub struct StatusArgs {
    /// How many recent supervisor decisions to show.
    #[arg(long, default_value_t = 10)]
    pub decisions: usize,
}

pub fn run_with<W: Write>(
    args: &StatusArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let state = StateDir::resolve(env)?;
    writeln!(w, "state dir: {}", state.root().display())?;

    let mut sessions: Vec<String> = std::fs::read_dir(state.sockets())
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("sock"))
                .filter_map(|e| e.path().file_stem().map(|s| s.to_string_lossy().to_string()))
                .collect()
        })
        .unwrap_or_default();
    sessions.sort();

    writeln!(w, "\nsupervised sessions:")?;
    if sessions.is_empty() {
        writeln!(w, "  no supervised sessions")?;
    } else {
        for session in &sessions {
            writeln!(w, "  {session}")?;
        }
    }

    writeln!(w, "\nlatest handoff for {}:", repo.display())?;
    match latest_for_repo(&state, repo)? {
        Some((path, handoff)) => {
            writeln!(w, "  {}", path.display())?;
            writeln!(w, "  task: {}", handoff.task)?;
            writeln!(w, "  next: {}", handoff.next_step)?;
        }
        None => writeln!(w, "  no handoff stored")?,
    }

    writeln!(w, "\nrecent decisions:")?;
    let lines = log::tail(&state, args.decisions)?;
    if lines.is_empty() {
        writeln!(w, "  none recorded")?;
    } else {
        for line in lines.iter().rev() {
            writeln!(w, "  {line}")?;
        }
    }

    Ok(0)
}

pub fn run<W: Write>(args: &StatusArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &repo, &env)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test ctx::status 2>&1 | tail -20`
Expected: PASS, 4 tests.

- [ ] **Step 5: Run the whole suite and the lints**

Run: `cargo test --verbose -- --test-threads=1 2>&1 | tail -15`
Expected: PASS.
Run: `cargo fmt -- --check && cargo clippy --all-targets -- -D warnings 2>&1 | tail -10`
Expected: clean.

- [ ] **Step 6: Verify Phase A end to end by hand**

Run:

```bash
T=$(ls -t ~/.claude/projects/*/*.jsonl | head -1)
cargo run --quiet -- ctx score --transcript "$T"
cargo run --quiet -- ctx handoff --transcript "$T" --no-model --stdout | head -12
cargo run --quiet -- ctx status
```

Expected: a JSON score line, a markdown handoff with all six sections, and a status report naming the state dir.

- [ ] **Step 7: Commit**

```bash
git add src/commands/ctx/status.rs
git commit -m "feat(ctx): zirv ctx status report"
```

---

# Phase B: Headless supervisors

Ships `zirv ctx exec` and `zirv ctx loop`. Depends on all of Phase A.

### Task B1: Fake agent binary and supervision primitives

**Files:**
- Create: `tests/fixtures/fake-agent.sh`
- Modify: `src/commands/ctx/supervise.rs`

**Interfaces:**
- Consumes: `CtxResult` (A1).
- Produces:
  - `pub enum Tick { Continue, Stop(&'static str) }`
  - `pub enum Outcome { Exited(i32), TimedOut, StoppedByTick(&'static str) }`
  - `pub fn spawn(command: std::process::Command) -> CtxResult<std::process::Child>`
  - `pub fn supervise_child(child: &mut std::process::Child, deadline: std::time::Instant, poll: std::time::Duration, on_tick: &mut dyn FnMut() -> Tick) -> CtxResult<Outcome>`
  - `pub fn terminate(child: &mut std::process::Child, grace: std::time::Duration) -> CtxResult<()>`
  - `pub struct Watcher` with `pub fn new(path: PathBuf) -> Self`, `pub fn read_if_changed(&mut self) -> CtxResult<Option<String>>`, `pub fn path(&self) -> &Path`
  - `pub fn run_shell(command: &str, cwd: &Path) -> CtxResult<i32>` for `on_failure` hooks

- [ ] **Step 1: Write the fake agent script**

Create `tests/fixtures/fake-agent.sh` and `chmod +x` it:

```sh
#!/bin/sh
# Stands in for a headless agent. Writes a claude-format transcript to exactly
# the path the claude adapter computes, so the tests exercise real path
# derivation instead of a test-only shortcut.
#
# Invoked through the adapter as:
#   fake-agent.sh -p <prompt> --session-id <uuid> [extra...]
#
# Behavior comes from the environment:
#   FAKE_AGENT_MODE=healthy|rot|hang|fail   (default healthy)
#   FAKE_AGENT_MODE_FILE=<path>             one mode per line, popped per run
#   FAKE_AGENT_TURNS=<n>                    (default 12)
#   FAKE_AGENT_SLEEP=<secs>                 rot mode only (default 0)
#
#   healthy  distinct tool inputs, marker on every final, 20k tokens
#   rot      identical tool input, every result an error, marker only on the
#            first two turns, 170k tokens: score 100, verdict restart
#   hang     writes a healthy transcript then never exits
#   fail     writes a healthy transcript then exits 3
#
# FAKE_AGENT_MODE_FILE lets one test script a sequence across restarts, for
# example "rot" then "healthy" to prove a restarted child is supervised on its
# own transcript. FAKE_AGENT_SLEEP applies only in rot mode, so a rotted run
# stays alive long enough to be scored while a healthy one exits promptly.
set -eu

session=""
while [ $# -gt 0 ]; do
  case "$1" in
    --session-id) session="${2:-}"; shift 2 ;;
    *) shift ;;
  esac
done
[ -n "$session" ] || { echo "fake-agent: no --session-id given" >&2; exit 64; }

mode="${FAKE_AGENT_MODE:-healthy}"
if [ -n "${FAKE_AGENT_MODE_FILE:-}" ] && [ -s "${FAKE_AGENT_MODE_FILE}" ]; then
  mode=$(head -n 1 "$FAKE_AGENT_MODE_FILE")
  tail -n +2 "$FAKE_AGENT_MODE_FILE" > "$FAKE_AGENT_MODE_FILE.next"
  mv "$FAKE_AGENT_MODE_FILE.next" "$FAKE_AGENT_MODE_FILE"
fi
turns="${FAKE_AGENT_TURNS:-12}"

slug=$(printf '%s' "$(pwd)" | tr -c 'A-Za-z0-9-' '-')
dir="$HOME/.claude/projects/$slug"
mkdir -p "$dir"
t="$dir/$session.jsonl"
: > "$t"

emit_turn() { # $1 tool input  $2 is_error  $3 final text  $4 tokens
  printf '{"type":"user","message":{"content":"do the thing"}}\n' >> "$t"
  printf '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t","name":"Bash","input":%s}],"usage":{"input_tokens":2,"cache_read_input_tokens":%s}}}\n' "$1" "$4" >> "$t"
  printf '{"type":"user","message":{"content":[{"type":"tool_result","content":"out","is_error":%s}]}}\n' "$2" >> "$t"
  printf '{"type":"assistant","message":{"content":[{"type":"text","text":"%s"}],"usage":{"input_tokens":2,"cache_read_input_tokens":%s}}}\n' "$3" "$4" >> "$t"
}

i=1
while [ "$i" -le "$turns" ]; do
  if [ "$mode" = "rot" ]; then
    if [ "$i" -le 2 ]; then final="[zirv] step $i"; else final="step $i"; fi
    emit_turn '{"command":"ls"}' true "$final" 170000
  else
    emit_turn "{\"command\":\"ls $i\"}" false "[zirv] step $i" 20000
  fi
  i=$((i + 1))
done

sleep_secs="${FAKE_AGENT_SLEEP:-0}"
if [ "$mode" = "rot" ] && [ "$sleep_secs" != "0" ]; then
  sleep "$sleep_secs"
fi

case "$mode" in
  hang) while true; do sleep 1; done ;;
  fail) exit 3 ;;
  *) exit 0 ;;
esac
```

- [ ] **Step 2: Verify the fixture by hand**

Run:

```bash
chmod +x tests/fixtures/fake-agent.sh
cd "$(mktemp -d)" && HOME="$PWD/home" FAKE_AGENT_MODE=rot \
  "$OLDPWD/tests/fixtures/fake-agent.sh" -p x --session-id 11111111-2222-4333-8444-555555555555
find "$PWD/home" -name '*.jsonl'
```

Expected: one transcript path printed under a slug of the temp cwd.

Run (from the repo root, substituting the path just printed):

```bash
cargo run --quiet -- ctx score --transcript <THAT_PATH>
```

Expected: `"verdict":"restart"` and `"score":100`.

- [ ] **Step 3: Write the failing test**

Bottom of `src/commands/ctx/supervise.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        cmd
    }

    #[test]
    fn a_clean_exit_is_reported_with_its_code() {
        let mut child = spawn(sh("exit 0")).expect("spawn");
        let outcome = supervise_child(
            &mut child,
            Instant::now() + Duration::from_secs(10),
            Duration::from_millis(20),
            &mut || Tick::Continue,
        )
        .expect("supervise");
        assert_eq!(outcome, Outcome::Exited(0));
    }

    #[test]
    fn a_failing_exit_code_is_preserved() {
        let mut child = spawn(sh("exit 7")).expect("spawn");
        let outcome = supervise_child(
            &mut child,
            Instant::now() + Duration::from_secs(10),
            Duration::from_millis(20),
            &mut || Tick::Continue,
        )
        .expect("supervise");
        assert_eq!(outcome, Outcome::Exited(7));
    }

    #[test]
    fn a_deadline_kills_the_child_and_reports_a_timeout() {
        let mut child = spawn(sh("sleep 30")).expect("spawn");
        let started = Instant::now();
        let outcome = supervise_child(
            &mut child,
            Instant::now() + Duration::from_millis(200),
            Duration::from_millis(20),
            &mut || Tick::Continue,
        )
        .expect("supervise");
        assert_eq!(outcome, Outcome::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10), "must not wait for the child");
        assert!(child.try_wait().expect("try_wait").is_some(), "child was reaped");
    }

    #[test]
    fn a_tick_can_stop_the_run_and_name_its_reason() {
        let mut child = spawn(sh("sleep 30")).expect("spawn");
        let mut ticks = 0;
        let outcome = supervise_child(
            &mut child,
            Instant::now() + Duration::from_secs(30),
            Duration::from_millis(20),
            &mut || {
                ticks += 1;
                if ticks >= 2 { Tick::Stop("rot") } else { Tick::Continue }
            },
        )
        .expect("supervise");
        assert_eq!(outcome, Outcome::StoppedByTick("rot"));
        assert!(ticks >= 2);
    }

    #[test]
    fn ticks_fire_at_the_poll_interval() {
        let mut child = spawn(sh("sleep 0.5")).expect("spawn");
        let mut ticks = 0;
        supervise_child(
            &mut child,
            Instant::now() + Duration::from_secs(30),
            Duration::from_millis(50),
            &mut || {
                ticks += 1;
                Tick::Continue
            },
        )
        .expect("supervise");
        assert!(ticks >= 3, "expected several ticks in 500ms, got {ticks}");
    }

    #[test]
    fn terminate_stops_a_child_that_ignores_sigterm() {
        let mut child = spawn(sh("trap '' TERM; sleep 30")).expect("spawn");
        let started = Instant::now();
        terminate(&mut child, Duration::from_millis(150)).expect("terminate");
        assert!(child.try_wait().expect("try_wait").is_some(), "child is gone");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn terminate_is_safe_on_an_already_dead_child() {
        let mut child = spawn(sh("exit 0")).expect("spawn");
        let _ = child.wait();
        terminate(&mut child, Duration::from_millis(50)).expect("terminate must be idempotent");
    }

    #[test]
    fn the_watcher_reports_content_only_when_it_changed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.jsonl");
        let mut watcher = Watcher::new(path.clone());

        assert_eq!(watcher.read_if_changed().expect("missing file is not an error"), None);

        std::fs::write(&path, "line one\n").expect("write");
        assert_eq!(watcher.read_if_changed().expect("read"), Some("line one\n".to_string()));
        assert_eq!(watcher.read_if_changed().expect("read"), None, "unchanged");

        std::fs::write(&path, "line one\nline two\n").expect("append");
        assert_eq!(
            watcher.read_if_changed().expect("read"),
            Some("line one\nline two\n".to_string()),
            "the whole file, since scoring needs the full history"
        );
    }

    #[test]
    fn run_shell_reports_the_exit_code() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(run_shell("exit 0", dir.path()).expect("run"), 0);
        assert_eq!(run_shell("exit 9", dir.path()).expect("run"), 9);
    }

    #[test]
    fn run_shell_runs_in_the_given_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        run_shell("touch marker", dir.path()).expect("run");
        assert!(dir.path().join("marker").exists());
    }
}
```

- [ ] **Step 4: Run tests to verify they fail**

Run: `cargo test ctx::supervise 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find function spawn`.

- [ ] **Step 5: Write minimal implementation**

Replace the placeholder in `src/commands/ctx/supervise.rs`:

```rust
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::CtxResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    Continue,
    Stop(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Exited(i32),
    TimedOut,
    StoppedByTick(&'static str),
}

pub fn spawn(mut command: Command) -> CtxResult<Child> {
    Ok(command
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?)
}

/// Polls the child, calling `on_tick` at every interval. Stops on child exit,
/// on the deadline, or when a tick asks to stop; kills the child in the last two
/// cases so no supervisor ever leaks a process.
pub fn supervise_child(
    child: &mut Child,
    deadline: Instant,
    poll: Duration,
    on_tick: &mut dyn FnMut() -> Tick,
) -> CtxResult<Outcome> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Outcome::Exited(status.code().unwrap_or(1)));
        }
        if Instant::now() >= deadline {
            terminate(child, Duration::from_secs(5))?;
            return Ok(Outcome::TimedOut);
        }
        if let Tick::Stop(reason) = on_tick() {
            terminate(child, Duration::from_secs(5))?;
            return Ok(Outcome::StoppedByTick(reason));
        }
        std::thread::sleep(poll);
    }
}

/// SIGTERM, then SIGKILL after the grace period. Safe to call on a child that
/// already exited.
pub fn terminate(child: &mut Child, grace: Duration) -> CtxResult<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }

    #[cfg(unix)]
    {
        // SAFETY: `kill` with a pid this process owns and a valid signal number.
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }

    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

/// Polls a growing transcript. Returns the whole file whenever its length
/// changed, because scoring needs the full turn history, not just the delta.
pub struct Watcher {
    path: PathBuf,
    len: u64,
}

impl Watcher {
    pub fn new(path: PathBuf) -> Self {
        Self { path, len: 0 }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read_if_changed(&mut self) -> CtxResult<Option<String>> {
        let Ok(meta) = std::fs::metadata(&self.path) else {
            return Ok(None);
        };
        if meta.len() == self.len {
            return Ok(None);
        }
        self.len = meta.len();
        Ok(Some(std::fs::read_to_string(&self.path)?))
    }
}

/// Runs an `on_failure` hook the way the script runner runs commands.
pub fn run_shell(command: &str, cwd: &Path) -> CtxResult<i32> {
    let mut shell = if cfg!(windows) {
        let mut c = Command::new("powershell");
        c.arg("-Command").arg(command);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    };
    let status = shell.current_dir(cwd).status()?;
    Ok(status.code().unwrap_or(1))
}
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test ctx::supervise 2>&1 | tail -20`
Expected: PASS, 10 tests.

- [ ] **Step 7: Commit**

```bash
git add tests/fixtures/fake-agent.sh src/commands/ctx/supervise.rs
git commit -m "feat(ctx): supervision primitives and a fake agent fixture"
```

---

### Task B2: `zirv ctx exec` restart ladder

**Files:**
- Modify: `src/commands/ctx/exec.rs`

**Interfaces:**
- Consumes: `supervise` primitives (B1); `score_transcript` (A13); `distill_or_structural`, `store` (A18/A19); `adapters::select` (A6); `CtxConfig` (A2); `log` (A3).
- Produces:
  - `pub struct ExecArgs { pub agent: Option<String>, pub session_id: Option<String>, pub transcript: Option<PathBuf>, pub prompt: Option<String>, pub max_restarts: Option<u32>, pub timeout_secs: Option<u64>, pub command: Vec<String> }`
  - `pub fn extract_prompt(command: &[String]) -> Option<String>`
  - `pub const EXIT_ROT_EXHAUSTED: i32 = 75;` and `pub const EXIT_TIMEOUT: i32 = 76;`
  - `pub fn run<W: Write>(args: &ExecArgs, w: &mut W) -> CtxResult<i32>` and `run_with(args, w, repo, env)`

Exit codes are a contract with callers: the child's own code on a natural exit, `75` when the restart budget is exhausted after rot, `76` on a wall-clock timeout with no restarts left.

- [ ] **Step 1: Write the failing test**

Bottom of `src/commands/ctx/exec.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn fixture(name: &str) -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    /// Runs the fake agent directly, so `exec` supervises a real child whose
    /// transcript path we control through `--transcript`.
    fn fake_agent_command(session: &str) -> Vec<String> {
        vec![
            "sh".to_string(),
            fixture("fake-agent.sh").display().to_string(),
            "-p".to_string(),
            "do the work".to_string(),
            "--session-id".to_string(),
            session.to_string(),
        ]
    }

    fn base_env(state: &std::path::Path) -> HashMap<String, String> {
        [
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state.display().to_string(),
            ),
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                fixture("fake-agent.sh").display().to_string(),
            ),
        ]
        .into()
    }

    fn transcript_for(home: &std::path::Path, repo: &std::path::Path, session: &str) -> PathBuf {
        home.join(".claude/projects")
            .join(crate::commands::ctx::adapters::claude::project_slug(repo))
            .join(format!("{session}.jsonl"))
    }

    #[test]
    fn prompt_extraction_finds_the_dash_p_argument() {
        let cmd = vec![
            "claude".to_string(),
            "-p".to_string(),
            "fix the bug".to_string(),
            "--session-id".to_string(),
            "x".to_string(),
        ];
        assert_eq!(extract_prompt(&cmd), Some("fix the bug".to_string()));
    }

    #[test]
    fn prompt_extraction_handles_print_and_positional_forms() {
        assert_eq!(
            extract_prompt(&["claude".to_string(), "--print".to_string(), "go".to_string()]),
            Some("go".to_string())
        );
        assert_eq!(
            extract_prompt(&["codex".to_string(), "exec".to_string(), "go".to_string()]),
            Some("go".to_string())
        );
    }

    #[test]
    fn prompt_extraction_gives_up_rather_than_guessing() {
        assert_eq!(extract_prompt(&["claude".to_string(), "-p".to_string()]), None);
        assert_eq!(
            extract_prompt(&["claude".to_string(), "--resume".to_string(), "abc".to_string()]),
            None
        );
        assert_eq!(extract_prompt(&[]), None);
    }

    #[test]
    fn a_healthy_run_exits_with_the_childs_own_code() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let session = "11111111-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        // SAFETY: CI runs tests single-threaded.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(2),
            timeout_secs: Some(60),
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), 0);
    }

    #[test]
    fn a_failing_child_propagates_its_exit_code() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let session = "22222222-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("FAKE_AGENT_MODE", "fail");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(60),
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 3);
    }

    /// `FAKE_AGENT_MODE` applies to every invocation, so both the original child
    /// and the restarted one rot and the budget runs out.
    #[test]
    fn a_rotted_run_is_killed_restarted_and_capped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "33333333-2222-4333-8444-555555555555";
        let env = base_env(&state);

        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("FAKE_AGENT_MODE", "rot");
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            timeout_secs: Some(60),
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
        }

        assert_eq!(
            code.expect("runs"),
            EXIT_ROT_EXHAUSTED,
            "the caller applies its own policy after the budget is spent"
        );

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"verb\":\"exec\""), "got {log}");
        assert!(log.contains("\"action\":\"restart\""), "a restart was attempted: {log}");
        assert!(log.contains("\"action\":\"give-up\""), "and then it stopped: {log}");

        let handoffs = state.join("handoffs");
        let stored: Vec<_> = walk_md(&handoffs);
        assert!(!stored.is_empty(), "a handoff is written before each restart");
    }

    fn walk_md(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return found;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(walk_md(&path));
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                found.push(path);
            }
        }
        found
    }

    fn transcripts_in(home: &std::path::Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(dirs) = std::fs::read_dir(home.join(".claude/projects")) else {
            return found;
        };
        for dir in dirs.flatten() {
            let Ok(files) = std::fs::read_dir(dir.path()) else { continue };
            for file in files.flatten() {
                if file.path().extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    found.push(file.path());
                }
            }
        }
        found
    }

    /// The restarted child is a new session writing to a new transcript, so
    /// supervision must follow it there. If the watcher kept polling the killed
    /// child's rotted file, this healthy second child would be killed too and
    /// the run would exit 75 instead of 0.
    #[test]
    fn a_restart_supervises_the_new_sessions_transcript() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let session = "88888888-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        // First child rots, second is healthy.
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "rot\nhealthy\n").expect("write modes");

        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
            std::env::set_var("FAKE_AGENT_TURNS", "12");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            timeout_secs: Some(60),
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
            std::env::remove_var("FAKE_AGENT_TURNS");
        }

        assert_eq!(
            code.expect("runs"),
            0,
            "the healthy restarted child must be allowed to finish"
        );

        let found = transcripts_in(&home);
        assert_eq!(found.len(), 2, "one transcript per session: {found:?}");
        let first = transcript_for(&home, tmp.path(), session);
        assert!(
            found.iter().any(|p| *p == first),
            "the original session's transcript: {found:?}"
        );
        assert!(
            found.iter().any(|p| *p != first),
            "the restarted session wrote its own transcript: {found:?}"
        );
    }

    #[test]
    fn a_run_with_no_discoverable_prompt_refuses_to_restart() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let session = "44444444-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("FAKE_AGENT_MODE", "rot");
            // Keep the child alive past the first scoring tick so rot is seen.
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
        }
        let mut command = fake_agent_command(session);
        command.retain(|a| a != "-p" && a != "do the work");
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: None,
            max_restarts: Some(2),
            timeout_secs: Some(60),
            command,
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
        }

        assert_eq!(
            code.expect("runs"),
            EXIT_ROT_EXHAUSTED,
            "rot was detected but no restart was possible"
        );
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("cannot restart"),
            "say why supervision stood down: {text}"
        );
    }

    #[test]
    fn an_empty_command_is_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let env = base_env(&tmp.path().join("state"));
        let args = ExecArgs {
            agent: None,
            session_id: None,
            transcript: None,
            prompt: None,
            max_restarts: None,
            timeout_secs: None,
            command: Vec::new(),
        };
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("nothing to supervise");
        assert!(err.to_string().contains("command"), "got {err}");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ctx::exec -- --test-threads=1 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find function extract_prompt`.

- [ ] **Step 3: Write minimal implementation**

Replace the stub in `src/commands/ctx/exec.rs`:

```rust
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::event::{SessionId, SessionRef};
use super::rot::Verdict;
use super::state::{StateDir, now_secs};
use super::supervise::{self, Outcome, Tick, Watcher};
use super::{CtxResult, adapters, handoff, log, score};

/// The restart budget is spent and the session is still rotting. Callers apply
/// their own policy from here.
pub const EXIT_ROT_EXHAUSTED: i32 = 75;
/// Wall-clock timeout with no restarts left.
pub const EXIT_TIMEOUT: i32 = 76;

#[derive(Debug, clap::Args)]
pub struct ExecArgs {
    /// Adapter name: claude or codex. Detected from the command when omitted.
    #[arg(long)]
    pub agent: Option<String>,
    /// Session id of the supervised run, used to locate its transcript.
    #[arg(long)]
    pub session_id: Option<String>,
    /// Transcript path, when the agent writes somewhere the adapter cannot derive.
    #[arg(long)]
    pub transcript: Option<PathBuf>,
    /// Prompt to reuse on restart. Extracted from the command when omitted.
    #[arg(long)]
    pub prompt: Option<String>,
    /// Restart budget before giving up.
    #[arg(long)]
    pub max_restarts: Option<u32>,
    /// Wall-clock limit for the whole supervised run.
    #[arg(long)]
    pub timeout_secs: Option<u64>,
    /// The headless agent command, after `--`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, last = true)]
    pub command: Vec<String>,
}

/// Finds the prompt in a headless agent command. Returns `None` rather than
/// guessing: a restart with the wrong prompt is worse than no restart.
pub fn extract_prompt(command: &[String]) -> Option<String> {
    let mut iter = command.iter().enumerate().skip(1);
    while let Some((index, arg)) = iter.next() {
        let is_prompt_flag = arg == "-p" || arg == "--print";
        let is_subcommand = arg == "exec";
        if !is_prompt_flag && !is_subcommand {
            continue;
        }
        let next = command.get(index + 1)?;
        if next.starts_with('-') {
            return None;
        }
        return Some(next.clone());
    }
    None
}

fn build_command(command: &[String], repo: &Path) -> CtxResult<Command> {
    let (program, rest) = command.split_first().ok_or("no command to supervise; pass it after --")?;
    let mut cmd = Command::new(program);
    cmd.args(rest).current_dir(repo);
    Ok(cmd)
}

pub fn run_with<W: Write>(
    args: &ExecArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load(repo, env)?;
    let adapter = adapters::select(
        args.agent.as_deref().or(cfg.agent.as_deref()),
        &args.command,
        cfg.agent_bin.as_deref(),
    )?;
    let state = StateDir::resolve(env)?;

    let session_raw = args
        .session_id
        .clone()
        .unwrap_or_else(|| SessionId::new_v4().to_string());
    let mut session = SessionId::parse(&session_raw);

    let derive_transcript = |session: &SessionId| {
        adapter.transcript_path(&SessionRef {
            id: session.clone(),
            cwd: repo.to_path_buf(),
        })
    };

    // `--transcript` describes the caller's own first child only. Every restart
    // is a new session launched by the adapter, so its transcript path has to be
    // derived again or the watcher would keep polling the dead child's file.
    let mut transcript = args
        .transcript
        .clone()
        .unwrap_or_else(|| derive_transcript(&session));

    let prompt = args.prompt.clone().or_else(|| extract_prompt(&args.command));
    let max_restarts = args.max_restarts.unwrap_or(cfg.supervise.max_restarts);
    let timeout = Duration::from_secs(args.timeout_secs.unwrap_or(cfg.supervise.max_cycle_secs));
    let poll = Duration::from_millis(cfg.supervise.poll_ms);

    let mut command = build_command(&args.command, repo)?;
    let mut restarts = 0;

    loop {
        let mut child = supervise::spawn(command)?;
        // Fresh watcher per iteration, over the current session's transcript.
        let mut watcher = Watcher::new(transcript.clone());
        let mut rotted = false;

        let outcome = supervise_run(
            &mut child,
            Instant::now() + timeout,
            poll,
            &mut watcher,
            &transcript,
            args.agent.as_deref().or(cfg.agent.as_deref()),
            repo,
            env,
            &mut rotted,
        )?;

        match outcome {
            Outcome::Exited(code) => return Ok(code),
            Outcome::TimedOut | Outcome::StoppedByTick(_) => {}
        }

        let reason = if rotted { "rot" } else { "timeout" };
        let exhausted_code = if rotted { EXIT_ROT_EXHAUSTED } else { EXIT_TIMEOUT };

        let Some(prompt_text) = prompt.clone() else {
            writeln!(
                w,
                "zirv ctx exec: {reason} detected but the original prompt is unknown, so it cannot restart. Pass --prompt to enable restarts."
            )?;
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: reason,
                    score: 0,
                    action: "stand-down",
                    detail: "no prompt available for restart",
                },
            );
            return Ok(exhausted_code);
        };

        if restarts >= max_restarts {
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: reason,
                    score: 0,
                    action: "give-up",
                    detail: "restart budget exhausted",
                },
            );
            writeln!(
                w,
                "zirv ctx exec: {reason} after {restarts} restarts, giving up with exit {exhausted_code}"
            )?;
            return Ok(exhausted_code);
        }

        let jsonl = std::fs::read_to_string(&transcript).unwrap_or_default();
        let ctx = adapter.structural_context(&jsonl, cfg.handoff.tail_items);
        let (note, source) =
            handoff::distill_or_structural(adapter.as_ref(), &cfg.handoff.model, &ctx);
        let stored = handoff::store(&state, repo, session.as_str(), &note)?;

        restarts += 1;
        let _ = log::append(
            &state,
            &log::Decision {
                ts: now_secs(),
                session: session.as_str(),
                verb: "exec",
                verdict: reason,
                score: 0,
                action: "restart",
                detail: &format!("{source} handoff at {}", stored.display()),
            },
        );
        writeln!(
            w,
            "zirv ctx exec: {reason} detected, restarting ({restarts}/{max_restarts}) with a {source} handoff"
        )?;

        session = SessionId::new_v4();
        // The new session writes somewhere new, so the next iteration's watcher
        // must follow it rather than the file the killed child left behind.
        transcript = derive_transcript(&session);
        let combined = format!("{prompt_text}\n\n{}", note.to_markdown());
        command = adapter.headless_cmd(&combined, &session, &[]);
        command.current_dir(repo);
    }
}

#[allow(clippy::too_many_arguments)]
fn supervise_run(
    child: &mut std::process::Child,
    deadline: Instant,
    poll: Duration,
    watcher: &mut Watcher,
    transcript: &Path,
    agent: Option<&str>,
    repo: &Path,
    env: EnvLookup<'_>,
    rotted: &mut bool,
) -> CtxResult<Outcome> {
    let mut tick = || {
        // A scoring failure must never kill a healthy run.
        match watcher.read_if_changed() {
            Ok(Some(_)) => {}
            _ => return Tick::Continue,
        }
        match score::score_transcript(transcript, agent, repo, env) {
            Ok(score) if score.verdict == Verdict::Restart => {
                *rotted = true;
                Tick::Stop("rot")
            }
            _ => Tick::Continue,
        }
    };
    supervise::supervise_child(child, deadline, poll, &mut tick)
}

pub fn run<W: Write>(args: &ExecArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &repo, &env)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test ctx::exec -- --test-threads=1 2>&1 | tail -25`
Expected: PASS, 9 tests. If `a_rotted_run_is_killed_restarted_and_capped` hangs, the fake agent's `FAKE_AGENT_SLEEP=30` is doing its job but the tick is not scoring: check that `--transcript` points at the path the script actually wrote (print it in the test) and that `poll_ms` is small enough to fire before the 60s timeout. If `a_restart_supervises_the_new_sessions_transcript` returns `75` instead of `0`, the restart is still watching the first child's transcript: confirm `transcript` is reassigned from the new session id inside the loop.

- [ ] **Step 5: Commit**

```bash
git add src/commands/ctx/exec.rs
git commit -m "feat(ctx): zirv ctx exec supervises headless runs with bounded restarts"
```

---

### Task B3: `zirv ctx exec` timeout kills and turn-signal wiring

**Files:**
- Modify: `src/commands/ctx/exec.rs`

**Interfaces:**
- Consumes: everything from B2; `signal::SignalServer`, `TurnSignal` (A14); `AgentAdapter::register_turn_signal` (A8); `StateDir::socket_for` (A3).
- Produces: `pub fn should_stop_for_signal(signal: &TurnSignal) -> bool`, and `run_with` binding a per-session socket, exporting its env into the child, and scoring immediately on a turn signal instead of waiting for the next poll.

Turn signals are an accelerator, not a requirement: if the socket cannot be bound (Windows, an over-long path, a stale directory) `exec` logs once and keeps polling.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` in `src/commands/ctx/exec.rs`:

```rust
    use crate::commands::ctx::rot::Verdict;
    use crate::commands::ctx::signal::TurnSignal;

    fn signal_with(verdict: Verdict, score: u32) -> TurnSignal {
        TurnSignal {
            session_id: "s".to_string(),
            turn: 4,
            score,
            verdict,
        }
    }

    #[test]
    fn only_a_restart_signal_stops_the_run() {
        assert!(should_stop_for_signal(&signal_with(Verdict::Restart, 95)));
        assert!(!should_stop_for_signal(&signal_with(Verdict::Compact, 65)));
        assert!(!should_stop_for_signal(&signal_with(Verdict::Advise, 45)));
        assert!(!should_stop_for_signal(&signal_with(Verdict::Healthy, 0)));
    }

    #[test]
    fn a_hanging_child_is_killed_at_the_deadline() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "55555555-2222-4333-8444-555555555555";
        let env = base_env(&state);

        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("FAKE_AGENT_MODE", "hang");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(1),
            command: fake_agent_command(session),
        };
        let started = std::time::Instant::now();
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), EXIT_TIMEOUT);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "the deadline must not wait for the child"
        );
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"verdict\":\"timeout\""), "got {log}");
    }

    #[cfg(unix)]
    #[test]
    fn the_child_is_told_where_the_socket_is() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "66666666-2222-4333-8444-555555555555";
        let env = base_env(&state);
        let marker = tmp.path().join("socket-env.txt");

        // A child that records the socket env it inherited, then exits.
        let command = vec![
            "sh".to_string(),
            "-c".to_string(),
            format!(
                "printf '%s' \"$ZIRV_CTX_SOCKET\" > {}; exit 0",
                marker.display()
            ),
        ];

        unsafe {
            std::env::set_var("HOME", &home);
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(30),
            command,
        };
        let mut out = Vec::new();
        run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned()).expect("runs");

        let seen = std::fs::read_to_string(&marker).expect("marker written");
        assert!(seen.ends_with(".sock"), "socket path exported: {seen}");
        assert!(seen.contains("66666666"), "per-session socket: {seen}");
    }

    #[test]
    fn an_unbindable_socket_does_not_stop_the_run() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let session = "77777777-2222-4333-8444-555555555555";
        // A state dir path long enough that the socket path exceeds the limit.
        let long_state = tmp.path().join("x".repeat(120));
        let mut env = base_env(&long_state);
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());

        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(30),
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 0, "polling still supervises the run");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ctx::exec -- --test-threads=1 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find function should_stop_for_signal`.

- [ ] **Step 3: Write minimal implementation**

Add to `src/commands/ctx/exec.rs`:

```rust
use super::signal::{self, TurnSignal};

/// Compaction of a headless run is pointless (there is no TUI to type into), so
/// only a restart verdict acts.
pub fn should_stop_for_signal(signal: &TurnSignal) -> bool {
    signal.verdict == Verdict::Restart
}
```

In `run_with`, before the supervision loop:

```rust
    let socket_path = state.socket_for(session.as_str());
    let server = match signal::SignalServer::bind(&socket_path) {
        Ok(server) => Some(server),
        Err(e) => {
            // Turn signals only accelerate detection; polling is the floor.
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: "n/a",
                    score: 0,
                    action: "no-socket",
                    detail: &e.to_string(),
                },
            );
            None
        }
    };
    let turn_env = server
        .as_ref()
        .map(|server| {
            adapter
                .register_turn_signal(
                    &SessionRef {
                        id: session.clone(),
                        cwd: repo.to_path_buf(),
                    },
                    server.path(),
                )
                .env
        })
        .unwrap_or_default();
```

Apply `turn_env` to every command built in the loop, both the initial one and each restart:

```rust
    let mut command = build_command(&args.command, repo)?;
    for (key, value) in &turn_env {
        command.env(key, value);
    }
```

and after the restart `command = adapter.headless_cmd(...)` line:

```rust
        command.current_dir(repo);
        for (key, value) in &turn_env {
            command.env(key, value);
        }
```

Extend `supervise_run` with the server so a signal triggers an immediate score, and log the outcome. Change its signature to take `server: Option<&signal::SignalServer>` and replace the tick body:

```rust
    let mut tick = || {
        if let Some(server) = server
            && let Some(received) = server.try_recv()
            && should_stop_for_signal(&received)
        {
            *rotted = true;
            return Tick::Stop("rot");
        }
        match watcher.read_if_changed() {
            Ok(Some(_)) => {}
            _ => return Tick::Continue,
        }
        match score::score_transcript(transcript, agent, repo, env) {
            Ok(score) if score.verdict == Verdict::Restart => {
                *rotted = true;
                Tick::Stop("rot")
            }
            _ => Tick::Continue,
        }
    };
```

Finally, log the timeout case so `a_hanging_child_is_killed_at_the_deadline` can assert on it. In the loop, right after the `match outcome` block computes `reason`:

```rust
        let _ = log::append(
            &state,
            &log::Decision {
                ts: now_secs(),
                session: session.as_str(),
                verb: "exec",
                verdict: reason,
                score: 0,
                action: "kill",
                detail: &transcript.display().to_string(),
            },
        );
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test ctx::exec -- --test-threads=1 2>&1 | tail -25`
Expected: PASS, 13 tests.

- [ ] **Step 5: Check lints**

Run: `cargo clippy --all-targets -- -D warnings 2>&1 | tail -20`
Expected: no warnings. `supervise_run` now has enough parameters that `clippy::too_many_arguments` matters; the `#[allow]` added in B2 covers it.

- [ ] **Step 6: Commit**

```bash
git add src/commands/ctx/exec.rs
git commit -m "feat(ctx): exec timeout kills and turn-signal accelerated scoring"
```

---

### Task B4: `zirv ctx loop` fresh session per cycle

**Files:**
- Modify: `src/commands/ctx/run_loop.rs`

**Interfaces:**
- Consumes: `supervise` primitives (B1); `score_transcript` (A13); `adapters` (A6/A8); `CtxConfig` (A2); `log`, `StateDir` (A3).
- Produces:
  - `pub struct LoopArgs { pub prompt: Option<String>, pub prompt_file: Option<PathBuf>, pub agent: Option<String>, pub interval_secs: Option<u64>, pub max_cycle_secs: Option<u64>, pub max_failures: Option<u32>, pub on_failure: Option<String>, pub cycles: Option<u32>, pub extra: Vec<String> }`
  - `pub fn resolve_prompt(args: &LoopArgs) -> CtxResult<String>`
  - `pub fn run<W: Write>(args: &LoopArgs, w: &mut W) -> CtxResult<i32>` and `run_with(args, w, repo, env)`

Each cycle is a brand new session id, so context rot at the orchestrator is structurally impossible. Durable state lives wherever the prompt's own conventions keep it.

- [ ] **Step 1: Write the failing test**

Bottom of `src/commands/ctx/run_loop.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn fixture(name: &str) -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    fn base_env(state: &std::path::Path) -> HashMap<String, String> {
        [
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state.display().to_string(),
            ),
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                fixture("fake-agent.sh").display().to_string(),
            ),
            ("ZIRV_CTX_POLL_MS".to_string(), "50".to_string()),
        ]
        .into()
    }

    fn args_for(cycles: u32) -> LoopArgs {
        LoopArgs {
            prompt: Some("run the issue loop".to_string()),
            prompt_file: None,
            agent: Some("claude".to_string()),
            interval_secs: Some(0),
            max_cycle_secs: Some(30),
            max_failures: Some(3),
            on_failure: None,
            cycles: Some(cycles),
            extra: Vec::new(),
        }
    }

    fn transcripts_in(home: &std::path::Path) -> Vec<PathBuf> {
        let projects = home.join(".claude/projects");
        let mut found = Vec::new();
        let Ok(dirs) = std::fs::read_dir(&projects) else {
            return found;
        };
        for dir in dirs.flatten() {
            let Ok(files) = std::fs::read_dir(dir.path()) else { continue };
            for file in files.flatten() {
                if file.path().extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    found.push(file.path());
                }
            }
        }
        found
    }

    #[test]
    fn prompt_resolution_prefers_the_flag_then_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("p.txt");
        std::fs::write(&file, "from the file\n").expect("write");

        let mut args = args_for(1);
        assert_eq!(resolve_prompt(&args).expect("prompt"), "run the issue loop");

        args.prompt = None;
        args.prompt_file = Some(file);
        assert_eq!(resolve_prompt(&args).expect("prompt"), "from the file");

        args.prompt_file = None;
        let err = resolve_prompt(&args).expect_err("no prompt at all");
        assert!(err.to_string().contains("--prompt"), "got {err}");
    }

    #[test]
    fn each_cycle_gets_a_fresh_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let env = base_env(&tmp.path().join("state"));

        // SAFETY: CI runs tests single-threaded.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_TURNS", "3");
        }
        let mut out = Vec::new();
        let code = run_with(&args_for(3), &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_TURNS");
        }

        assert_eq!(code.expect("runs"), 0);
        let found = transcripts_in(&home);
        assert_eq!(found.len(), 3, "one transcript per cycle: {found:?}");
    }

    #[test]
    fn a_rotted_cycle_is_killed_without_counting_as_a_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let env = base_env(&state);

        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("FAKE_AGENT_MODE", "rot");
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
        }
        let mut args = args_for(2);
        args.max_failures = Some(1);
        let started = std::time::Instant::now();
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
        }

        assert_eq!(
            code.expect("runs"),
            0,
            "rot is session hygiene, not a cycle failure: the next cycle is the restart"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(25),
            "both cycles were killed early, not left to sleep 30s each"
        );
        assert_eq!(transcripts_in(&home).len(), 2);

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"rot-kill\""), "got {log}");
        assert!(!log.contains("\"action\":\"give-up\""), "no failure escalation: {log}");
    }

    #[test]
    fn zero_cycles_is_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let env = base_env(&tmp.path().join("state"));
        let mut args = args_for(0);
        args.cycles = Some(0);
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("nothing to do");
        assert!(err.to_string().contains("cycles"), "got {err}");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ctx::run_loop -- --test-threads=1 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find function resolve_prompt`.

- [ ] **Step 3: Write minimal implementation**

Replace the stub in `src/commands/ctx/run_loop.rs`:

```rust
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::event::{SessionId, SessionRef};
use super::rot::Verdict;
use super::state::{StateDir, now_secs};
use super::supervise::{self, Outcome, Tick, Watcher};
use super::{CtxResult, adapters, log, score};

/// Repeated cycle failures, escalated to the caller.
pub const EXIT_FAILED: i32 = 75;

#[derive(Debug, clap::Args)]
pub struct LoopArgs {
    /// Prompt to run each cycle.
    #[arg(long)]
    pub prompt: Option<String>,
    /// File holding the prompt, when it is long or shared.
    #[arg(long)]
    pub prompt_file: Option<PathBuf>,
    /// Adapter name: claude or codex.
    #[arg(long)]
    pub agent: Option<String>,
    /// Seconds to wait between cycles.
    #[arg(long)]
    pub interval_secs: Option<u64>,
    /// Wall-clock limit for one cycle.
    #[arg(long)]
    pub max_cycle_secs: Option<u64>,
    /// Consecutive failures before giving up.
    #[arg(long)]
    pub max_failures: Option<u32>,
    /// Shell command to run when the loop gives up.
    #[arg(long)]
    pub on_failure: Option<String>,
    /// Stop after this many cycles. Runs forever when omitted.
    #[arg(long)]
    pub cycles: Option<u32>,
    /// Extra arguments passed through to the agent.
    #[arg(long)]
    pub extra: Vec<String>,
}

pub fn resolve_prompt(args: &LoopArgs) -> CtxResult<String> {
    if let Some(prompt) = &args.prompt {
        return Ok(prompt.clone());
    }
    if let Some(path) = &args.prompt_file {
        return Ok(std::fs::read_to_string(path)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .trim()
            .to_string());
    }
    Err("no prompt: pass --prompt or --prompt-file".into())
}

pub fn run_with<W: Write>(
    args: &LoopArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    if args.cycles == Some(0) {
        return Err("--cycles must be at least 1".into());
    }

    let prompt = resolve_prompt(args)?;
    let cfg = CtxConfig::load(repo, env)?;
    let adapter = adapters::select(
        args.agent.as_deref().or(cfg.agent.as_deref()),
        &[],
        cfg.agent_bin.as_deref(),
    )?;
    let state = StateDir::resolve(env)?;

    let interval = Duration::from_secs(args.interval_secs.unwrap_or(cfg.supervise.interval_secs));
    let max_cycle = Duration::from_secs(args.max_cycle_secs.unwrap_or(cfg.supervise.max_cycle_secs));
    let poll = Duration::from_millis(cfg.supervise.poll_ms);
    let max_failures = args.max_failures.unwrap_or(cfg.supervise.max_failures);

    let mut cycle = 0u32;
    loop {
        if let Some(limit) = args.cycles
            && cycle >= limit
        {
            return Ok(0);
        }
        cycle += 1;

        // A fresh session id per cycle is the whole point: the orchestrator
        // never accumulates context across cycles.
        let session = SessionId::new_v4();
        let transcript = adapter.transcript_path(&SessionRef {
            id: session.clone(),
            cwd: repo.to_path_buf(),
        });

        let mut command = adapter.headless_cmd(&prompt, &session, &args.extra);
        command.current_dir(repo);

        writeln!(w, "zirv ctx loop: cycle {cycle} session {session}")?;
        let mut child = supervise::spawn(command)?;
        let mut watcher = Watcher::new(transcript.clone());
        let mut rotted = false;

        let outcome = {
            let agent = args.agent.as_deref().or(cfg.agent.as_deref());
            let mut tick = || {
                match watcher.read_if_changed() {
                    Ok(Some(_)) => {}
                    _ => return Tick::Continue,
                }
                match score::score_transcript(&transcript, agent, repo, env) {
                    Ok(score) if score.verdict == Verdict::Restart => {
                        rotted = true;
                        Tick::Stop("rot")
                    }
                    _ => Tick::Continue,
                }
            };
            supervise::supervise_child(&mut child, Instant::now() + max_cycle, poll, &mut tick)?
        };

        let (action, failed) = match outcome {
            // Rot is hygiene, not failure: the next cycle is the restart.
            Outcome::StoppedByTick(_) if rotted => ("rot-kill", false),
            Outcome::StoppedByTick(reason) => (reason, true),
            Outcome::TimedOut => ("timeout-kill", true),
            Outcome::Exited(0) => ("ok", false),
            Outcome::Exited(_) => ("nonzero-exit", true),
        };

        let _ = log::append(
            &state,
            &log::Decision {
                ts: now_secs(),
                session: session.as_str(),
                verb: "loop",
                verdict: if rotted { "restart" } else { "n/a" },
                score: 0,
                action,
                detail: &transcript.display().to_string(),
            },
        );

        if let Some(code) = handle_cycle_outcome(
            args, &cfg, &state, w, repo, failed, max_failures, cycle, interval,
        )? {
            return Ok(code);
        }
    }
}

pub fn run<W: Write>(args: &LoopArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &repo, &env)
}
```

For now add the simplest `handle_cycle_outcome` that satisfies these tests; Task B5 replaces its body with backoff and `on_failure`:

```rust
#[allow(clippy::too_many_arguments)]
fn handle_cycle_outcome<W: Write>(
    _args: &LoopArgs,
    _cfg: &CtxConfig,
    _state: &StateDir,
    _w: &mut W,
    _repo: &Path,
    _failed: bool,
    _max_failures: u32,
    _cycle: u32,
    interval: Duration,
) -> CtxResult<Option<i32>> {
    if !interval.is_zero() {
        std::thread::sleep(interval);
    }
    Ok(None)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test ctx::run_loop -- --test-threads=1 2>&1 | tail -20`
Expected: PASS, 4 tests.

- [ ] **Step 5: Commit**

```bash
git add src/commands/ctx/run_loop.rs
git commit -m "feat(ctx): zirv ctx loop runs a fresh headless session per cycle"
```

---

### Task B5: `zirv ctx loop` backoff, failure caps and `on_failure`

**Files:**
- Modify: `src/commands/ctx/run_loop.rs`

**Interfaces:**
- Consumes: `handle_cycle_outcome` (B4); `supervise::run_shell` (B1).
- Produces: `pub fn backoff_for(failures: u32, base: Duration, interval: Duration) -> Duration` and a real `handle_cycle_outcome` that tracks consecutive failures across cycles.

Consecutive-failure state has to live across cycles, so `handle_cycle_outcome` takes `failures: &mut u32` and `run_with` owns the counter.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` in `src/commands/ctx/run_loop.rs`:

```rust
    #[test]
    fn backoff_doubles_and_is_capped() {
        let base = Duration::from_secs(60);
        let interval = Duration::from_secs(900);
        assert_eq!(backoff_for(0, base, interval), Duration::ZERO);
        assert_eq!(backoff_for(1, base, interval), Duration::from_secs(60));
        assert_eq!(backoff_for(2, base, interval), Duration::from_secs(120));
        assert_eq!(backoff_for(3, base, interval), Duration::from_secs(240));
        assert_eq!(
            backoff_for(20, base, interval),
            Duration::from_secs(3600),
            "capped at four intervals"
        );
    }

    #[test]
    fn backoff_never_overflows_on_absurd_failure_counts() {
        let capped = backoff_for(u32::MAX, Duration::from_secs(60), Duration::from_secs(900));
        assert_eq!(capped, Duration::from_secs(3600));
    }

    #[test]
    fn repeated_failures_run_on_failure_and_exit_nonzero() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let env = base_env(&state);
        let marker = tmp.path().join("on-failure-ran");

        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("FAKE_AGENT_MODE", "fail");
            std::env::set_var("FAKE_AGENT_TURNS", "2");
        }
        let mut args = args_for(5);
        args.max_failures = Some(2);
        args.on_failure = Some(format!("touch {}", marker.display()));
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_TURNS");
        }

        assert_eq!(code.expect("runs"), EXIT_FAILED);
        assert!(marker.exists(), "the on_failure hook must run");
        assert_eq!(
            transcripts_in(&home).len(),
            2,
            "it stopped at the failure cap instead of running all 5 cycles"
        );

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"give-up\""), "got {log}");
    }

    #[test]
    fn a_successful_cycle_resets_the_failure_count() {
        let mut failures = 3u32;
        let mut out = Vec::new();
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let mut args = args_for(1);
        args.interval_secs = Some(0);

        let code = handle_cycle_outcome(
            &args,
            &cfg,
            &state,
            &mut out,
            tmp.path(),
            false,
            5,
            1,
            Duration::ZERO,
            &mut failures,
        )
        .expect("handled");
        assert_eq!(code, None, "keep looping");
        assert_eq!(failures, 0, "a green cycle clears the streak");
    }

    #[test]
    fn a_failure_below_the_cap_keeps_looping() {
        let mut failures = 0u32;
        let mut out = Vec::new();
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let mut args = args_for(1);
        args.on_failure = None;

        let code = handle_cycle_outcome(
            &args,
            &cfg,
            &state,
            &mut out,
            tmp.path(),
            true,
            5,
            1,
            Duration::ZERO,
            &mut failures,
        )
        .expect("handled");
        assert_eq!(code, None);
        assert_eq!(failures, 1);
    }
```

Also update the two B4 tests that call `handle_cycle_outcome` indirectly: nothing changes for them, but `run_with` must now pass `&mut failures`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ctx::run_loop -- --test-threads=1 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find function backoff_for`, and an arity mismatch on `handle_cycle_outcome`.

- [ ] **Step 3: Write minimal implementation**

Replace the placeholder `handle_cycle_outcome` in `src/commands/ctx/run_loop.rs`:

```rust
/// Exponential backoff on consecutive failures, capped at four intervals so a
/// broken loop still checks in occasionally.
pub fn backoff_for(failures: u32, base: Duration, interval: Duration) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    let cap = interval.saturating_mul(4);
    let shift = (failures - 1).min(16);
    let scaled = base.saturating_mul(1u32 << shift);
    if scaled > cap { cap } else { scaled }
}

#[allow(clippy::too_many_arguments)]
fn handle_cycle_outcome<W: Write>(
    args: &LoopArgs,
    cfg: &CtxConfig,
    state: &StateDir,
    w: &mut W,
    repo: &Path,
    failed: bool,
    max_failures: u32,
    cycle: u32,
    interval: Duration,
    failures: &mut u32,
) -> CtxResult<Option<i32>> {
    if !failed {
        *failures = 0;
        if !interval.is_zero() {
            std::thread::sleep(interval);
        }
        return Ok(None);
    }

    *failures += 1;
    writeln!(
        w,
        "zirv ctx loop: cycle {cycle} failed ({}/{max_failures} consecutive)",
        *failures
    )?;

    if *failures >= max_failures {
        let on_failure = args.on_failure.clone().or_else(|| cfg.supervise.on_failure.clone());
        let detail = match &on_failure {
            Some(command) => {
                let code = supervise::run_shell(command, repo)?;
                format!("on_failure exited {code}")
            }
            None => "no on_failure command configured".to_string(),
        };
        let _ = log::append(
            state,
            &log::Decision {
                ts: now_secs(),
                session: "loop",
                verb: "loop",
                verdict: "n/a",
                score: 0,
                action: "give-up",
                detail: &detail,
            },
        );
        writeln!(
            w,
            "zirv ctx loop: giving up after {} consecutive failures, exiting {EXIT_FAILED}",
            *failures
        )?;
        return Ok(Some(EXIT_FAILED));
    }

    let wait = backoff_for(*failures, Duration::from_secs(cfg.supervise.backoff_base_secs), interval);
    if !wait.is_zero() {
        writeln!(w, "zirv ctx loop: backing off {}s", wait.as_secs())?;
        std::thread::sleep(wait);
    }
    Ok(None)
}
```

In `run_with`, declare `let mut failures = 0u32;` before the loop and pass `&mut failures` as the final argument.

The backoff sleeps real time, so `repeated_failures_run_on_failure_and_exit_nonzero` sets `max_failures = 2`: the first failure backs off `backoff_base_secs`. Set `ZIRV_CTX_BACKOFF_BASE_SECS` is not in the env map, so instead the test relies on `interval_secs = 0`, which caps the backoff at zero. Confirm `backoff_for(1, 60s, 0s)` returns `Duration::ZERO` because the cap is `0 * 4 = 0`. Add that as an explicit test:

```rust
    #[test]
    fn a_zero_interval_disables_backoff_entirely() {
        assert_eq!(
            backoff_for(3, Duration::from_secs(60), Duration::ZERO),
            Duration::ZERO,
            "tests and one-shot loops must not sleep"
        );
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test ctx::run_loop -- --test-threads=1 2>&1 | tail -20`
Expected: PASS, 10 tests.

- [ ] **Step 5: Run the whole suite and the lints**

Run: `cargo test --verbose -- --test-threads=1 2>&1 | tail -15`
Expected: PASS.
Run: `cargo fmt -- --check && cargo clippy --all-targets -- -D warnings 2>&1 | tail -10`
Expected: clean.

- [ ] **Step 6: Verify Phase B by hand against the fake agent**

Run:

```bash
WORK=$(mktemp -d) && cd "$WORK"
HOME="$WORK/home" FAKE_AGENT_MODE=healthy FAKE_AGENT_TURNS=3 \
ZIRV_CTX_AGENT_BIN="$OLDPWD/tests/fixtures/fake-agent.sh" \
ZIRV_CTX_STATE_DIR="$WORK/state" \
  "$OLDPWD/target/debug/zirv" ctx loop --prompt "probe" --cycles 2 --interval-secs 0
find "$WORK/home" -name '*.jsonl' | wc -l
```

Expected: two cycle lines with different session ids, and `2` transcripts.

- [ ] **Step 7: Commit**

```bash
git add src/commands/ctx/run_loop.rs
git commit -m "feat(ctx): loop backoff, failure caps and on_failure hook"
```

---

# Phase C: Interactive supervision

Ships `zirv ctx wrap`. The TUI is sacred: a wrapped session must never be worse than an unwrapped one.

### Task C1: Terminal raw mode and window size

**Files:**
- Modify: `src/commands/ctx/term.rs`

**Interfaces:**
- Consumes: `CtxResult` (A1); `libc` (A1).
- Produces:
  - `pub struct RawGuard` with `pub fn enter(fd: i32) -> CtxResult<Self>`, `pub fn restore(&mut self) -> CtxResult<()>` (idempotent), `pub fn is_active(&self) -> bool`
  - `pub fn window_size(fd: i32) -> CtxResult<(u16, u16)>` returning `(cols, rows)`
  - `pub const STDIN_FD: i32 = 0;`

The release profile is `panic = "abort"`, so unwinding never runs `Drop`. Every error arm in `wrap` therefore calls `restore()` explicitly; the `Drop` impl exists only as a backstop for ordinary returns.

- [ ] **Step 1: Write the failing test**

Bottom of `src/commands/ctx/term.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entering_raw_mode_on_a_non_terminal_is_an_error() {
        // CI has no controlling terminal, so this is the path that must be safe.
        let err = RawGuard::enter(-1).expect_err("fd -1 is not a terminal");
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn window_size_on_a_non_terminal_is_an_error() {
        assert!(window_size(-1).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn restore_is_idempotent_and_reports_when_it_is_done() {
        // A real pty gives us a terminal even in CI.
        let pty = portable_pty::native_pty_system();
        let pair = pty
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        let fd = {
            use std::os::fd::AsRawFd;
            pair.slave.as_raw_fd().expect("slave fd")
        };

        let mut guard = RawGuard::enter(fd).expect("raw mode on a pty");
        assert!(guard.is_active());
        guard.restore().expect("restore");
        assert!(!guard.is_active());
        guard.restore().expect("a second restore is a no-op");
        assert!(!guard.is_active());
    }

    #[cfg(unix)]
    #[test]
    fn window_size_reads_the_pty_dimensions() {
        let pty = portable_pty::native_pty_system();
        let pair = pty
            .openpty(portable_pty::PtySize {
                rows: 30,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let fd = {
            use std::os::fd::AsRawFd;
            pair.slave.as_raw_fd().expect("slave fd")
        };
        assert_eq!(window_size(fd).expect("size"), (100, 30));
    }
}
```

If `portable_pty::SlavePty` exposes no `as_raw_fd` in 0.9, replace the fd source in both tests with `libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR)` guarded by a skip when it returns `-1` (no controlling terminal), and keep the two non-terminal tests as the CI-visible coverage. Check with: `cargo doc -p portable-pty --open` or `grep -rn "as_raw_fd" ~/.cargo/registry/src/*/portable-pty-0.9.0/src/`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ctx::term 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find struct RawGuard`.

- [ ] **Step 3: Write minimal implementation**

```rust
use super::CtxResult;

pub const STDIN_FD: i32 = 0;

#[cfg(unix)]
pub struct RawGuard {
    fd: i32,
    saved: libc::termios,
    active: bool,
}

#[cfg(unix)]
impl RawGuard {
    pub fn enter(fd: i32) -> CtxResult<Self> {
        // SAFETY: `saved` is only read after a successful tcgetattr.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
            return Err("tcgetattr failed: stdin is not a terminal".into());
        }
        let mut raw = saved;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err("tcsetattr failed: could not enter raw mode".into());
        }
        Ok(Self {
            fd,
            saved,
            active: true,
        })
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Idempotent. `panic = "abort"` means Drop is not guaranteed, so callers
    /// invoke this explicitly in every arm that leaves the pump loop.
    pub fn restore(&mut self) -> CtxResult<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        if unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) } != 0 {
            return Err("tcsetattr failed: could not restore the terminal".into());
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(unix)]
pub fn window_size(fd: i32) -> CtxResult<(u16, u16)> {
    // SAFETY: `ws` is only read after a successful ioctl.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut ws) } != 0 {
        return Err("TIOCGWINSZ failed: not a terminal".into());
    }
    Ok((ws.ws_col, ws.ws_row))
}

#[cfg(not(unix))]
pub struct RawGuard {
    active: bool,
}

#[cfg(not(unix))]
impl RawGuard {
    pub fn enter(_fd: i32) -> CtxResult<Self> {
        Err("raw terminal mode is only implemented on unix".into())
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn restore(&mut self) -> CtxResult<()> {
        self.active = false;
        Ok(())
    }
}

#[cfg(not(unix))]
pub fn window_size(_fd: i32) -> CtxResult<(u16, u16)> {
    Err("window size probing is only implemented on unix".into())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test ctx::term 2>&1 | tail -20`
Expected: PASS, 4 tests on unix (2 if the pty fd probe had to be dropped per Step 1).

- [ ] **Step 5: Commit**

```bash
git add src/commands/ctx/term.rs
git commit -m "feat(ctx): raw mode guard and window size probing"
```

---

### Task C2: `zirv ctx wrap` byte-for-byte PTY passthrough

**Files:**
- Create: `tests/fixtures/stub-tui.sh`
- Modify: `src/commands/ctx/wrap.rs`

**Interfaces:**
- Consumes: `RawGuard`, `window_size`, `STDIN_FD` (C1); `adapters::select` (A6); `CtxConfig` (A2); `portable-pty` (A1).
- Produces:
  - `pub struct WrapArgs { pub agent: Option<String>, pub no_supervise: bool, pub command: Vec<String> }`
  - `pub enum PumpEvent { Output(usize), Input(usize), PtyClosed }`
  - `pub fn run<W: Write>(args: &WrapArgs, w: &mut W) -> CtxResult<i32>` and `run_with(args, w, repo, env)`

Passthrough comes first and alone in this task: supervision is layered on in C3 to C6. A reviewer can reject supervision while keeping a working transparent wrapper.

- [ ] **Step 1: Write the stub TUI**

Create `tests/fixtures/stub-tui.sh` and `chmod +x` it:

```sh
#!/bin/sh
# Minimal interactive stand-in for an agent TUI, driven by wrap's PTY.
#
# Echoes every line back with a prefix so passthrough fidelity is checkable.
# Records injected slash-commands to $STUB_TUI_LOG and appends a compaction
# event to $STUB_TUI_TRANSCRIPT when it sees /compact, which is what wrap
# watches for when it verifies an injection. Exits on /exit, /quit or EOF.
set -eu
printf 'stub-tui ready\n'
while IFS= read -r line; do
  printf 'echo: %s\n' "$line"
  case "$line" in
    /compact*)
      [ -z "${STUB_TUI_LOG:-}" ] || printf '%s\n' "$line" >> "$STUB_TUI_LOG"
      if [ -n "${STUB_TUI_TRANSCRIPT:-}" ]; then
        printf '{"type":"system","subtype":"compact_boundary","content":"Conversation compacted"}\n' >> "$STUB_TUI_TRANSCRIPT"
        printf '{"type":"user","message":{"content":"post-compaction"}}\n' >> "$STUB_TUI_TRANSCRIPT"
        printf '{"type":"assistant","message":{"content":[{"type":"text","text":"[zirv] fresh"}],"usage":{"input_tokens":9000}}}\n' >> "$STUB_TUI_TRANSCRIPT"
      fi
      printf 'compacted\n'
      ;;
    /exit|/quit) printf 'bye\n'; exit 0 ;;
    /fail) exit 5 ;;
  esac
done
```

The PTY slave keeps default line discipline, so a carriage return sent by wrap arrives as a newline for `read`, and the stub's `\n` output comes back as `\r\n`. Assertions therefore use `contains`, never equality.

- [ ] **Step 2: Write the failing test**

Bottom of `src/commands/ctx/wrap.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::io::{Read, Write as _};
    use std::time::{Duration, Instant};

    pub(crate) fn zirv_bin() -> PathBuf {
        // cargo test builds the bin target, so it sits next to the test binary's
        // grandparent directory (target/debug/deps/<test> -> target/debug/zirv).
        std::env::current_exe()
            .expect("current_exe")
            .parent()
            .and_then(|p| p.parent())
            .expect("target dir")
            .join(if cfg!(windows) { "zirv.exe" } else { "zirv" })
    }

    pub(crate) fn fixture(name: &str) -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    /// Drives `zirv ctx wrap` from inside an outer PTY, which is the only way to
    /// exercise raw-mode passthrough end to end.
    pub(crate) struct Harness {
        pub reader: Box<dyn Read + Send>,
        pub writer: Box<dyn Write + Send>,
        pub child: Box<dyn portable_pty::Child + Send + Sync>,
    }

    pub(crate) fn spawn_wrap(extra_env: &[(&str, String)], wrapped: &[&str]) -> Harness {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        let mut cmd = CommandBuilder::new(zirv_bin());
        cmd.arg("ctx");
        cmd.arg("wrap");
        cmd.arg("--agent");
        cmd.arg("claude");
        cmd.arg("--");
        for arg in wrapped {
            cmd.arg(arg);
        }
        cmd.env("TERM", "xterm");
        for (key, value) in extra_env {
            cmd.env(key, value);
        }

        let child = pair.slave.spawn_command(cmd).expect("spawn wrap");
        drop(pair.slave);
        Harness {
            reader: pair.master.try_clone_reader().expect("reader"),
            writer: pair.master.take_writer().expect("writer"),
            child,
        }
    }

    /// Reads until `needle` appears or the timeout expires.
    pub(crate) fn read_until(reader: &mut Box<dyn Read + Send>, needle: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        let mut seen = String::new();
        let mut buf = [0u8; 1024];
        while Instant::now() < deadline {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    seen.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if seen.contains(needle) {
                        return seen;
                    }
                }
                Err(_) => break,
            }
        }
        seen
    }

    #[test]
    fn wrap_needs_a_command() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let args = WrapArgs {
            agent: None,
            no_supervise: false,
            command: Vec::new(),
        };
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|_| None).expect_err("nothing to wrap");
        assert!(err.to_string().contains("command"), "got {err}");
    }

    #[cfg(unix)]
    #[test]
    fn the_wrapped_program_output_reaches_the_terminal() {
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(&[], &["sh", &script]);

        let seen = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));
        assert!(seen.contains("stub-tui ready"), "got: {seen:?}");

        h.writer.write_all(b"/exit\r").expect("write");
        h.writer.flush().expect("flush");
        let _ = read_until(&mut h.reader, "bye", Duration::from_secs(10));
        let status = h.child.wait().expect("wait");
        assert_eq!(status.exit_code(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn keystrokes_pass_through_byte_for_byte() {
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(&[], &["sh", &script]);
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        h.writer.write_all("hello wrap\r".as_bytes()).expect("write");
        h.writer.flush().expect("flush");
        let seen = read_until(&mut h.reader, "echo: hello wrap", Duration::from_secs(10));
        assert!(seen.contains("echo: hello wrap"), "got: {seen:?}");

        h.writer.write_all(b"/exit\r").expect("write");
        let _ = h.child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn the_wrapped_exit_code_is_propagated() {
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(&[], &["sh", &script]);
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        h.writer.write_all(b"/fail\r").expect("write");
        h.writer.flush().expect("flush");
        let status = h.child.wait().expect("wait");
        assert_eq!(status.exit_code(), 5, "wrap must not swallow the agent's code");
    }

    #[cfg(unix)]
    #[test]
    fn wrap_exits_when_the_wrapped_program_exits_on_its_own() {
        let mut h = spawn_wrap(&[], &["sh", "-c", "printf done\\n; exit 0"]);
        let seen = read_until(&mut h.reader, "done", Duration::from_secs(10));
        assert!(seen.contains("done"), "got {seen:?}");
        let status = h.child.wait().expect("wait");
        assert_eq!(status.exit_code(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_wrapped_binary_fails_without_wrecking_the_terminal() {
        let mut h = spawn_wrap(&[], &["/nonexistent/agent-binary"]);
        let status = h.child.wait().expect("wait");
        assert_ne!(status.exit_code(), 0);
        let seen = read_until(&mut h.reader, "", Duration::from_millis(300));
        assert!(!seen.contains("panicked"), "no panic on the hot path: {seen:?}");
    }
}
```

`zirv_bin`, `fixture`, `Harness`, `spawn_wrap` and `read_until` are `pub(crate)` because Tasks C4, C5 and C6 add tests to this same module that reuse them.

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test ctx::wrap 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find struct WrapArgs` fields / `run_with`.

- [ ] **Step 4: Write minimal implementation**

Replace the stub in `src/commands/ctx/wrap.rs`:

```rust
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::term::{RawGuard, STDIN_FD, window_size};
use super::{CtxResult, adapters};

const PUMP_POLL: Duration = Duration::from_millis(100);
const DEFAULT_SIZE: (u16, u16) = (80, 24);

#[derive(Debug, clap::Args)]
pub struct WrapArgs {
    /// Adapter name: claude or codex. Detected from the command when omitted.
    #[arg(long)]
    pub agent: Option<String>,
    /// Pure passthrough: no scoring, no injection.
    #[arg(long, default_value_t = false)]
    pub no_supervise: bool,
    /// The interactive agent command, after `--`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, last = true)]
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpEvent {
    Output(usize),
    Input(usize),
    PtyClosed,
}

pub fn run_with<W: Write>(
    args: &WrapArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let (program, rest) = args
        .command
        .split_first()
        .ok_or("no command to wrap; pass it after --")?;

    let cfg = CtxConfig::load(repo, env)?;
    // Selection happens here so an unknown or unverified agent fails before the
    // terminal is touched.
    let _adapter = adapters::select(
        args.agent.as_deref().or(cfg.agent.as_deref()),
        &args.command,
        cfg.agent_bin.as_deref(),
    )?;

    let (cols, rows) = window_size(STDIN_FD).unwrap_or(DEFAULT_SIZE);
    let pair = native_pty_system().openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut command = CommandBuilder::new(program);
    for arg in rest {
        command.arg(arg);
    }
    command.cwd(repo);
    let mut child = pair.slave.spawn_command(command)?;

    let mut reader = pair.master.try_clone_reader()?;
    // One writer, shared: the stdin pump and (from Task C4) the injector both
    // need it, and `take_writer` can only be called once.
    let writer = std::sync::Arc::new(std::sync::Mutex::new(pair.master.take_writer()?));
    let (tx, rx) = mpsc::channel::<PumpEvent>();

    // PTY to stdout.
    let output_tx = tx.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut stdout = std::io::stdout();
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => {
                    let _ = output_tx.send(PumpEvent::PtyClosed);
                    return;
                }
                Ok(n) => {
                    if stdout.write_all(&buf[..n]).is_err() || stdout.flush().is_err() {
                        let _ = output_tx.send(PumpEvent::PtyClosed);
                        return;
                    }
                    if output_tx.send(PumpEvent::Output(n)).is_err() {
                        return;
                    }
                }
            }
        }
    });

    // stdin to PTY.
    let input_tx = tx;
    let input_writer = std::sync::Arc::clone(&writer);
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut stdin = std::io::stdin();
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    let Ok(mut sink) = input_writer.lock() else {
                        return;
                    };
                    if sink.write_all(&buf[..n]).is_err() || sink.flush().is_err() {
                        return;
                    }
                    drop(sink);
                    if input_tx.send(PumpEvent::Input(n)).is_err() {
                        return;
                    }
                }
            }
        }
    });

    // Raw mode is best-effort: without a terminal (a pipe, or CI) the wrapper
    // still passes bytes through.
    let mut raw = RawGuard::enter(STDIN_FD).ok();

    let exit = pump(&mut child, &rx, &pair);

    if let Some(guard) = raw.as_mut() {
        let _ = guard.restore();
    }

    match exit {
        Ok(code) => Ok(code),
        Err(e) => {
            writeln!(w, "zirv ctx wrap: {e}")?;
            Ok(1)
        }
    }
}

fn pump(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    rx: &mpsc::Receiver<PumpEvent>,
    pair: &portable_pty::PtyPair,
) -> CtxResult<i32> {
    let mut last_size = window_size(STDIN_FD).unwrap_or(DEFAULT_SIZE);

    loop {
        if let Some(status) = child.try_wait()? {
            // Let the reader thread flush whatever is still buffered.
            while rx.recv_timeout(Duration::from_millis(50)).is_ok() {}
            return Ok(status.exit_code() as i32);
        }

        while let Ok(event) = rx.try_recv() {
            if event == PumpEvent::PtyClosed {
                let status = child.wait()?;
                return Ok(status.exit_code() as i32);
            }
        }

        if let Ok(size) = window_size(STDIN_FD)
            && size != last_size
        {
            last_size = size;
            let _ = pair.master.resize(PtySize {
                rows: size.1,
                cols: size.0,
                pixel_width: 0,
                pixel_height: 0,
            });
        }

        std::thread::sleep(PUMP_POLL);
    }
}

pub fn run<W: Write>(args: &WrapArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &repo, &env)
}
```

If `take_writer` cannot be called twice in portable-pty 0.9, drop the `writer` binding and take it exactly once inside the stdin thread, then obtain the injection writer in C4 through `pair.master.take_writer()` before the thread starts and share it with a `std::sync::Mutex`. Check with: `grep -rn "fn take_writer" ~/.cargo/registry/src/*/portable-pty-0.9.0/src/`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test ctx::wrap -- --test-threads=1 2>&1 | tail -25`
Expected: PASS, 6 tests. These spawn the built binary, so run `cargo build` first if the binary is stale.

- [ ] **Step 6: Verify transparency by hand**

Run: `cargo run --quiet -- ctx wrap -- sh -c 'echo hello; sleep 1'`
Expected: `hello`, a clean exit, and a terminal that still echoes your typing afterwards. Then run `stty -a | head -2` and confirm `echo` is present, proving raw mode was restored.

- [ ] **Step 7: Commit**

```bash
git add tests/fixtures/stub-tui.sh src/commands/ctx/wrap.rs
git commit -m "feat(ctx): zirv ctx wrap transparent PTY passthrough"
```

---

### Task C3: Injection preconditions

**Files:**
- Modify: `src/commands/ctx/wrap.rs`

**Interfaces:**
- Consumes: `PumpEvent` (C2); `Verdict`, `Score` (A12); `TurnSignal`, `SignalServer` (A14); `WrapConfig` (A2).
- Produces:
  - `pub struct InjectionState { pub last_turn: u64, pub verdict: Verdict, pub score: u32, pub user_typed_since_turn: bool, pub last_output: Instant, pub cooldown_until_turn: Option<u64>, pub degraded: bool }` with `pub fn new() -> Self` (plus `Default`), `pub fn on_event(&mut self, event: PumpEvent, now: Instant)`, `pub fn on_turn(&mut self, signal: &TurnSignal)`
  - `pub fn may_inject(state: &InjectionState, now: Instant, debounce: Duration) -> bool`

Both preconditions from the spec are enforced here and nowhere else: a turn boundary must have arrived, and the user must be idle (nothing typed since that boundary, and the PTY output-quiet for the debounce interval).

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` in `src/commands/ctx/wrap.rs`:

```rust
    use crate::commands::ctx::rot::Verdict;
    use crate::commands::ctx::signal::TurnSignal;

    fn turn_signal(turn: u64, verdict: Verdict) -> TurnSignal {
        TurnSignal {
            session_id: "s".to_string(),
            turn,
            score: 64,
            verdict,
        }
    }

    fn ready_state(now: Instant) -> InjectionState {
        let mut state = InjectionState::new();
        state.on_turn(&turn_signal(3, Verdict::Compact));
        state.last_output = now - Duration::from_secs(10);
        state
    }

    const DEBOUNCE: Duration = Duration::from_secs(3);

    #[test]
    fn a_fresh_state_never_injects() {
        let now = Instant::now();
        let state = InjectionState::new();
        assert!(!may_inject(&state, now, DEBOUNCE), "no turn boundary seen yet");
    }

    #[test]
    fn an_idle_user_at_a_turn_boundary_may_be_injected_into() {
        let now = Instant::now();
        assert!(may_inject(&ready_state(now), now, DEBOUNCE));
    }

    #[test]
    fn typing_after_the_turn_blocks_injection() {
        let now = Instant::now();
        let mut state = ready_state(now);
        state.on_event(PumpEvent::Input(1), now);
        assert!(!may_inject(&state, now, DEBOUNCE), "the user is mid-thought");
    }

    #[test]
    fn recent_output_blocks_injection_until_the_debounce_passes() {
        let now = Instant::now();
        let mut state = ready_state(now);
        state.on_event(PumpEvent::Output(120), now);
        assert!(!may_inject(&state, now, DEBOUNCE));
        assert!(
            may_inject(&state, now + Duration::from_secs(4), DEBOUNCE),
            "quiet for longer than the debounce"
        );
    }

    #[test]
    fn a_new_turn_clears_the_typing_flag() {
        let now = Instant::now();
        let mut state = ready_state(now);
        state.on_event(PumpEvent::Input(1), now);
        state.on_turn(&turn_signal(4, Verdict::Compact));
        state.last_output = now - Duration::from_secs(10);
        assert!(may_inject(&state, now, DEBOUNCE));
        assert_eq!(state.last_turn, 4);
    }

    #[test]
    fn the_cooldown_blocks_until_a_later_turn_arrives() {
        let now = Instant::now();
        let mut state = ready_state(now);
        state.cooldown_until_turn = Some(3);
        assert!(!may_inject(&state, now, DEBOUNCE), "same turn as the cooldown");

        state.on_turn(&turn_signal(4, Verdict::Compact));
        state.last_output = now - Duration::from_secs(10);
        assert!(may_inject(&state, now, DEBOUNCE), "a later turn releases it");
    }

    #[test]
    fn a_degraded_supervisor_never_injects() {
        let now = Instant::now();
        let mut state = ready_state(now);
        state.degraded = true;
        assert!(!may_inject(&state, now, DEBOUNCE));
    }

    #[test]
    fn the_state_records_the_latest_verdict_and_score() {
        let mut state = InjectionState::new();
        state.on_turn(&TurnSignal {
            session_id: "s".to_string(),
            turn: 9,
            score: 91,
            verdict: Verdict::Restart,
        });
        assert_eq!(state.verdict, Verdict::Restart);
        assert_eq!(state.score, 91);
        assert_eq!(state.last_turn, 9);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ctx::wrap 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find struct InjectionState`.

- [ ] **Step 3: Write minimal implementation**

Add to `src/commands/ctx/wrap.rs`:

```rust
use std::time::Instant;

use super::rot::Verdict;
use super::signal::TurnSignal;

#[derive(Debug, Clone)]
pub struct InjectionState {
    pub last_turn: u64,
    pub verdict: Verdict,
    pub score: u32,
    pub user_typed_since_turn: bool,
    pub last_output: Instant,
    pub cooldown_until_turn: Option<u64>,
    pub degraded: bool,
}

impl Default for InjectionState {
    fn default() -> Self {
        Self::new()
    }
}

impl InjectionState {
    pub fn new() -> Self {
        Self {
            last_turn: 0,
            verdict: Verdict::Healthy,
            score: 0,
            user_typed_since_turn: false,
            last_output: Instant::now(),
            cooldown_until_turn: None,
            degraded: false,
        }
    }

    pub fn on_event(&mut self, event: PumpEvent, now: Instant) {
        match event {
            PumpEvent::Output(_) => self.last_output = now,
            PumpEvent::Input(_) => self.user_typed_since_turn = true,
            PumpEvent::PtyClosed => {}
        }
    }

    pub fn on_turn(&mut self, signal: &TurnSignal) {
        self.last_turn = signal.turn;
        self.verdict = signal.verdict;
        self.score = signal.score;
        self.user_typed_since_turn = false;
    }
}

/// Both spec preconditions, and nothing else: a turn boundary has been reported
/// and the user is idle. Everything about which verdict deserves which action
/// lives in the escalation ladder, not here.
pub fn may_inject(state: &InjectionState, now: Instant, debounce: Duration) -> bool {
    !state.degraded
        && state.last_turn > 0
        && !state.user_typed_since_turn
        && now.duration_since(state.last_output) >= debounce
        && state
            .cooldown_until_turn
            .is_none_or(|turn| state.last_turn > turn)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test ctx::wrap -- --test-threads=1 2>&1 | tail -20`
Expected: PASS, 14 tests.

- [ ] **Step 5: Commit**

```bash
git add src/commands/ctx/wrap.rs
git commit -m "feat(ctx): wrap injection preconditions at turn boundary and user idle"
```

---

### Task C4: Advisory and verified compaction with cooldown

**Files:**
- Modify: `src/commands/ctx/wrap.rs`

**Interfaces:**
- Consumes: `InjectionState`, `may_inject` (C3); `AgentAdapter::compact_command` (A6); `Watcher` (B1); `NormalizedEvent::Compaction` (A4); `log` (A3).
- Produces:
  - `pub const COMPACT_FOCUS: &str`
  - `pub enum Action { None, Advise, Compact, Restart }`
  - `pub fn action_for(state: &InjectionState, now: Instant, debounce: Duration) -> Action`
  - `pub fn advisory_line(score: u32, tokens: u64) -> String`
  - `pub fn inject_compact(sink: &mut dyn Write, compact_command: &str) -> CtxResult<()>`
  - `pub fn verify_compaction(watcher: &mut Watcher, adapter: &dyn AgentAdapter, deadline: Instant) -> CtxResult<bool>`

The advisory rung writes to the terminal and does not inject. The compact rung injects the adapter's compaction command **with focus instructions as arguments**, which is where the spec's "compaction focus instructions" actually live, since `PreCompact` hooks cannot inject them. After injecting, wrap verifies a compaction event appeared and arms the cooldown; if verification fails it retreats to advisory rather than retrying keystrokes.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` in `src/commands/ctx/wrap.rs`:

```rust
    use crate::commands::ctx::adapters::claude::ClaudeAdapter;
    use crate::commands::ctx::supervise::Watcher;

    #[test]
    fn the_ladder_maps_verdicts_to_actions() {
        let now = Instant::now();
        let mut state = ready_state(now);

        state.verdict = Verdict::Healthy;
        assert_eq!(action_for(&state, now, DEBOUNCE), Action::None);

        state.verdict = Verdict::Advise;
        assert_eq!(action_for(&state, now, DEBOUNCE), Action::Advise);

        state.verdict = Verdict::Compact;
        assert_eq!(action_for(&state, now, DEBOUNCE), Action::Compact);

        state.verdict = Verdict::Restart;
        assert_eq!(action_for(&state, now, DEBOUNCE), Action::Restart);
    }

    #[test]
    fn an_advisory_needs_no_injection_window() {
        let now = Instant::now();
        let mut state = ready_state(now);
        state.verdict = Verdict::Advise;
        state.on_event(PumpEvent::Input(1), now);
        assert_eq!(
            action_for(&state, now, DEBOUNCE),
            Action::Advise,
            "advice is written to the terminal, never typed into the agent"
        );
    }

    #[test]
    fn compaction_and_restart_respect_the_injection_window() {
        let now = Instant::now();
        for verdict in [Verdict::Compact, Verdict::Restart] {
            let mut state = ready_state(now);
            state.verdict = verdict;
            state.on_event(PumpEvent::Input(1), now);
            assert_eq!(
                action_for(&state, now, DEBOUNCE),
                Action::None,
                "{verdict:?} must wait for an idle user"
            );
        }
    }

    #[test]
    fn a_degraded_supervisor_still_advises_but_never_injects() {
        let now = Instant::now();
        let mut state = ready_state(now);
        state.degraded = true;
        state.verdict = Verdict::Advise;
        assert_eq!(action_for(&state, now, DEBOUNCE), Action::None);
    }

    #[test]
    fn the_advisory_line_is_one_line_and_plain() {
        let line = advisory_line(47, 138_000);
        assert_eq!(line.lines().count(), 1);
        assert!(line.contains("47"));
        assert!(line.contains("138000") || line.contains("138"));
        assert!(!line.contains('\u{2014}'), "no em dashes in user-facing copy");
    }

    #[test]
    fn the_injected_command_carries_focus_instructions_and_ends_with_a_carriage_return() {
        let mut sink: Vec<u8> = Vec::new();
        inject_compact(&mut sink, "/compact").expect("inject");
        let text = String::from_utf8(sink).expect("utf8");
        assert!(text.starts_with("/compact "), "got {text:?}");
        assert!(text.contains(COMPACT_FOCUS));
        assert!(text.ends_with('\r'), "a TUI submits on carriage return: {text:?}");
        assert_eq!(text.matches('\r').count(), 1, "exactly one submit");
        assert!(!text.contains('\n'), "no stray newline: {text:?}");
    }

    #[test]
    fn the_focus_text_names_what_to_preserve() {
        for needle in ["task", "file", "error", "next step"] {
            assert!(
                COMPACT_FOCUS.to_lowercase().contains(needle),
                "focus text should mention {needle}: {COMPACT_FOCUS}"
            );
        }
        assert!(!COMPACT_FOCUS.contains('\u{2014}'));
    }

    #[test]
    fn verification_succeeds_when_a_compaction_event_appears() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, "{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n")
            .expect("write");

        let mut watcher = Watcher::new(path.clone());
        let _ = watcher.read_if_changed();

        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open");
            use std::io::Write as _;
            writeln!(
                file,
                "{{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"content\":\"x\"}}"
            )
            .expect("append");
        });

        let adapter = ClaudeAdapter::new(None);
        let verified = verify_compaction(
            &mut watcher,
            &adapter,
            Instant::now() + Duration::from_secs(5),
        )
        .expect("verify");
        writer.join().expect("writer thread");
        assert!(verified);
    }

    #[test]
    fn verification_gives_up_at_the_deadline_instead_of_retrying() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, "{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n")
            .expect("write");
        let mut watcher = Watcher::new(path);
        let adapter = ClaudeAdapter::new(None);

        let started = Instant::now();
        let verified = verify_compaction(
            &mut watcher,
            &adapter,
            Instant::now() + Duration::from_millis(300),
        )
        .expect("verify");
        assert!(!verified);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[cfg(unix)]
    #[test]
    fn a_compact_verdict_at_an_idle_turn_boundary_injects_into_the_tui() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let log = tmp.path().join("injected.log");
        let transcript = tmp.path().join("t.jsonl");
        // A transcript that scores `compact`: marker misses plus tool failures
        // at 165k tokens, which is above the ceiling but below the restart score.
        let mut text = String::new();
        for i in 0..12 {
            text.push_str("{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n");
            text.push_str("{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"r\",\"is_error\":true}]}}\n");
            let block = if i < 2 { "[zirv] ok" } else { "sloppy" };
            text.push_str(&format!(
                "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{block}\"}}],\"usage\":{{\"input_tokens\":165000}}}}}}\n"
            ));
        }
        std::fs::write(&transcript, &text).expect("write");

        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                ("STUB_TUI_LOG", log.display().to_string()),
                ("STUB_TUI_TRANSCRIPT", transcript.display().to_string()),
                ("ZIRV_CTX_TRANSCRIPT", transcript.display().to_string()),
                ("ZIRV_CTX_DEBOUNCE_MS", "300".to_string()),
                (
                    "ZIRV_CTX_STATE_DIR",
                    tmp.path().join("state").display().to_string(),
                ),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        // A turn boundary is what unlocks injection, and the hook is what
        // reports one, so drive it exactly the way the real hook does.
        let socket = std::fs::read_to_string(tmp.path().join("state/socket-path"))
            .unwrap_or_default();
        assert!(!socket.trim().is_empty(), "wrap must publish its socket path");
        crate::commands::ctx::signal::send(
            std::path::Path::new(socket.trim()),
            &turn_signal(3, Verdict::Compact),
        )
        .expect("send turn signal");

        let seen = read_until(&mut h.reader, "compacted", Duration::from_secs(15));
        assert!(seen.contains("compacted"), "got {seen:?}");

        let injected = std::fs::read_to_string(&log).expect("injection log");
        assert!(injected.contains("/compact"), "got {injected:?}");
        assert!(injected.contains("Preserve"), "focus text was sent: {injected:?}");
        assert_eq!(injected.lines().count(), 1, "cooldown prevents a second injection");

        h.writer.write_all(b"/exit\r").expect("write");
        let _ = h.child.wait();
    }
```

The PTY test needs two seams that the implementation must provide: `ZIRV_CTX_TRANSCRIPT` to point wrap at a known transcript (real sessions derive it from the adapter, but a stub TUI writes no session file), and a `socket-path` file in the state dir so a test can find the socket wrap bound. Both are legitimate features: the first lets users wrap an agent whose transcript lives somewhere unusual, the second is what `zirv ctx status` and external tooling read.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ctx::wrap 2>&1 | tail -25`
Expected: FAIL to compile, `cannot find enum Action`, `cannot find function inject_compact`.

- [ ] **Step 3: Write minimal implementation**

Add to `src/commands/ctx/wrap.rs`:

```rust
use super::adapters::AgentAdapter;
use super::event::NormalizedEvent;
use super::supervise::Watcher;

/// Sent as arguments to the adapter's compaction command. PreCompact hooks
/// cannot add instructions to a compaction, so this is the only channel for them.
pub const COMPACT_FOCUS: &str = "Preserve the current task and its acceptance criteria, the file paths touched so far, any unresolved errors or failing tests, and the exact next step. Drop resolved tangents and full file dumps.";

pub const TRANSCRIPT_ENV: &str = "ZIRV_CTX_TRANSCRIPT";
pub const SOCKET_PATH_FILE: &str = "socket-path";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Advise,
    Compact,
    Restart,
}

/// Advisories only print, so they need no injection window. Compaction and
/// restart type into the agent, so both preconditions apply.
pub fn action_for(state: &InjectionState, now: Instant, debounce: Duration) -> Action {
    if state.degraded {
        return Action::None;
    }
    match state.verdict {
        Verdict::Healthy => Action::None,
        Verdict::Advise => Action::Advise,
        Verdict::Compact if may_inject(state, now, debounce) => Action::Compact,
        Verdict::Restart if may_inject(state, now, debounce) => Action::Restart,
        _ => Action::None,
    }
}

pub fn advisory_line(score: u32, tokens: u64) -> String {
    format!(
        "zirv ctx: context health is slipping (score {score}, {tokens} tokens in context). A /compact soon will keep instruction-following sharp."
    )
}

pub fn inject_compact(sink: &mut dyn Write, compact_command: &str) -> CtxResult<()> {
    // A TUI submits on carriage return, not newline.
    write!(sink, "{compact_command} {COMPACT_FOCUS}\r")?;
    sink.flush()?;
    Ok(())
}

/// Watches the transcript for the compaction the injection was supposed to
/// cause. No blind keystroke retries: either it is recorded or wrap retreats.
pub fn verify_compaction(
    watcher: &mut Watcher,
    adapter: &dyn AgentAdapter,
    deadline: Instant,
) -> CtxResult<bool> {
    while Instant::now() < deadline {
        if let Some(jsonl) = watcher.read_if_changed()?
            && adapter
                .parse_events(&jsonl)
                .iter()
                .any(|event| matches!(event, NormalizedEvent::Compaction))
        {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(false)
}
```

Wire it into `run_with`. After the threads start and before `pump`:

```rust
    let state_dir = super::state::StateDir::resolve(env)?;
    let session = super::event::SessionId::new_v4();

    let mut supervision = InjectionState::new();
    supervision.degraded = args.no_supervise;

    let server = if args.no_supervise {
        None
    } else {
        match super::signal::SignalServer::bind(&state_dir.socket_for(session.as_str())) {
            Ok(server) => {
                // Publish the path so `zirv ctx status` and tests can find it.
                let _ = std::fs::create_dir_all(state_dir.root());
                let _ = std::fs::write(
                    state_dir.root().join(SOCKET_PATH_FILE),
                    server.path().display().to_string(),
                );
                Some(server)
            }
            Err(_) => {
                supervision.degraded = true;
                None
            }
        }
    };

    let transcript = env(TRANSCRIPT_ENV).map(PathBuf::from).unwrap_or_else(|| {
        adapter.transcript_path(&super::event::SessionRef {
            id: session.clone(),
            cwd: repo.to_path_buf(),
        })
    });
```

Replace the `_adapter` binding with `let adapter = ...` (it is used now), and inject the adapter's turn-signal env into the `CommandBuilder` before spawning:

```rust
    if let Some(server) = server.as_ref() {
        let setup = adapter.register_turn_signal(
            &super::event::SessionRef {
                id: session.clone(),
                cwd: repo.to_path_buf(),
            },
            server.path(),
        );
        for (key, value) in setup.env {
            command.env(key, value);
        }
    }
```

Then extend `pump` to take `&InjectionState`, the server, the adapter, the writer handle, the transcript and the config, and act on each iteration:

```rust
        while let Ok(event) = rx.try_recv() {
            if event == PumpEvent::PtyClosed {
                let status = child.wait()?;
                return Ok(status.exit_code() as i32);
            }
            supervision.on_event(event, Instant::now());
        }

        if let Some(server) = server
            && let Some(signal) = server.try_recv()
        {
            supervision.on_turn(&signal);
        }

        match action_for(supervision, Instant::now(), debounce) {
            Action::None => {}
            Action::Advise => {
                let mut stderr = std::io::stderr();
                let _ = writeln!(
                    stderr,
                    "\r\n{}\r",
                    advisory_line(supervision.score, 0)
                );
                // Advise once per turn.
                supervision.cooldown_until_turn = Some(supervision.last_turn);
            }
            Action::Compact => {
                let injected = writer
                    .lock()
                    .map_err(|_| "pty writer poisoned")
                    .and_then(|mut sink| {
                        let command = adapter.compact_command().unwrap_or("/compact");
                        inject_compact(&mut *sink, command).map_err(|_| "injection failed")
                    });

                // Arm the cooldown before verifying so a failed verification
                // cannot turn into a retry loop.
                supervision.cooldown_until_turn = Some(supervision.last_turn);

                let verified = injected.is_ok()
                    && verify_compaction(
                        &mut Watcher::new(transcript.to_path_buf()),
                        adapter,
                        Instant::now() + inject_timeout,
                    )
                    .unwrap_or(false);

                if !verified {
                    supervision.degraded = true;
                }
                let _ = super::log::append(
                    state_dir,
                    &super::log::Decision {
                        ts: super::state::now_secs(),
                        session: session.as_str(),
                        verb: "wrap",
                        verdict: "compact",
                        score: supervision.score,
                        action: if verified { "inject" } else { "inject-unverified" },
                        detail: &transcript.display().to_string(),
                    },
                );
            }
            Action::Restart => {
                // Task C5.
                supervision.cooldown_until_turn = Some(supervision.last_turn);
            }
        }
```

with `debounce = Duration::from_millis(cfg.wrap.debounce_ms)` and `inject_timeout = Duration::from_millis(cfg.wrap.inject_timeout_ms)` computed in `run_with` and passed in.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo build && cargo test ctx::wrap -- --test-threads=1 2>&1 | tail -25`
Expected: PASS, 24 tests. If the PTY injection test times out, print the decision log (`cat <tmp>/state/logs/decisions.jsonl`) inside the test to see whether the action fired and verification failed, or whether the turn signal never arrived.

- [ ] **Step 5: Commit**

```bash
git add src/commands/ctx/wrap.rs
git commit -m "feat(ctx): wrap advisory and verified compaction injection with cooldown"
```

---

### Task C5: Restart in place with a handoff

**Files:**
- Modify: `src/commands/ctx/wrap.rs`

**Interfaces:**
- Consumes: `Action::Restart` (C4); `handoff::distill_or_structural`, `store` (A18/A19); `AgentAdapter::quit_sequence`, `interactive_cmd` (A6/A8); `supervise::terminate` is not usable here (the PTY child is a `portable_pty::Child`), so a local `quit_child` handles the ladder.
- Produces:
  - `pub fn quit_child(sink: &mut dyn Write, child: &mut Box<dyn portable_pty::Child + Send + Sync>, quit_sequence: &str, grace: Duration) -> CtxResult<()>`
  - `pub fn restart_prompt(handoff: &Handoff) -> String`

The relaunch reuses the same `PtyPair`, so the slave is deliberately **not** dropped in C2 and child exit is detected with `try_wait`.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` in `src/commands/ctx/wrap.rs`:

```rust
    use crate::commands::ctx::handoff::Handoff;

    #[test]
    fn the_restart_prompt_carries_the_handoff() {
        let handoff = Handoff {
            task: "Wire the webhook".to_string(),
            next_step: "Write the failing test".to_string(),
            ..Handoff::default()
        };
        let prompt = restart_prompt(&handoff);
        assert!(prompt.contains("Wire the webhook"));
        assert!(prompt.contains("Write the failing test"));
        assert!(prompt.to_lowercase().contains("previous session"));
        assert!(!prompt.contains('\u{2014}'));
    }

    #[cfg(unix)]
    #[test]
    fn quit_child_sends_the_sequence_then_escalates() {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        // A child that ignores everything typed at it.
        let mut cmd = CommandBuilder::new("sh");
        cmd.arg("-c");
        cmd.arg("trap '' TERM; while true; do sleep 1; done");
        let mut child = pair.slave.spawn_command(cmd).expect("spawn");
        let mut sink = pair.master.take_writer().expect("writer");

        let started = Instant::now();
        quit_child(&mut sink, &mut child, "/exit\r", Duration::from_millis(200)).expect("quit");
        assert!(child.try_wait().expect("try_wait").is_some(), "child is gone");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[test]
    fn quit_child_returns_immediately_for_a_cooperative_child() {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let script = fixture("stub-tui.sh").display().to_string();
        let mut cmd = CommandBuilder::new("sh");
        cmd.arg(&script);
        let mut child = pair.slave.spawn_command(cmd).expect("spawn");
        let mut sink = pair.master.take_writer().expect("writer");

        std::thread::sleep(Duration::from_millis(200));
        quit_child(&mut sink, &mut child, "/exit\r", Duration::from_secs(5)).expect("quit");
        assert!(child.try_wait().expect("try_wait").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_restart_verdict_writes_a_handoff_and_relaunches_in_the_same_pty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let transcript = tmp.path().join("t.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"type":"user","message":{"content":"wire the webhook"}}"#,
                "\n",
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"a","name":"Read","input":{"file_path":"/work/src/hook.rs"}}],"usage":{"input_tokens":180000}}}"#,
                "\n"
            ),
        )
        .expect("write");

        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                ("ZIRV_CTX_TRANSCRIPT", transcript.display().to_string()),
                ("ZIRV_CTX_DEBOUNCE_MS", "300".to_string()),
                ("ZIRV_CTX_STATE_DIR", state.display().to_string()),
                // Relaunch runs the stub again instead of a real agent.
                ("ZIRV_CTX_AGENT_BIN", format!("sh {script}")),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        let socket = std::fs::read_to_string(state.join("socket-path")).expect("socket path");
        crate::commands::ctx::signal::send(
            std::path::Path::new(socket.trim()),
            &turn_signal(5, Verdict::Restart),
        )
        .expect("send");

        // The relaunched stub greets again in the same PTY.
        let seen = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(20));
        assert!(seen.contains("stub-tui ready"), "relaunched: {seen:?}");

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"restart\""), "got {log}");

        let handoffs: Vec<_> = walk_md(&state.join("handoffs"));
        assert_eq!(handoffs.len(), 1, "one handoff per restart: {handoffs:?}");
        let note = std::fs::read_to_string(&handoffs[0]).expect("handoff");
        assert!(note.contains("wire the webhook"), "structural task: {note}");

        h.writer.write_all(b"/exit\r").expect("write");
        let _ = h.child.wait();
    }

    fn walk_md(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return found;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(walk_md(&path));
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                found.push(path);
            }
        }
        found
    }
```

`ZIRV_CTX_AGENT_BIN` here holds `sh <script>`, which already works: Task A8 splits a multi-word bin into program plus leading arguments inside `ClaudeAdapter::new` and applies it to `headless_cmd`, `interactive_cmd` and `distiller_cmd` alike. No adapter change is needed in this task. If `a_restart_verdict_writes_a_handoff_and_relaunches_in_the_same_pty` fails with a "no such file or directory" style error, re-run `cargo test ctx::adapters::claude::tests::a_multi_word_agent_bin_is_split_across_every_command_kind` to confirm that split is in place.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ctx::wrap 2>&1 | tail -25`
Expected: FAIL to compile, `cannot find function quit_child`.

- [ ] **Step 3: Write minimal implementation**

Add to `src/commands/ctx/wrap.rs`:

```rust
use super::handoff::{self, Handoff};

pub fn restart_prompt(handoff: &Handoff) -> String {
    format!(
        "The previous session in this terminal ran out of usable context and was restarted by \
zirv ctx. Continue from the handoff below. Re-read the listed files before changing them, and \
do not redo work marked as done.\n\n{}",
        handoff.to_markdown()
    )
}

/// Ask the TUI to quit, then escalate. A TUI that will not leave politely is
/// killed rather than left running under a supervisor that has moved on.
pub fn quit_child(
    sink: &mut dyn Write,
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    quit_sequence: &str,
    grace: Duration,
) -> CtxResult<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    let _ = write!(sink, "{quit_sequence}");
    let _ = sink.flush();

    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Ctrl-C twice is the conventional escape hatch before force.
    let _ = write!(sink, "\x03\x03");
    let _ = sink.flush();
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}
```

Replace the `Action::Restart` arm in `pump`:

```rust
            Action::Restart => {
                supervision.cooldown_until_turn = Some(supervision.last_turn);

                let jsonl = std::fs::read_to_string(transcript).unwrap_or_default();
                let ctx = adapter.structural_context(&jsonl, tail_items);
                let (note, source) =
                    handoff::distill_or_structural(adapter, distiller_model, &ctx);
                let stored = handoff::store(state_dir, repo, session.as_str(), &note);

                let quit = writer
                    .lock()
                    .map_err(|_| "pty writer poisoned".to_string())
                    .and_then(|mut sink| {
                        quit_child(&mut *sink, child, adapter.quit_sequence(), grace)
                            .map_err(|e| e.to_string())
                    });

                let relaunched = quit.is_ok()
                    && {
                        let mut command = adapter.interactive_cmd(Some(&restart_prompt(&note)), &[]);
                        command.current_dir(repo);
                        let mut builder = CommandBuilder::new(command.get_program());
                        for arg in command.get_args() {
                            builder.arg(arg);
                        }
                        builder.cwd(repo);
                        match pair.slave.spawn_command(builder) {
                            Ok(fresh) => {
                                *child = fresh;
                                true
                            }
                            Err(_) => false,
                        }
                    };

                if !relaunched {
                    supervision.degraded = true;
                }
                let _ = super::log::append(
                    state_dir,
                    &super::log::Decision {
                        ts: super::state::now_secs(),
                        session: session.as_str(),
                        verb: "wrap",
                        verdict: "restart",
                        score: supervision.score,
                        action: if relaunched { "restart" } else { "restart-failed" },
                        detail: &match stored {
                            Ok(path) => format!("{source} handoff at {}", path.display()),
                            Err(e) => format!("{source} handoff not stored: {e}"),
                        },
                    },
                );
                if !relaunched {
                    let status = child.wait()?;
                    return Ok(status.exit_code() as i32);
                }
            }
```

`pump` now also needs `pair`, `repo`, `tail_items` (from `cfg.handoff.tail_items`), `distiller_model` (from `cfg.handoff.model`) and `grace`; pass them from `run_with` and keep `#[allow(clippy::too_many_arguments)]` on `pump`. Do **not** drop `pair.slave` in `run_with`: the relaunch spawns from it, and child exit is detected with `try_wait`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo build && cargo test ctx::wrap -- --test-threads=1 2>&1 | tail -25`
Expected: PASS, 28 tests.

- [ ] **Step 5: Commit**

```bash
git add src/commands/ctx/wrap.rs
git commit -m "feat(ctx): wrap restarts the TUI in place with a distilled handoff"
```

---

### Task C6: Degradation to pure passthrough

**Files:**
- Modify: `src/commands/ctx/wrap.rs`

**Interfaces:**
- Consumes: everything from C2 to C5.
- Produces: `pub fn note_failure(state: &mut InjectionState, log_target: Option<(&StateDir, &str)>, what: &str)` and the guarantee, asserted by tests, that a wrapped session with broken supervision behaves exactly like an unwrapped one.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` in `src/commands/ctx/wrap.rs`:

```rust
    #[test]
    fn noting_a_failure_degrades_the_supervisor_once_and_for_all() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().to_path_buf());
        let mut supervision = InjectionState::new();

        note_failure(&mut supervision, Some((&state, "sess")), "socket died");
        assert!(supervision.degraded);

        // Even a fresh turn signal cannot re-enable injection.
        supervision.on_turn(&turn_signal(9, Verdict::Restart));
        supervision.last_output = Instant::now() - Duration::from_secs(30);
        assert_eq!(action_for(&supervision, Instant::now(), DEBOUNCE), Action::None);

        let log = std::fs::read_to_string(state.logs().join("decisions.jsonl")).expect("log");
        assert!(log.contains("degrade"), "got {log}");
        assert!(log.contains("socket died"), "record the reason: {log}");
    }

    #[test]
    fn note_failure_without_a_state_dir_still_degrades() {
        let mut supervision = InjectionState::new();
        note_failure(&mut supervision, None, "no state dir");
        assert!(supervision.degraded);
    }

    #[cfg(unix)]
    #[test]
    fn an_unbindable_socket_leaves_a_fully_transparent_wrapper() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // A state dir path long enough that the socket path exceeds the limit.
        let long_state = tmp.path().join("s".repeat(120));
        let script = fixture("stub-tui.sh").display().to_string();

        let mut h = spawn_wrap(
            &[("ZIRV_CTX_STATE_DIR", long_state.display().to_string())],
            &["sh", &script],
        );
        let seen = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));
        assert!(seen.contains("stub-tui ready"), "got {seen:?}");

        h.writer.write_all(b"hello\r").expect("write");
        h.writer.flush().expect("flush");
        let echoed = read_until(&mut h.reader, "echo: hello", Duration::from_secs(10));
        assert!(echoed.contains("echo: hello"), "passthrough intact: {echoed:?}");

        h.writer.write_all(b"/exit\r").expect("write");
        let status = h.child.wait().expect("wait");
        assert_eq!(status.exit_code(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_broken_transcript_path_never_stops_the_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                (
                    "ZIRV_CTX_TRANSCRIPT",
                    "/nonexistent/dir/t.jsonl".to_string(),
                ),
                ("ZIRV_CTX_DEBOUNCE_MS", "200".to_string()),
                (
                    "ZIRV_CTX_STATE_DIR",
                    tmp.path().join("state").display().to_string(),
                ),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        let socket = std::fs::read_to_string(tmp.path().join("state/socket-path")).expect("path");
        crate::commands::ctx::signal::send(
            std::path::Path::new(socket.trim()),
            &turn_signal(2, Verdict::Compact),
        )
        .expect("send");

        // The injection is attempted and cannot be verified, so wrap degrades
        // while the session continues.
        h.writer.write_all(b"still here\r").expect("write");
        h.writer.flush().expect("flush");
        let echoed = read_until(&mut h.reader, "echo: still here", Duration::from_secs(20));
        assert!(echoed.contains("echo: still here"), "got {echoed:?}");

        h.writer.write_all(b"/exit\r").expect("write");
        let status = h.child.wait().expect("wait");
        assert_eq!(status.exit_code(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn no_supervise_skips_supervision_entirely() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let script = fixture("stub-tui.sh").display().to_string();

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = CommandBuilder::new(zirv_bin());
        cmd.arg("ctx");
        cmd.arg("wrap");
        cmd.arg("--agent");
        cmd.arg("claude");
        cmd.arg("--no-supervise");
        cmd.arg("--");
        cmd.arg("sh");
        cmd.arg(&script);
        cmd.env("TERM", "xterm");
        cmd.env(
            "ZIRV_CTX_STATE_DIR",
            tmp.path().join("state").display().to_string(),
        );
        let mut child = pair.slave.spawn_command(cmd).expect("spawn");
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().expect("reader");
        let mut writer = pair.master.take_writer().expect("writer");

        let seen = read_until(&mut reader, "stub-tui ready", Duration::from_secs(10));
        assert!(seen.contains("stub-tui ready"));
        assert!(
            !tmp.path().join("state/socket-path").exists(),
            "no socket is bound when supervision is off"
        );

        writer.write_all(b"/exit\r").expect("write");
        let status = child.wait().expect("wait");
        assert_eq!(status.exit_code(), 0);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ctx::wrap 2>&1 | tail -25`
Expected: FAIL to compile, `cannot find function note_failure`.

- [ ] **Step 3: Write minimal implementation**

Add to `src/commands/ctx/wrap.rs`:

```rust
use super::state::StateDir;

/// One-way switch to pure passthrough. Once supervision has proven unreliable
/// in a session it stays off: a wrapped session must never be worse than an
/// unwrapped one.
pub fn note_failure(
    state: &mut InjectionState,
    log_target: Option<(&StateDir, &str)>,
    what: &str,
) {
    state.degraded = true;
    if let Some((state_dir, session)) = log_target {
        let _ = super::log::append(
            state_dir,
            &super::log::Decision {
                ts: super::state::now_secs(),
                session,
                verb: "wrap",
                verdict: "n/a",
                score: 0,
                action: "degrade",
                detail: what,
            },
        );
    }
}
```

Then replace every direct `supervision.degraded = true;` from C4 and C5 with a `note_failure` call carrying a specific reason (`"compaction not verified"`, `"relaunch failed"`, `"pty writer poisoned"`), and do the same for the socket-bind failure in `run_with` (`"socket unavailable"`) and for a `StateDir::resolve` failure (pass `None` as the log target there, since there is nowhere to log).

Also confirm that every arm of `pump` that returns early passes through `run_with`'s explicit `raw.restore()` call. That is the `panic = "abort"` requirement: grep for `return Ok(` inside `pump` and check each one exits into `run_with` rather than calling `std::process::exit`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo build && cargo test ctx::wrap -- --test-threads=1 2>&1 | tail -25`
Expected: PASS, 33 tests.

- [ ] **Step 5: Run the whole suite and the lints**

Run: `cargo test --verbose -- --test-threads=1 2>&1 | tail -15`
Expected: PASS.
Run: `cargo fmt -- --check && cargo clippy --all-targets -- -D warnings 2>&1 | tail -10`
Expected: clean.

- [ ] **Step 6: Verify no `unwrap` or `expect` survives on the hot path**

Run: `grep -nE '\.unwrap\(\)|\.expect\(' src/commands/ctx/wrap.rs src/commands/ctx/term.rs | grep -v '#\[cfg(test)\]'`
Expected: no output outside the `mod tests` block. If a line appears in production code, replace it with a `?`, an `unwrap_or`, or a `note_failure` degradation.

- [ ] **Step 7: Verify transparency by hand one more time**

Run: `cargo run --quiet -- ctx wrap -- sh -c 'echo transparent; sleep 1'` then `stty -a | head -2`
Expected: the output appears, and `echo` is still set afterwards, proving raw mode was restored through the normal exit path.

- [ ] **Step 8: Commit**

```bash
git add src/commands/ctx/wrap.rs
git commit -m "feat(ctx): wrap degrades to pure passthrough on any supervision failure"
```

---

# Phase E: Usage pacing

Ships in the same 2.5.0, after Phase C and before Task D1. Autonomous loops must never die mid-run because a subscription usage window ran dry, so supervised work is paced to keep a window at or below `pace_max_percent`.

Every task here builds only on `docs/superpowers/notes/2026-07-31-claude-usage-window-facts.md`. Two facts in that file are **BLOCKED** and get the Task A9 treatment (verify first, ship docs-verified with an empirical follow-up, never guess):

- whether the subscription limiter weights token classes identically: the estimator is labeled an approximation, never overrides fresher collector data, and is **off by default** until the operator sets a token budget;
- the machine-readable shape of a limit-hit: the matcher ships against the three documented strings only, with a follow-up to confirm empirically.

Facts these tasks rely on, all verified in that notes file: no local usage state exists and no headless query exists (so the statusline is the only collector); statusline fields are `rate_limits.five_hour.used_percentage` / `.resets_at` and `rate_limits.seven_day.*`, subscriber-only and each window independently absent; every assistant transcript event carries `usage`; subagent turns live in separate `<session>/subagents/*.jsonl` files that a machine-wide sum must walk.

### Task E1: Usage state file and the `zirv ctx usage tee` statusline hook

**Files:**
- Create: `src/commands/ctx/window.rs`
- Create: `src/commands/ctx/usage.rs`
- Create: `tests/fixtures/statusline-with-limits.json`
- Create: `tests/fixtures/statusline-no-limits.json`
- Create: `tests/fixtures/fake-statusline.sh`
- Modify: `src/commands/ctx/state.rs:61-89` (add `usage()`)
- Modify: `src/commands/ctx/mod.rs:3-19` (declare `usage`, `window`), `:37-55` (`CtxVerb::Usage`), `:69-78` (dispatch arm)

**Interfaces:**
- Consumes, all already implemented and read from the tree: `CtxResult` (`mod.rs:23`), `StateDir::{from_root, resolve, root}` and `state::now_secs` (`state.rs:15,41,47,57`), `EnvLookup` and `env_from_process` (`config.rs:14,18`), the verb convention `pub fn run<W: Write>(args, w) -> CtxResult<i32>` plus a `run_with(args, w, repo, env)` seam (as in `status.rs:16,72`).
- Produces:
  - `state.rs`: `pub fn usage(&self) -> PathBuf` returning `<root>/usage.json`
  - `window.rs`: `pub struct Window { pub used_percentage: f64, pub resets_at: u64, pub observed_at: u64 }` (`Debug, Clone, Copy, PartialEq, Serialize, Deserialize`); `pub struct UsageWindows { pub five_hour: Option<Window>, pub seven_day: Option<Window> }` (`Debug, Clone, Default, PartialEq, Serialize, Deserialize`, `#[serde(default)]`); `pub fn parse_statusline(json: &str, observed_at: u64) -> Option<UsageWindows>`; `pub fn load(state: &StateDir) -> UsageWindows`; `pub fn store(state: &StateDir, windows: &UsageWindows) -> CtxResult<()>`; `pub fn merge(existing: UsageWindows, fresh: UsageWindows) -> UsageWindows`; `pub fn age_secs(window: &Window, now: u64) -> u64`
  - `usage.rs`: `pub struct UsageArgs { pub action: Option<UsageAction> }`; `pub enum UsageAction { Tee { command: Vec<String> } }`; `pub fn run_tee<W: Write>(w: &mut W, stdin_text: &str, command: &[String], state: Option<&StateDir>, now: u64) -> i32`; `pub fn fallback_line(json: &str) -> String`

- [ ] **Step 1: Write the fixtures**

Create `tests/fixtures/statusline-with-limits.json` (shape copied from the documented fields in the notes file):

```json
{
  "hook_event_name": "Status",
  "session_id": "11111111-2222-4333-8444-555555555555",
  "cwd": "/home/testuser/repo",
  "model": { "id": "claude-fable-5", "display_name": "Fable 5" },
  "workspace": { "current_dir": "/home/testuser/repo" },
  "context_window": { "used_percentage": 42 },
  "rate_limits": {
    "five_hour": { "used_percentage": 87.5, "resets_at": 1785000000 },
    "seven_day": { "used_percentage": 31, "resets_at": 1785400000 }
  }
}
```

Create `tests/fixtures/statusline-no-limits.json` (a non-subscriber or pre-first-response session, which the notes file records as normal):

```json
{
  "hook_event_name": "Status",
  "session_id": "11111111-2222-4333-8444-555555555555",
  "cwd": "/home/testuser/repo",
  "model": { "id": "claude-fable-5", "display_name": "Fable 5" },
  "workspace": { "current_dir": "/home/testuser/repo" },
  "context_window": { "used_percentage": 42 }
}
```

Create `tests/fixtures/fake-statusline.sh` and `chmod +x` it:

```sh
#!/bin/sh
# Stands in for the user's real statusline script during tee tests.
#   FAKE_STATUSLINE_MODE=ok|fail|silent   (default ok)
# ok      echoes a recognizable line built from the JSON it received on stdin
# fail    exits non-zero without printing, so the fallback path is exercised
# silent  exits 0 printing nothing
set -eu
input=$(cat)
case "${FAKE_STATUSLINE_MODE:-ok}" in
  fail) exit 7 ;;
  silent) exit 0 ;;
  *)
    [ -z "${FAKE_STATUSLINE_LOG:-}" ] || printf '%s' "$input" > "$FAKE_STATUSLINE_LOG"
    printf 'CHAINED-OK bytes=%s\n' "$(printf '%s' "$input" | wc -c | tr -d ' ')"
    ;;
esac
```

Run: `chmod +x tests/fixtures/fake-statusline.sh && printf '{"a":1}' | ./tests/fixtures/fake-statusline.sh`
Expected: a line starting `CHAINED-OK bytes=`.

- [ ] **Step 2: Write the failing state-file test**

Add to the existing `mod tests` in `src/commands/ctx/state.rs`:

```rust
    #[test]
    fn the_usage_file_hangs_off_the_state_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        assert_eq!(state.usage(), tmp.path().join("usage.json"));
    }
```

- [ ] **Step 3: Run it and see it fail**

Run: `cargo test ctx::state 2>&1 | tail -20`
Expected: FAIL to compile, `no method named usage found for struct StateDir`.

- [ ] **Step 4: Add the accessor**

In `src/commands/ctx/state.rs`, next to `logs()`:

```rust
    /// Machine-wide usage-window state, shared by every session that runs the
    /// statusline tee. One file, not per-session: the windows are per account.
    pub fn usage(&self) -> PathBuf {
        self.0.join("usage.json")
    }
```

- [ ] **Step 5: Run it and see it pass**

Run: `cargo test ctx::state 2>&1 | tail -20`
Expected: PASS, 5 tests.

- [ ] **Step 6: Write the failing window tests**

Create `src/commands/ctx/window.rs` containing only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::state::StateDir;

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    #[test]
    fn documented_rate_limit_fields_are_parsed() {
        let json = std::fs::read_to_string(fixture("statusline-with-limits.json")).expect("fixture");
        let windows = parse_statusline(&json, 1_784_999_000).expect("rate_limits present");

        let five = windows.five_hour.expect("five_hour");
        assert_eq!(five.used_percentage, 87.5);
        assert_eq!(five.resets_at, 1_785_000_000);
        assert_eq!(five.observed_at, 1_784_999_000);

        let seven = windows.seven_day.expect("seven_day");
        assert_eq!(seven.used_percentage, 31.0, "integer percentages parse as floats");
        assert_eq!(seven.resets_at, 1_785_400_000);
    }

    #[test]
    fn a_statusline_without_rate_limits_yields_nothing_to_persist() {
        let json = std::fs::read_to_string(fixture("statusline-no-limits.json")).expect("fixture");
        assert_eq!(
            parse_statusline(&json, 1_784_999_000),
            None,
            "non-subscriber and pre-first-response sessions are normal, not errors"
        );
    }

    #[test]
    fn each_window_may_be_independently_absent() {
        let only_five = "{\"rate_limits\":{\"five_hour\":{\"used_percentage\":10,\"resets_at\":5}}}";
        let windows = parse_statusline(only_five, 1).expect("five_hour present");
        assert!(windows.five_hour.is_some());
        assert!(windows.seven_day.is_none());
    }

    #[test]
    fn a_window_missing_resets_at_is_still_usable_for_its_percentage() {
        let json = "{\"rate_limits\":{\"five_hour\":{\"used_percentage\":99.9}}}";
        let five = parse_statusline(json, 7).expect("parsed").five_hour.expect("five");
        assert_eq!(five.used_percentage, 99.9);
        assert_eq!(five.resets_at, 0, "zero means unknown, callers use the fallback delay");
    }

    #[test]
    fn garbage_input_parses_to_nothing_rather_than_erroring() {
        assert_eq!(parse_statusline("not json at all", 1), None);
        assert_eq!(parse_statusline("", 1), None);
        assert_eq!(parse_statusline("{\"rate_limits\":\"nope\"}", 1), None);
    }

    #[test]
    fn state_round_trips_through_the_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());

        assert_eq!(load(&state), UsageWindows::default(), "absent file is empty state");

        let windows = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 50.0,
                resets_at: 100,
                observed_at: 10,
            }),
            seven_day: None,
        };
        store(&state, &windows).expect("store");
        assert_eq!(load(&state), windows);
    }

    #[test]
    fn a_corrupt_state_file_reads_as_empty_instead_of_failing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        std::fs::create_dir_all(state.root()).expect("mkdir");
        std::fs::write(state.usage(), "{ this is not json").expect("write");
        assert_eq!(load(&state), UsageWindows::default());
    }

    #[test]
    fn store_leaves_no_partial_file_behind() {
        // Concurrent live sessions all write this file, so the write is atomic:
        // a temp file plus rename, never a truncate-then-write.
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        store(&state, &UsageWindows::default()).expect("store");
        let strays: Vec<_> = std::fs::read_dir(state.root())
            .expect("read_dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name != "usage.json")
            .collect();
        assert!(strays.is_empty(), "temp file was not cleaned up: {strays:?}");
    }

    #[test]
    fn merging_keeps_the_newest_observation_per_window() {
        let old = UsageWindows {
            five_hour: Some(Window { used_percentage: 10.0, resets_at: 100, observed_at: 10 }),
            seven_day: Some(Window { used_percentage: 20.0, resets_at: 200, observed_at: 10 }),
        };
        let fresh = UsageWindows {
            five_hour: Some(Window { used_percentage: 90.0, resets_at: 300, observed_at: 50 }),
            seven_day: None,
        };

        let merged = merge(old, fresh);
        assert_eq!(merged.five_hour.expect("five").used_percentage, 90.0);
        assert_eq!(
            merged.seven_day.expect("seven").used_percentage,
            20.0,
            "an absent window in a fresh reading must not erase what is known"
        );
    }

    #[test]
    fn merging_never_moves_a_window_backwards_in_time() {
        let newer = UsageWindows {
            five_hour: Some(Window { used_percentage: 90.0, resets_at: 300, observed_at: 50 }),
            seven_day: None,
        };
        let stale = UsageWindows {
            five_hour: Some(Window { used_percentage: 5.0, resets_at: 100, observed_at: 10 }),
            seven_day: None,
        };
        let merged = merge(newer, stale);
        assert_eq!(
            merged.five_hour.expect("five").used_percentage,
            90.0,
            "a late-arriving stale sample must not win"
        );
    }

    #[test]
    fn age_is_measured_from_the_observation() {
        let window = Window { used_percentage: 1.0, resets_at: 0, observed_at: 100 };
        assert_eq!(age_secs(&window, 160), 60);
        assert_eq!(age_secs(&window, 90), 0, "clock skew reads as fresh, not negative");
    }
}
```

- [ ] **Step 7: Run them and see them fail**

Run: `cargo test ctx::window 2>&1 | tail -20`
Expected: FAIL. First `module window not found` until `mod.rs` declares it, then `cannot find function parse_statusline`.

- [ ] **Step 8: Write the window implementation**

Add `pub mod usage;` and `pub mod window;` to the module list in `src/commands/ctx/mod.rs` (alphabetical: after `supervise`, `term`, then `usage`, `window`). Then put this above the test module in `src/commands/ctx/window.rs`:

```rust
// Consumed by the usage verb and the pacing gate in later tasks of this plan.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::CtxResult;
use super::state::StateDir;

/// One subscription window as last reported by the collector. `resets_at` is a
/// unix epoch second; `0` means the field was absent and callers must fall back.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub used_percentage: f64,
    pub resets_at: u64,
    pub observed_at: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageWindows {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
}

/// A window needs a percentage to be useful. `resets_at` may be absent, and `0`
/// is the documented "unknown" marker callers fall back on.
fn window_at(node: Option<&Value>, observed_at: u64) -> Option<Window> {
    let node = node?;
    let used_percentage = node.get("used_percentage").and_then(Value::as_f64)?;
    Some(Window {
        used_percentage,
        resets_at: node.get("resets_at").and_then(Value::as_u64).unwrap_or(0),
        observed_at,
    })
}

/// Reads the documented statusline `rate_limits` block. `None` means there was
/// nothing to persist, which is the normal case for non-subscribers and for the
/// first statusline of a session, so it is never an error.
pub fn parse_statusline(json: &str, observed_at: u64) -> Option<UsageWindows> {
    let value: Value = serde_json::from_str(json).ok()?;
    let limits = value.get("rate_limits")?;
    if !limits.is_object() {
        return None;
    }

    let windows = UsageWindows {
        five_hour: window_at(limits.get("five_hour"), observed_at),
        seven_day: window_at(limits.get("seven_day"), observed_at),
    };
    if windows.five_hour.is_none() && windows.seven_day.is_none() {
        return None;
    }
    Some(windows)
}

/// Never fails: an absent or corrupt file reads as "nothing known", because a
/// statusline hook must not break on a half-written state file.
pub fn load(state: &StateDir) -> UsageWindows {
    std::fs::read_to_string(state.usage())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Atomic: every live session's statusline writes this file, so a reader must
/// never observe a truncated one.
pub fn store(state: &StateDir, windows: &UsageWindows) -> CtxResult<()> {
    let target = state.usage();
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = target.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&temp, serde_json::to_string(windows)?)?;
    std::fs::rename(&temp, &target)?;
    Ok(())
}

fn newer(existing: Option<Window>, fresh: Option<Window>) -> Option<Window> {
    match (existing, fresh) {
        (Some(existing), Some(fresh)) if fresh.observed_at >= existing.observed_at => Some(fresh),
        (Some(existing), Some(_)) => Some(existing),
        (None, fresh) => fresh,
        (existing, None) => existing,
    }
}

/// Per-window merge. Each window may be independently absent from any given
/// statusline payload, so an absent window never erases a known one.
pub fn merge(existing: UsageWindows, fresh: UsageWindows) -> UsageWindows {
    UsageWindows {
        five_hour: newer(existing.five_hour, fresh.five_hour),
        seven_day: newer(existing.seven_day, fresh.seven_day),
    }
}

pub fn age_secs(window: &Window, now: u64) -> u64 {
    now.saturating_sub(window.observed_at)
}
```

- [ ] **Step 9: Run them and see them pass**

Run: `cargo test ctx::window 2>&1 | tail -20`
Expected: PASS, 11 tests.

- [ ] **Step 10: Write the failing tee tests**

Create `src/commands/ctx/usage.rs` containing only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::state::StateDir;
    use crate::commands::ctx::window;

    fn fixture(name: &str) -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    fn statusline_script() -> Vec<String> {
        vec![
            "sh".to_string(),
            fixture("fake-statusline.sh").display().to_string(),
        ]
    }

    #[test]
    fn tee_persists_the_windows_and_chains_the_original_command() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let json = std::fs::read_to_string(fixture("statusline-with-limits.json")).expect("fixture");

        let mut out = Vec::new();
        let code = run_tee(&mut out, &json, &statusline_script(), Some(&state), 1_784_999_000);
        assert_eq!(code, 0);

        let printed = String::from_utf8(out).expect("utf8");
        assert!(printed.contains("CHAINED-OK"), "chained output must reach the terminal: {printed}");

        let stored = window::load(&state);
        assert_eq!(stored.five_hour.expect("five_hour").used_percentage, 87.5);
        assert_eq!(stored.seven_day.expect("seven_day").resets_at, 1_785_400_000);
    }

    #[test]
    fn the_chained_command_receives_the_original_json_on_stdin() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let log = tmp.path().join("seen.json");
        let json = std::fs::read_to_string(fixture("statusline-with-limits.json")).expect("fixture");

        // SAFETY: CI runs tests single-threaded.
        unsafe {
            std::env::set_var("FAKE_STATUSLINE_LOG", &log);
        }
        let mut out = Vec::new();
        run_tee(&mut out, &json, &statusline_script(), None, 1);
        unsafe {
            std::env::remove_var("FAKE_STATUSLINE_LOG");
        }

        let seen = std::fs::read_to_string(&log).expect("chained command ran");
        assert_eq!(seen, json, "the payload must pass through byte for byte");
    }

    #[test]
    fn a_payload_without_rate_limits_still_chains_and_writes_no_state() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let json = std::fs::read_to_string(fixture("statusline-no-limits.json")).expect("fixture");

        let mut out = Vec::new();
        let code = run_tee(&mut out, &json, &statusline_script(), Some(&state), 1);
        assert_eq!(code, 0);
        assert!(String::from_utf8_lossy(&out).contains("CHAINED-OK"));
        assert!(!state.usage().exists(), "nothing to record means no file: {:?}", state.usage());
    }

    #[test]
    fn a_failing_chained_command_still_produces_a_statusline() {
        let json = std::fs::read_to_string(fixture("statusline-with-limits.json")).expect("fixture");
        unsafe {
            std::env::set_var("FAKE_STATUSLINE_MODE", "fail");
        }
        let mut out = Vec::new();
        let code = run_tee(&mut out, &json, &statusline_script(), None, 1);
        unsafe {
            std::env::remove_var("FAKE_STATUSLINE_MODE");
        }

        assert_eq!(code, 0, "a broken statusline script must not break the statusline");
        let printed = String::from_utf8(out).expect("utf8");
        assert!(!printed.trim().is_empty(), "fallback line expected");
        assert!(printed.contains("Fable 5"), "fallback names the model: {printed}");
    }

    #[test]
    fn a_missing_chained_binary_falls_back_instead_of_erroring() {
        let json = std::fs::read_to_string(fixture("statusline-with-limits.json")).expect("fixture");
        let mut out = Vec::new();
        let code = run_tee(
            &mut out,
            &json,
            &["/nonexistent/statusline".to_string()],
            None,
            1,
        );
        assert_eq!(code, 0);
        assert!(String::from_utf8_lossy(&out).contains("Fable 5"));
    }

    #[test]
    fn no_chained_command_means_the_fallback_is_the_statusline() {
        let json = std::fs::read_to_string(fixture("statusline-with-limits.json")).expect("fixture");
        let mut out = Vec::new();
        let code = run_tee(&mut out, &json, &[], None, 1);
        assert_eq!(code, 0);
        let printed = String::from_utf8(out).expect("utf8");
        assert!(printed.contains("Fable 5"));
        assert!(printed.contains("42"), "context percentage carries through: {printed}");
    }

    #[test]
    fn an_unwritable_state_dir_never_breaks_the_statusline() {
        let json = std::fs::read_to_string(fixture("statusline-with-limits.json")).expect("fixture");
        let state = StateDir::from_root(PathBuf::from("/proc/nonexistent/zirv-ctx"));
        let mut out = Vec::new();
        let code = run_tee(&mut out, &json, &statusline_script(), Some(&state), 1);
        assert_eq!(code, 0);
        assert!(String::from_utf8_lossy(&out).contains("CHAINED-OK"));
    }

    #[test]
    fn garbage_on_stdin_is_passed_through_untouched() {
        let mut out = Vec::new();
        let code = run_tee(&mut out, "this is not json", &statusline_script(), None, 1);
        assert_eq!(code, 0);
        assert!(String::from_utf8_lossy(&out).contains("CHAINED-OK"));
    }

    #[test]
    fn the_fallback_line_is_plain_and_has_no_em_dash() {
        let json = std::fs::read_to_string(fixture("statusline-with-limits.json")).expect("fixture");
        let line = fallback_line(&json);
        assert_eq!(line.lines().count(), 1);
        assert!(!line.contains('\u{2014}'));
        assert_eq!(fallback_line("garbage").lines().count(), 1, "always exactly one line");
    }

    #[test]
    fn tee_parses_as_a_subcommand_with_a_trailing_command() {
        let cli = crate::commands::ctx::CtxCli::try_parse_from([
            "zirv ctx",
            "usage",
            "tee",
            "--",
            "bash",
            "~/.claude/statusline-command.sh",
        ])
        .expect("usage tee should parse");
        match cli.verb {
            crate::commands::ctx::CtxVerb::Usage(args) => match args.action {
                Some(UsageAction::Tee { command }) => assert_eq!(
                    command,
                    vec![
                        "bash".to_string(),
                        "~/.claude/statusline-command.sh".to_string()
                    ]
                ),
                other => panic!("expected Tee, got {other:?}"),
            },
            other => panic!("expected Usage, got {other:?}"),
        }
    }
}
```

- [ ] **Step 11: Run them and see them fail**

Run: `cargo test ctx::usage 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find function run_tee`.

- [ ] **Step 12: Write the tee implementation**

Above the test module in `src/commands/ctx/usage.rs`:

```rust
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use clap::Parser;
use serde_json::Value;

use super::config::{EnvLookup, env_from_process};
use super::state::{StateDir, now_secs};
use super::{CtxResult, window};

#[derive(Debug, clap::Args)]
pub struct UsageArgs {
    #[command(subcommand)]
    pub action: Option<UsageAction>,
}

#[derive(Debug, clap::Subcommand)]
pub enum UsageAction {
    /// Statusline wrapper: record usage windows, then run the original command.
    Tee {
        /// The original statusline command, after `--`.
        //
        // `allow_hyphen_values` + `last` without `trailing_var_arg`, matching
        // `ExecArgs::command`: adding `trailing_var_arg` trips a clap debug
        // assertion that aborts the process instead of erroring.
        #[arg(allow_hyphen_values = true, last = true)]
        command: Vec<String>,
    },
}

/// Last-resort statusline: enough context to keep the line useful when the
/// chained command is missing or broken.
pub fn fallback_line(json: &str) -> String {
    let value: Value = serde_json::from_str(json).unwrap_or(Value::Null);
    let model = value
        .get("model")
        .and_then(|m| m.get("display_name"))
        .and_then(Value::as_str)
        .unwrap_or("claude");
    let context = value
        .get("context_window")
        .and_then(|c| c.get("used_percentage"))
        .and_then(Value::as_f64);

    match context {
        Some(percent) => format!("{model} | context {}%", percent.round() as i64),
        None => format!("{model}"),
    }
}

/// Never returns non-zero and never returns without emitting a statusline:
/// Claude Code shows whatever this prints, so a silent failure would look like
/// a broken terminal to the user.
pub fn run_tee<W: Write>(
    w: &mut W,
    stdin_text: &str,
    command: &[String],
    state: Option<&StateDir>,
    now: u64,
) -> i32 {
    // Persisting is best-effort and happens first, so a broken statusline
    // script cannot cost us the reading.
    if let (Some(state), Some(fresh)) = (state, window::parse_statusline(stdin_text, now)) {
        let merged = window::merge(window::load(state), fresh);
        let _ = window::store(state, &merged);
    }

    let chained = run_chained(stdin_text, command);
    match chained {
        Some(output) if !output.trim().is_empty() => {
            let _ = write!(w, "{output}");
        }
        _ => {
            let _ = writeln!(w, "{}", fallback_line(stdin_text));
        }
    }
    0
}

/// `None` when there is no command, it could not start, or it failed.
fn run_chained(stdin_text: &str, command: &[String]) -> Option<String> {
    let (program, rest) = command.split_first()?;
    let mut child = Command::new(program)
        .args(rest)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .ok()?;

    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(stdin_text.as_bytes());
    }
    drop(child.stdin.take());

    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

fn read_stdin() -> String {
    let mut buffer = String::new();
    let _ = std::io::stdin().read_to_string(&mut buffer);
    buffer
}

pub fn run_with<W: Write>(
    args: &UsageArgs,
    w: &mut W,
    repo: &std::path::Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    match &args.action {
        Some(UsageAction::Tee { command }) => {
            let state = StateDir::resolve(env).ok();
            Ok(run_tee(w, &read_stdin(), command, state.as_ref(), now_secs()))
        }
        // The human-readable report arrives in Task E5.
        None => {
            let _ = repo;
            Err("zirv ctx usage reporting is not implemented yet".into())
        }
    }
}

pub fn run<W: Write>(args: &UsageArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &repo, &env)
}
```

The `use clap::Parser;` import is only needed by the test that calls `CtxCli::try_parse_from`; if clippy flags it as unused in the non-test build, move it into the test module instead.

Wire the verb in `src/commands/ctx/mod.rs`: add to `CtxVerb`

```rust
    /// Report usage windows, or tee the statusline to record them.
    Usage(usage::UsageArgs),
```

and to the dispatch match

```rust
        CtxVerb::Usage(a) => usage::run(a, &mut out),
```

- [ ] **Step 13: Run them and see them pass**

Run: `cargo test ctx::usage -- --test-threads=1 2>&1 | tail -20`
Expected: PASS, 10 tests. If `an_unwritable_state_dir_never_breaks_the_statusline` fails on macOS because `/proc` does not exist, that is still an unwritable path and the assertion holds; if the path happens to be creatable in your environment, point it at `/dev/null/zirv-ctx` instead.

- [ ] **Step 14: Verify against the real statusline by hand**

Run:

```bash
cargo build
T=$(ls -t ~/.claude/projects/*/*.jsonl | head -1)
printf '{"model":{"display_name":"Fable 5"},"context_window":{"used_percentage":42},"cwd":"%s"}' "$PWD" \
  | ZIRV_CTX_STATE_DIR=/tmp/zirv-usage-probe ./target/debug/zirv ctx usage tee -- bash ~/.claude/statusline-command.sh
echo "exit=$?"
cat /tmp/zirv-usage-probe/usage.json 2>/dev/null || echo "no rate_limits in that payload, as expected"
```

Expected: the real statusline renders exactly as it does today, `exit=0`, and no `usage.json` (the synthetic payload carries no `rate_limits`). This is the safety property that matters: the tee is transparent.

- [ ] **Step 15: Commit**

```bash
git add tests/fixtures/statusline-with-limits.json tests/fixtures/statusline-no-limits.json tests/fixtures/fake-statusline.sh src/commands/ctx/window.rs src/commands/ctx/usage.rs src/commands/ctx/state.rs src/commands/ctx/mod.rs
git commit -m "feat(ctx): collect usage windows through a transparent statusline tee"
```

---

### Task E2: Transcript-sum estimator

**Files:**
- Modify: `src/commands/ctx/window.rs`

**Interfaces:**
- Consumes: `Window`, `UsageWindows` (E1); `crate::utils::home_dir()` (`src/utils.rs:16`, already reused by `config.rs:250`).
- Produces:
  - `pub const FIVE_HOUR_SECS: u64 = 18_000;` and `pub const SEVEN_DAY_SECS: u64 = 604_800;`
  - `pub fn parse_iso8601_utc(ts: &str) -> Option<u64>`
  - `pub fn usage_tokens_of(usage: &serde_json::Value, count_cache_reads: bool) -> u64`
  - `pub struct TokenSums { pub five_hour: u64, pub seven_day: u64, pub oldest_in_five_hour: u64, pub oldest_in_seven_day: u64, pub files_scanned: usize, pub events_counted: usize }` (`Debug, Clone, Copy, PartialEq, Default`)
  - `pub fn sum_file(jsonl: &str, now: u64, count_cache_reads: bool, into: &mut TokenSums)`
  - `pub fn projects_root() -> CtxResult<PathBuf>` returning `~/.claude/projects`
  - `pub fn sum_transcripts(projects_root: &Path, now: u64, count_cache_reads: bool) -> TokenSums`
  - `pub fn estimate_windows(sums: &TokenSums, now: u64, five_hour_budget: u64, seven_day_budget: u64) -> UsageWindows`

Deliberate choices, both forced by the BLOCKED fact that class weighting is undocumented:

1. `cache_read_input_tokens` is **excluded by default**. It is the dominant class in a cached session (verified: 108427 of 108886 tokens in one real event) and is discounted by the API, so including it would overestimate wildly. The flag exists so an operator who learns otherwise can switch it on.
2. Estimator percentages are only produced when the operator has configured a budget. Nothing in the notes file tells us a plan's real token allowance, so inventing one would be a guess dressed as data.

- [ ] **Step 1: Write the failing timestamp and token tests**

Add to the `mod tests` in `src/commands/ctx/window.rs`:

```rust
    #[test]
    fn real_transcript_timestamps_parse_to_unix_seconds() {
        // Exact format observed in ~/.claude/projects/**/*.jsonl.
        assert_eq!(
            parse_iso8601_utc("2026-07-31T14:15:15.968Z"),
            Some(1_785_507_315),
        );
        assert_eq!(parse_iso8601_utc("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(parse_iso8601_utc("1970-01-02T00:00:01.000Z"), Some(86_401));
        // Leap-year handling, since the window arithmetic depends on it.
        assert_eq!(parse_iso8601_utc("2024-02-29T00:00:00.000Z"), Some(1_709_164_800));
    }

    #[test]
    fn malformed_timestamps_are_skipped_not_guessed() {
        assert_eq!(parse_iso8601_utc(""), None);
        assert_eq!(parse_iso8601_utc("yesterday"), None);
        assert_eq!(parse_iso8601_utc("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_iso8601_utc("2026-07-31"), None);
    }

    #[test]
    fn cache_reads_are_excluded_by_default_and_optional() {
        // The usage block of a real cached assistant event.
        let usage = serde_json::json!({
            "input_tokens": 2,
            "cache_creation_input_tokens": 457,
            "cache_read_input_tokens": 108_427,
            "output_tokens": 577
        });
        assert_eq!(
            usage_tokens_of(&usage, false),
            1036,
            "input + cache_creation + output, cache reads excluded"
        );
        assert_eq!(usage_tokens_of(&usage, true), 109_463);
        assert_eq!(usage_tokens_of(&serde_json::json!({}), false), 0);
    }
```

- [ ] **Step 2: Run them and see them fail**

Run: `cargo test ctx::window 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find function parse_iso8601_utc`.

- [ ] **Step 3: Write the timestamp and token implementation**

Append to `src/commands/ctx/window.rs`:

```rust
use std::path::{Path, PathBuf};

pub const FIVE_HOUR_SECS: u64 = 5 * 3600;
pub const SEVEN_DAY_SECS: u64 = 7 * 24 * 3600;

/// Days from the unix epoch for a civil date, valid for any year in range.
/// Howard Hinnant's `days_from_civil`, which is why no date crate is needed.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Parses the exact shape claude writes: `2026-07-31T14:15:15.968Z`. Fractional
/// seconds and the offset suffix are ignored; anything else returns `None` so a
/// malformed line is skipped rather than counted at the wrong time.
pub fn parse_iso8601_utc(ts: &str) -> Option<u64> {
    let bytes = ts.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    if bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }

    let field = |from: usize, to: usize| ts.get(from..to)?.parse::<i64>().ok();
    let year = field(0, 4)?;
    let month = field(5, 7)?;
    let day = field(8, 10)?;
    let hour = field(11, 13)?;
    let minute = field(14, 16)?;
    let second = field(17, 19)?;

    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    let days = days_from_civil(year, month, day);
    let total = days * 86_400 + hour * 3600 + minute * 60 + second;
    u64::try_from(total).ok()
}

/// Cache reads are excluded by default: they are the dominant class in a cached
/// session and are discounted by the API, and the notes file records that the
/// limiter's real weighting is undocumented.
pub fn usage_tokens_of(usage: &Value, count_cache_reads: bool) -> u64 {
    let field = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let mut total = field("input_tokens") + field("cache_creation_input_tokens") + field("output_tokens");
    if count_cache_reads {
        total += field("cache_read_input_tokens");
    }
    total
}
```

- [ ] **Step 4: Run them and see them pass**

Run: `cargo test ctx::window 2>&1 | tail -20`
Expected: PASS, 14 tests. If `real_transcript_timestamps_parse_to_unix_seconds` is off by exactly 86400, check the `day - 1` term in `days_from_civil`.

- [ ] **Step 5: Write the failing walk tests**

Add to the same `mod tests`:

```rust
    /// Builds a transcript whose assistant events sit at given ages in seconds.
    fn transcript_with_ages(now: u64, ages: &[u64], tokens: u64) -> String {
        let mut text = String::new();
        for age in ages {
            let at = now - age;
            text.push_str(&format!(
                "{{\"type\":\"assistant\",\"timestamp\":\"{}\",\"message\":{{\"usage\":{{\"input_tokens\":{tokens},\"cache_read_input_tokens\":999999}}}}}}\n",
                iso_of(at)
            ));
        }
        text
    }

    /// Inverse of `parse_iso8601_utc`, for building fixtures only.
    fn iso_of(unix: u64) -> String {
        let days = (unix / 86_400) as i64;
        let secs = unix % 86_400;
        let (year, month, day) = civil_from_days_for_tests(days);
        format!(
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.000Z",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    }

    fn civil_from_days_for_tests(days: i64) -> (i64, i64, i64) {
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        (if m <= 2 { y + 1 } else { y }, m, d)
    }

    #[test]
    fn the_fixture_timestamp_helper_round_trips() {
        for unix in [0_u64, 1_785_507_315, 1_709_164_800] {
            assert_eq!(parse_iso8601_utc(&iso_of(unix)), Some(unix), "round trip {unix}");
        }
    }

    #[test]
    fn only_events_inside_each_window_are_summed() {
        let now = 1_785_507_315;
        // 1h ago (both windows), 6h ago (7d only), 8d ago (neither).
        let jsonl = transcript_with_ages(now, &[3600, 21_600, 691_200], 100);

        let mut sums = TokenSums::default();
        sum_file(&jsonl, now, false, &mut sums);

        assert_eq!(sums.five_hour, 100, "one event within 5h");
        assert_eq!(sums.seven_day, 200, "two events within 7d");
        assert_eq!(sums.events_counted, 2);
    }

    #[test]
    fn the_oldest_counted_event_is_tracked_for_reset_estimation() {
        let now = 1_785_507_315;
        let jsonl = transcript_with_ages(now, &[3600, 7200], 10);
        let mut sums = TokenSums::default();
        sum_file(&jsonl, now, false, &mut sums);
        assert_eq!(sums.oldest_in_five_hour, now - 7200);
        assert_eq!(sums.oldest_in_seven_day, now - 7200);
    }

    #[test]
    fn non_assistant_and_malformed_lines_are_ignored() {
        let now = 1_785_507_315;
        let mut jsonl = String::new();
        jsonl.push_str("{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n");
        jsonl.push_str("not json\n\n");
        jsonl.push_str("{\"type\":\"assistant\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n");
        jsonl.push_str(&transcript_with_ages(now, &[60], 7));

        let mut sums = TokenSums::default();
        sum_file(&jsonl, now, false, &mut sums);
        assert_eq!(sums.five_hour, 7, "the event with no timestamp cannot be placed");
        assert_eq!(sums.events_counted, 1);
    }

    #[test]
    fn the_walk_includes_subagent_files() {
        let now = 1_785_507_315;
        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("projects");
        let session_dir = projects.join("-home-testuser-repo");
        std::fs::create_dir_all(session_dir.join("subagents")).expect("mkdir");

        std::fs::write(
            session_dir.join("main.jsonl"),
            transcript_with_ages(now, &[600], 100),
        )
        .expect("write main");
        std::fs::write(
            session_dir.join("subagents").join("sub.jsonl"),
            transcript_with_ages(now, &[600], 25),
        )
        .expect("write subagent");
        // A non-transcript file must not be parsed.
        std::fs::write(session_dir.join("notes.txt"), "ignore me").expect("write txt");

        let sums = sum_transcripts(&projects, now, false);
        assert_eq!(sums.files_scanned, 2, "main plus subagent, not the txt");
        assert_eq!(
            sums.five_hour, 125,
            "subagent turns live in their own files and must be counted"
        );
    }

    #[test]
    fn an_absent_projects_root_sums_to_zero() {
        let sums = sum_transcripts(std::path::Path::new("/nonexistent/projects"), 100, false);
        assert_eq!(sums, TokenSums::default());
    }

    #[test]
    fn percentages_need_a_configured_budget() {
        let now = 1_785_507_315;
        let sums = TokenSums {
            five_hour: 500,
            seven_day: 2000,
            oldest_in_five_hour: now - 3600,
            oldest_in_seven_day: now - 86_400,
            files_scanned: 1,
            events_counted: 4,
        };

        assert_eq!(
            estimate_windows(&sums, now, 0, 0),
            UsageWindows::default(),
            "no budget means no honest percentage"
        );

        let windows = estimate_windows(&sums, now, 1000, 8000);
        let five = windows.five_hour.expect("five_hour");
        assert_eq!(five.used_percentage, 50.0);
        assert_eq!(five.observed_at, now);
        assert_eq!(
            five.resets_at,
            now - 3600 + FIVE_HOUR_SECS,
            "a rolling window frees up when its oldest counted event ages out"
        );

        let seven = windows.seven_day.expect("seven_day");
        assert_eq!(seven.used_percentage, 25.0);
        assert_eq!(seven.resets_at, now - 86_400 + SEVEN_DAY_SECS);
    }

    #[test]
    fn percentages_are_capped_at_one_hundred() {
        let now = 1_000_000;
        let sums = TokenSums {
            five_hour: 5000,
            seven_day: 0,
            oldest_in_five_hour: now - 60,
            oldest_in_seven_day: 0,
            files_scanned: 1,
            events_counted: 1,
        };
        let five = estimate_windows(&sums, now, 1000, 0).five_hour.expect("five");
        assert_eq!(five.used_percentage, 100.0);
    }

    #[test]
    fn a_window_with_no_events_reports_zero_and_resets_now() {
        let now = 1_000_000;
        let windows = estimate_windows(&TokenSums::default(), now, 1000, 1000);
        let five = windows.five_hour.expect("five");
        assert_eq!(five.used_percentage, 0.0);
        assert_eq!(five.resets_at, now, "nothing to wait for");
    }
```

- [ ] **Step 6: Run them and see them fail**

Run: `cargo test ctx::window 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find type TokenSums`.

- [ ] **Step 7: Write the walk implementation**

Append to `src/commands/ctx/window.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenSums {
    pub five_hour: u64,
    pub seven_day: u64,
    /// Unix second of the oldest event counted in each window, or `0` when the
    /// window counted nothing. Used to estimate when the window frees up.
    pub oldest_in_five_hour: u64,
    pub oldest_in_seven_day: u64,
    pub files_scanned: usize,
    pub events_counted: usize,
}

fn note_oldest(slot: &mut u64, at: u64) {
    if *slot == 0 || at < *slot {
        *slot = at;
    }
}

/// Accumulates one transcript's assistant usage into the trailing windows.
/// Events without a parseable timestamp cannot be placed in a window and are
/// skipped rather than counted at the wrong time.
pub fn sum_file(jsonl: &str, now: u64, count_cache_reads: bool, into: &mut TokenSums) {
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if row.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(at) = row
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso8601_utc)
        else {
            continue;
        };
        let age = now.saturating_sub(at);
        if age > SEVEN_DAY_SECS {
            continue;
        }

        let Some(usage) = row.get("message").and_then(|m| m.get("usage")) else {
            continue;
        };
        let tokens = usage_tokens_of(usage, count_cache_reads);

        into.events_counted += 1;
        into.seven_day += tokens;
        note_oldest(&mut into.oldest_in_seven_day, at);
        if age <= FIVE_HOUR_SECS {
            into.five_hour += tokens;
            note_oldest(&mut into.oldest_in_five_hour, at);
        }
    }
}

pub fn projects_root() -> CtxResult<PathBuf> {
    Ok(crate::utils::home_dir()?.join(".claude").join("projects"))
}

/// Walks every transcript under the projects root, including the `subagents/`
/// subdirectories, because subagent turns live in their own files and still
/// spend the account's budget.
pub fn sum_transcripts(projects_root: &Path, now: u64, count_cache_reads: bool) -> TokenSums {
    let mut sums = TokenSums::default();
    let mut stack = vec![projects_root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            sums.files_scanned += 1;
            sum_file(&text, now, count_cache_reads, &mut sums);
        }
    }
    sums
}

fn estimated_window(used: u64, budget: u64, oldest: u64, span: u64, now: u64) -> Option<Window> {
    if budget == 0 {
        return None;
    }
    let percent = ((used as f64 / budget as f64) * 100.0).clamp(0.0, 100.0);
    let resets_at = if oldest == 0 { now } else { oldest + span };
    Some(Window {
        used_percentage: percent,
        resets_at,
        observed_at: now,
    })
}

/// Percentages only exist once the operator configures a budget: the notes file
/// records that a plan's real token allowance is undocumented, so a default
/// would be a guess presented as data.
pub fn estimate_windows(
    sums: &TokenSums,
    now: u64,
    five_hour_budget: u64,
    seven_day_budget: u64,
) -> UsageWindows {
    UsageWindows {
        five_hour: estimated_window(
            sums.five_hour,
            five_hour_budget,
            sums.oldest_in_five_hour,
            FIVE_HOUR_SECS,
            now,
        ),
        seven_day: estimated_window(
            sums.seven_day,
            seven_day_budget,
            sums.oldest_in_seven_day,
            SEVEN_DAY_SECS,
            now,
        ),
    }
}
```

`TokenSums` derives `Eq` while `Window` cannot (it holds an `f64`), which is why the two structs have different derive lists.

- [ ] **Step 8: Run them and see them pass**

Run: `cargo test ctx::window -- --test-threads=1 2>&1 | tail -20`
Expected: PASS, 23 tests.

- [ ] **Step 9: Sanity-check the estimator against this machine's real transcripts**

Run:

```bash
cargo build
python3 - <<'PY'
import glob, json, os, time
now = int(time.time())
five = seven = files = 0
for path in glob.glob(os.path.expanduser('~/.claude/projects/**/*.jsonl'), recursive=True):
    files += 1
    for line in open(path, errors='ignore'):
        try:
            row = json.loads(line)
        except Exception:
            continue
        if row.get('type') != 'assistant':
            continue
        ts = row.get('timestamp')
        if not ts:
            continue
        at = int(time.mktime(time.strptime(ts[:19], '%Y-%m-%dT%H:%M:%S')) - time.timezone)
        usage = (row.get('message') or {}).get('usage') or {}
        tokens = sum(int(usage.get(k) or 0) for k in ('input_tokens', 'cache_creation_input_tokens', 'output_tokens'))
        age = now - at
        if age <= 7 * 86400:
            seven += tokens
            if age <= 5 * 3600:
                five += tokens
print(f'files={files} five_hour={five} seven_day={seven}')
PY
```

Expected: plausible non-zero sums (a working day is typically tens to hundreds of thousands). Record the numbers in the commit message: this is the only cross-check available, since no API reports the true figure.

- [ ] **Step 10: Commit**

```bash
git add src/commands/ctx/window.rs
git commit -m "feat(ctx): estimate usage windows by summing transcript token usage"
```

---

### Task E3: Pacing gate and configuration

**Files:**
- Create: `src/commands/ctx/pace.rs`
- Modify: `src/commands/ctx/config.rs:119-128` (add `pace`), `:130-134` (`EnvKind`), `:136-189` (`ENV_MAP`), `:223-231` (`env_value`)
- Modify: `src/commands/ctx/mod.rs:3-19` (declare `pace`)

**Interfaces:**
- Consumes: `UsageWindows`, `Window`, `age_secs` (E1); `CtxConfig`, `EnvKind`, `ENV_MAP`, `env_value` (`config.rs`, all as implemented).
- Produces:
  - `config.rs`: `pub struct PaceConfig { pub enabled: bool, pub max_percent: f64, pub collector_max_age_secs: u64, pub estimator: bool, pub five_hour_budget_tokens: u64, pub seven_day_budget_tokens: u64, pub count_cache_reads: bool, pub jitter_secs: u64, pub fallback_delay_secs: u64, pub wait_slack_secs: u64, pub max_wait_secs: Option<u64> }` with `Default` (`enabled: true`, `max_percent: 99.0`, `collector_max_age_secs: 900`, `estimator: true`, budgets `0`, `count_cache_reads: false`, `jitter_secs: 30`, `fallback_delay_secs: 900`, `wait_slack_secs: 3600`, `max_wait_secs: None`), plus `CtxConfig.pace: PaceConfig` and two new `EnvKind` variants `Float` and `Bool`
  - `pace.rs`: `pub enum Source { Collector, Estimator, None }`; `pub enum PaceDecision { Proceed { source: Source, worst_percent: f64 }, WaitUntil { reset_at: Option<u64>, window: &'static str, percent: f64, source: Source }, Unknown }`; `pub fn decide(collector: &UsageWindows, estimator: Option<&UsageWindows>, now: u64, cfg: &PaceConfig) -> PaceDecision`; `pub fn window_length(window: &str) -> u64`; `pub fn wait_cap(window: &str, cfg: &PaceConfig) -> u64`; `pub fn wait_deadline(decision: &PaceDecision, now: u64, cfg: &PaceConfig, seed: u64) -> Option<u64>`; `pub fn apply_jitter(until: u64, jitter_secs: u64, seed: u64) -> u64`; `pub fn describe(decision: &PaceDecision) -> String`

**The safety valve is scaled to the window, not to a fixed clock.** A seven-day window that trips genuinely needs up to seven days of waiting; a global six-hour cap would resume every six hours and burn tokens against an exhausted week, which is the opposite of what pacing is for. So the cap is `window_length + wait_slack_secs` (5h plus 1h, or 7d plus 1h), and `max_wait_secs` exists only as an explicit absolute override for an operator who would rather proceed sooner.

- [ ] **Step 1: Write the failing config tests**

Add to the existing `mod tests` in `src/commands/ctx/config.rs`:

```rust
    #[test]
    fn pacing_defaults_match_the_spec() {
        let pace = PaceConfig::default();
        assert!(pace.enabled, "pacing is on by default");
        assert_eq!(pace.max_percent, 99.0);
        assert_eq!(pace.collector_max_age_secs, 900);
        assert!(pace.estimator);
        assert_eq!(
            (pace.five_hour_budget_tokens, pace.seven_day_budget_tokens),
            (0, 0),
            "no invented budget: the estimator stays quiet until an operator sets one"
        );
        assert!(!pace.count_cache_reads);
        assert_eq!(pace.jitter_secs, 30);
        assert_eq!(pace.fallback_delay_secs, 900);
        assert_eq!(pace.wait_slack_secs, 3600);
        assert_eq!(
            pace.max_wait_secs, None,
            "no global cap by default: the cap is scaled to the window that tripped"
        );
    }

    #[test]
    fn pacing_reads_from_the_repo_config_file() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[pace]\nenabled = false\nmax_percent = 80.5\nfive_hour_budget_tokens = 500000\ncount_cache_reads = true\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(!cfg.pace.enabled);
        assert_eq!(cfg.pace.max_percent, 80.5);
        assert_eq!(cfg.pace.five_hour_budget_tokens, 500_000);
        assert!(cfg.pace.count_cache_reads);
        assert_eq!(cfg.pace.fallback_delay_secs, 900, "untouched keys keep defaults");
    }

    #[test]
    fn pacing_env_overrides_cover_floats_and_bools() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[
            ("ZIRV_CTX_PACE", "false"),
            ("ZIRV_CTX_PACE_MAX_PERCENT", "75"),
            ("ZIRV_CTX_FIVE_HOUR_BUDGET", "1000"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(!cfg.pace.enabled);
        assert_eq!(cfg.pace.max_percent, 75.0, "an integer literal must load as a float");
        assert_eq!(cfg.pace.five_hour_budget_tokens, 1000);
    }

    #[test]
    fn a_non_numeric_percent_is_rejected_with_the_variable_named() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_PACE_MAX_PERCENT", "loads")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect_err("bad float");
        let msg = err.to_string();
        assert!(msg.contains("ZIRV_CTX_PACE_MAX_PERCENT"), "got {msg}");
    }

    #[test]
    fn a_non_boolean_flag_is_rejected() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_PACE", "yes-please")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect_err("bad bool");
        assert!(err.to_string().contains("ZIRV_CTX_PACE"));
    }
```

- [ ] **Step 2: Run them and see them fail**

Run: `cargo test ctx::config 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find type PaceConfig`.

- [ ] **Step 3: Extend the config**

In `src/commands/ctx/config.rs`, add the struct next to `HandoffConfig`:

```rust
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PaceConfig {
    pub enabled: bool,
    /// A supervised window is kept at or below this percentage.
    pub max_percent: f64,
    /// Collector readings older than this are treated as stale.
    pub collector_max_age_secs: u64,
    pub estimator: bool,
    /// `0` disables the estimator for that window: a plan's real allowance is
    /// undocumented, so there is no honest default.
    pub five_hour_budget_tokens: u64,
    pub seven_day_budget_tokens: u64,
    pub count_cache_reads: bool,
    pub jitter_secs: u64,
    /// Used when a window's `resets_at` is unknown.
    pub fallback_delay_secs: u64,
    /// Head-room added to a window's own length to form the default safety cap,
    /// so a slightly wrong `resets_at` still resolves.
    pub wait_slack_secs: u64,
    /// Absolute override for the safety cap. `None` scales the cap to the window
    /// that tripped (5h or 7d, plus `wait_slack_secs`), which is what the spec's
    /// wait-until-reset semantics require: a global cap would resume early and
    /// spend tokens against a window that is still exhausted.
    pub max_wait_secs: Option<u64>,
}

impl Default for PaceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_percent: 99.0,
            collector_max_age_secs: 900,
            estimator: true,
            five_hour_budget_tokens: 0,
            seven_day_budget_tokens: 0,
            count_cache_reads: false,
            jitter_secs: 30,
            fallback_delay_secs: 900,
            wait_slack_secs: 3600,
            max_wait_secs: None,
        }
    }
}
```

Add the field to `CtxConfig`:

```rust
    pub pace: PaceConfig,
```

Extend `EnvKind` and `env_value`:

```rust
#[derive(Debug, Clone, Copy)]
enum EnvKind {
    Int,
    Float,
    Bool,
    Str,
}
```

```rust
fn env_value(raw: &str, kind: EnvKind) -> CtxResult<toml::Value> {
    match kind {
        EnvKind::Str => Ok(toml::Value::String(raw.to_string())),
        EnvKind::Int => raw
            .parse::<i64>()
            .map(toml::Value::Integer)
            .map_err(|_| format!("expected an integer, got '{raw}'").into()),
        EnvKind::Float => raw
            .parse::<f64>()
            .map(toml::Value::Float)
            .map_err(|_| format!("expected a number, got '{raw}'").into()),
        EnvKind::Bool => raw
            .parse::<bool>()
            .map(toml::Value::Boolean)
            .map_err(|_| format!("expected true or false, got '{raw}'").into()),
    }
}
```

Append to `ENV_MAP`:

```rust
    ("ZIRV_CTX_PACE", &["pace", "enabled"], EnvKind::Bool),
    (
        "ZIRV_CTX_PACE_MAX_PERCENT",
        &["pace", "max_percent"],
        EnvKind::Float,
    ),
    (
        "ZIRV_CTX_PACE_FALLBACK_SECS",
        &["pace", "fallback_delay_secs"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_PACE_MAX_WAIT_SECS",
        &["pace", "max_wait_secs"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_PACE_SLACK_SECS",
        &["pace", "wait_slack_secs"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_PACE_JITTER_SECS",
        &["pace", "jitter_secs"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_FIVE_HOUR_BUDGET",
        &["pace", "five_hour_budget_tokens"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_SEVEN_DAY_BUDGET",
        &["pace", "seven_day_budget_tokens"],
        EnvKind::Int,
    ),
```

A TOML float field accepts an integer literal only if it arrives as `Value::Float`, which is why `ZIRV_CTX_PACE_MAX_PERCENT=75` needs `EnvKind::Float` rather than `Int`.

- [ ] **Step 4: Run them and see them pass**

Run: `cargo test ctx::config 2>&1 | tail -20`
Expected: PASS, 10 tests.

- [ ] **Step 5: Write the failing gate tests**

Create `src/commands/ctx/pace.rs` containing only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::config::PaceConfig;
    use crate::commands::ctx::window::{UsageWindows, Window};

    const NOW: u64 = 1_785_507_315;

    fn window(percent: f64, resets_at: u64, observed_at: u64) -> Option<Window> {
        Some(Window {
            used_percentage: percent,
            resets_at,
            observed_at,
        })
    }

    fn collector(percent: f64) -> UsageWindows {
        UsageWindows {
            five_hour: window(percent, NOW + 600, NOW - 10),
            seven_day: None,
        }
    }

    #[test]
    fn a_healthy_fresh_collector_reading_proceeds() {
        let decision = decide(&collector(42.0), None, NOW, &PaceConfig::default());
        assert_eq!(
            decision,
            PaceDecision::Proceed {
                source: Source::Collector,
                worst_percent: 42.0
            }
        );
    }

    #[test]
    fn at_the_ceiling_the_gate_waits_for_the_reset() {
        let decision = decide(&collector(99.0), None, NOW, &PaceConfig::default());
        assert_eq!(
            decision,
            PaceDecision::WaitUntil {
                reset_at: Some(NOW + 600),
                window: "five_hour",
                percent: 99.0,
                source: Source::Collector
            },
            "the default ceiling is inclusive"
        );
    }

    #[test]
    fn just_below_the_ceiling_still_proceeds() {
        let decision = decide(&collector(98.9), None, NOW, &PaceConfig::default());
        assert!(matches!(decision, PaceDecision::Proceed { .. }));
    }

    #[test]
    fn the_worst_window_decides() {
        let both = UsageWindows {
            five_hour: window(10.0, NOW + 100, NOW),
            seven_day: window(99.5, NOW + 90_000, NOW),
        };
        let decision = decide(&both, None, NOW, &PaceConfig::default());
        assert_eq!(
            decision,
            PaceDecision::WaitUntil {
                reset_at: Some(NOW + 90_000),
                window: "seven_day",
                percent: 99.5,
                source: Source::Collector
            }
        );
    }

    #[test]
    fn a_stale_collector_reading_is_ignored_in_favour_of_the_estimator() {
        let stale = UsageWindows {
            five_hour: window(5.0, NOW + 600, NOW - 100_000),
            seven_day: None,
        };
        let estimated = UsageWindows {
            five_hour: window(99.9, NOW + 300, NOW),
            seven_day: None,
        };
        let decision = decide(&stale, Some(&estimated), NOW, &PaceConfig::default());
        assert_eq!(
            decision,
            PaceDecision::WaitUntil {
                reset_at: Some(NOW + 300),
                window: "five_hour",
                percent: 99.9,
                source: Source::Estimator
            }
        );
    }

    #[test]
    fn a_fresh_collector_reading_always_beats_the_estimator() {
        let estimated = UsageWindows {
            five_hour: window(100.0, NOW + 300, NOW),
            seven_day: None,
        };
        let decision = decide(&collector(20.0), Some(&estimated), NOW, &PaceConfig::default());
        assert_eq!(
            decision,
            PaceDecision::Proceed {
                source: Source::Collector,
                worst_percent: 20.0
            },
            "server-authoritative data wins even when the approximation disagrees"
        );
    }

    #[test]
    fn nothing_known_is_unknown_not_zero() {
        let decision = decide(&UsageWindows::default(), None, NOW, &PaceConfig::default());
        assert_eq!(decision, PaceDecision::Unknown);

        let empty_estimate = UsageWindows::default();
        assert_eq!(
            decide(&UsageWindows::default(), Some(&empty_estimate), NOW, &PaceConfig::default()),
            PaceDecision::Unknown,
            "an estimator with no configured budget contributes nothing"
        );
    }

    #[test]
    fn disabling_the_estimator_leaves_a_stale_collector_unknown() {
        // Stale and below the ceiling, so it carries no information at all.
        let stale = UsageWindows {
            five_hour: window(50.0, NOW + 600, NOW - 100_000),
            seven_day: None,
        };
        let estimated = UsageWindows {
            five_hour: window(99.9, NOW + 300, NOW),
            seven_day: None,
        };
        let cfg = PaceConfig {
            estimator: false,
            ..PaceConfig::default()
        };
        assert_eq!(decide(&stale, Some(&estimated), NOW, &cfg), PaceDecision::Unknown);
    }

    #[test]
    fn a_stale_full_window_keeps_binding_until_its_reset_arrives() {
        // Staleness must not clear a park: the percentage is old, but a window
        // cannot free up before its own reset, and resuming here would spend
        // tokens against a window that is still exhausted.
        let stale_but_full = UsageWindows {
            five_hour: window(100.0, NOW + 600, NOW - 100_000),
            seven_day: None,
        };
        assert_eq!(
            decide(&stale_but_full, None, NOW, &PaceConfig::default()),
            PaceDecision::WaitUntil {
                reset_at: Some(NOW + 600),
                window: "five_hour",
                percent: 100.0,
                source: Source::Collector
            }
        );
    }

    #[test]
    fn a_stale_full_window_stops_binding_once_its_reset_has_passed() {
        let expired = UsageWindows {
            five_hour: window(100.0, NOW - 1, NOW - 100_000),
            seven_day: None,
        };
        assert_eq!(
            decide(&expired, None, NOW, &PaceConfig::default()),
            PaceDecision::Unknown,
            "after the reset the old percentage says nothing about the new window"
        );
    }

    #[test]
    fn a_stale_full_window_still_loses_to_a_fresh_reading() {
        let mixed = UsageWindows {
            five_hour: window(100.0, NOW + 600, NOW - 100_000),
            seven_day: window(10.0, NOW + 90_000, NOW),
        };
        // The stale-but-full five hour window is the worse of the two and still
        // binds, so the gate waits on it rather than on the fresh healthy one.
        assert!(matches!(
            decide(&mixed, None, NOW, &PaceConfig::default()),
            PaceDecision::WaitUntil { window: "five_hour", .. }
        ));
    }

    #[test]
    fn pacing_disabled_always_proceeds() {
        let cfg = PaceConfig {
            enabled: false,
            ..PaceConfig::default()
        };
        assert_eq!(
            decide(&collector(100.0), None, NOW, &cfg),
            PaceDecision::Proceed {
                source: Source::None,
                worst_percent: 0.0
            }
        );
    }

    #[test]
    fn jitter_is_bounded_and_deterministic_for_a_seed() {
        for seed in [0_u64, 1, 12_345, u64::MAX] {
            let jittered = apply_jitter(NOW, 30, seed);
            assert!(
                (NOW..NOW + 30).contains(&jittered),
                "seed {seed} produced {jittered}"
            );
            assert_eq!(jittered, apply_jitter(NOW, 30, seed), "same seed, same answer");
        }
        assert_eq!(apply_jitter(NOW, 0, 7), NOW, "zero jitter is exact");
    }

    #[test]
    fn a_known_reset_becomes_a_jittered_deadline() {
        let decision = decide(&collector(99.0), None, NOW, &PaceConfig::default());
        let deadline = wait_deadline(&decision, NOW, &PaceConfig::default(), 7).expect("a deadline");
        assert!(
            (NOW + 600..NOW + 630).contains(&deadline),
            "reset plus jitter, got {deadline}"
        );
    }

    #[test]
    fn an_unknown_reset_uses_the_configured_fallback_delay() {
        let unknown = UsageWindows {
            five_hour: window(99.5, 0, NOW),
            seven_day: None,
        };
        let decision = decide(&unknown, None, NOW, &PaceConfig::default());
        assert_eq!(
            decision,
            PaceDecision::WaitUntil {
                reset_at: None,
                window: "five_hour",
                percent: 99.5,
                source: Source::Collector
            },
            "resets_at of zero means unknown, not epoch"
        );
        let deadline = wait_deadline(&decision, NOW, &PaceConfig::default(), 0).expect("deadline");
        assert_eq!(deadline, NOW + 900);
    }

    #[test]
    fn a_reset_already_in_the_past_uses_the_fallback_too() {
        let past = UsageWindows {
            five_hour: window(99.5, NOW - 5, NOW),
            seven_day: None,
        };
        let decision = decide(&past, None, NOW, &PaceConfig::default());
        let deadline = wait_deadline(&decision, NOW, &PaceConfig::default(), 0).expect("deadline");
        assert_eq!(deadline, NOW + 900, "a stale reset must not resolve instantly");
    }

    #[test]
    fn the_cap_is_scaled_to_the_window_that_tripped() {
        let cfg = PaceConfig::default();
        assert_eq!(window_length("five_hour"), 18_000);
        assert_eq!(window_length("seven_day"), 604_800);
        assert_eq!(
            window_length("something_new"),
            18_000,
            "an unknown window name must not buy a week-long wait"
        );

        assert_eq!(wait_cap("five_hour", &cfg), 18_000 + 3600);
        assert_eq!(wait_cap("seven_day", &cfg), 604_800 + 3600);
    }

    #[test]
    fn a_seven_day_trip_may_wait_days_not_hours() {
        // The reset is five days out. A global six-hour valve would resume long
        // before the week reset and spend tokens against an exhausted window.
        let exhausted_week = UsageWindows {
            five_hour: None,
            seven_day: window(100.0, NOW + 432_000, NOW),
        };
        let cfg = PaceConfig {
            jitter_secs: 0,
            ..PaceConfig::default()
        };
        let decision = decide(&exhausted_week, None, NOW, &cfg);
        assert_eq!(
            wait_deadline(&decision, NOW, &cfg, 0),
            Some(NOW + 432_000),
            "the real reset sits inside the seven-day cap, so it is honoured exactly"
        );
    }

    #[test]
    fn a_five_hour_trip_is_capped_near_five_hours() {
        // A bogus reset a year out must not park a supervisor for a year.
        let bogus = UsageWindows {
            five_hour: window(100.0, NOW + 31_000_000, NOW),
            seven_day: None,
        };
        let cfg = PaceConfig {
            jitter_secs: 0,
            ..PaceConfig::default()
        };
        let decision = decide(&bogus, None, NOW, &cfg);
        assert_eq!(
            wait_deadline(&decision, NOW, &cfg, 0),
            Some(NOW + 18_000 + 3600),
            "capped at the window length plus slack"
        );
    }

    #[test]
    fn an_absolute_override_replaces_the_per_window_cap() {
        let far = UsageWindows {
            five_hour: None,
            seven_day: window(99.5, NOW + 500_000, NOW),
        };
        let cfg = PaceConfig {
            max_wait_secs: Some(60),
            jitter_secs: 0,
            ..PaceConfig::default()
        };
        assert_eq!(wait_cap("seven_day", &cfg), 60, "the override wins outright");
        let decision = decide(&far, None, NOW, &cfg);
        assert_eq!(wait_deadline(&decision, NOW, &cfg, 0), Some(NOW + 60));
    }

    #[test]
    fn proceeding_and_unknown_have_no_deadline() {
        let cfg = PaceConfig::default();
        assert_eq!(wait_deadline(&PaceDecision::Unknown, NOW, &cfg, 0), None);
        assert_eq!(
            wait_deadline(
                &PaceDecision::Proceed { source: Source::Collector, worst_percent: 1.0 },
                NOW,
                &cfg,
                0
            ),
            None
        );
    }

    #[test]
    fn descriptions_are_one_line_and_name_the_source() {
        let waiting = decide(&collector(99.0), None, NOW, &PaceConfig::default());
        let text = describe(&waiting);
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("five_hour"));
        assert!(text.contains("collector"));
        assert!(!text.contains('\u{2014}'));

        assert!(describe(&PaceDecision::Unknown).contains("unknown"));
        assert!(
            describe(&PaceDecision::Unknown).contains("approximation")
                || describe(&PaceDecision::Unknown).contains("no usage data"),
            "be honest when nothing is known: {}",
            describe(&PaceDecision::Unknown)
        );
    }

    #[test]
    fn the_documented_limit_strings_are_matched() {
        // Exactly the three shapes recorded in
        // docs/superpowers/notes/2026-07-31-claude-usage-window-facts.md.
        assert!(is_limit_hit("You've hit your session limit · resets 3:45pm"));
        assert!(is_limit_hit("You've hit your weekly limit · resets Mon 12:00am"));
        assert!(is_limit_hit("You've hit your Opus limit · resets 3:45pm"));
        assert!(is_limit_hit("  WARNING: you've HIT YOUR SESSION LIMIT now  "));
    }

    #[test]
    fn only_the_documented_patterns_ship() {
        assert_eq!(
            LIMIT_HIT_PATTERNS.len(),
            3,
            "the notes file documents three strings; anything else needs verifying first"
        );
        // Plausible but unverified phrasings stay out until observed for real.
        assert!(!is_limit_hit("You've hit your Sonnet limit · resets 3:45pm"));
        assert!(!is_limit_hit("You've hit your usage limit"));
    }

    #[test]
    fn ordinary_output_is_not_a_limit_hit() {
        for line in [
            "",
            "rate limit headers look fine",
            "hit the ground running",
            "your session is limited to one file",
            "error: 429 too many requests",
        ] {
            assert!(!is_limit_hit(line), "false positive on {line:?}");
        }
    }
}
```

- [ ] **Step 6: Run them and see them fail**

Run: `cargo test ctx::pace 2>&1 | tail -20`
Expected: FAIL. `module pace not found` until `mod.rs` declares it, then `cannot find function decide`.

- [ ] **Step 7: Write the gate implementation**

Add `pub mod pace;` to `src/commands/ctx/mod.rs` (alphabetically after `mod log;`, before `resume`). Then put this above the test module in `src/commands/ctx/pace.rs`:

```rust
// Consumed by the supervisors in Task E4 and the usage verb in Task E5.
#![allow(dead_code)]

use super::config::PaceConfig;
use super::window::{FIVE_HOUR_SECS, SEVEN_DAY_SECS, UsageWindows, Window, age_secs};

/// Which data layer the decision rests on. Ordered by authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Collector,
    Estimator,
    None,
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Collector => "collector",
            Source::Estimator => "estimator",
            Source::None => "none",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PaceDecision {
    Proceed {
        source: Source,
        worst_percent: f64,
    },
    WaitUntil {
        /// `None` when the window's reset time is unknown, which is when the
        /// configured fallback delay applies.
        reset_at: Option<u64>,
        window: &'static str,
        percent: f64,
        source: Source,
    },
    Unknown,
}

/// Exactly the three strings documented in
/// `docs/superpowers/notes/2026-07-31-claude-usage-window-facts.md`, matched
/// case-insensitively on whole phrases. Deliberately narrow on both sides: a
/// false positive parks a healthy run, and an unverified guess is not a fact.
///
/// Candidates NOT shipped, pending empirical verification (see the follow-up in
/// that notes file). Add one only after observing it in real output:
///   "hit your sonnet limit"   plausible by symmetry with the Opus variant,
///                             but no documented occurrence
///   "hit your usage limit"    invented phrasing, no source at all
pub const LIMIT_HIT_PATTERNS: &[&str] = &[
    "hit your session limit",
    "hit your weekly limit",
    "hit your opus limit",
];

pub fn is_limit_hit(line: &str) -> bool {
    let lowered = line.to_lowercase();
    LIMIT_HIT_PATTERNS
        .iter()
        .any(|pattern| lowered.contains(pattern))
}

/// Whether a collector window may drive the decision.
///
/// A fresh observation always may. A stale one still may when it reported a full
/// window whose reset has not arrived: the percentage is out of date, but a
/// window cannot free up before its own reset time, so letting staleness clear
/// the park would resume straight into an exhausted window. A stale reading
/// below the ceiling is simply unknown and defers to the estimator.
fn binding<'a>(window: &'a Option<Window>, now: u64, cfg: &PaceConfig) -> Option<&'a Window> {
    let window = window.as_ref()?;
    if age_secs(window, now) <= cfg.collector_max_age_secs {
        return Some(window);
    }
    if window.used_percentage >= cfg.max_percent && window.resets_at > now {
        return Some(window);
    }
    None
}

/// The window closest to its limit, with its name.
fn worst<'a>(
    five_hour: Option<&'a Window>,
    seven_day: Option<&'a Window>,
) -> Option<(&'static str, &'a Window)> {
    let candidates = [("five_hour", five_hour), ("seven_day", seven_day)];
    candidates
        .into_iter()
        .filter_map(|(name, window)| window.map(|w| (name, w)))
        .max_by(|a, b| {
            a.1.used_percentage
                .partial_cmp(&b.1.used_percentage)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

/// Collector first when fresh, estimator second, nothing third. A fresher
/// lower-priority layer never overrides a fresh collector reading.
pub fn decide(
    collector: &UsageWindows,
    estimator: Option<&UsageWindows>,
    now: u64,
    cfg: &PaceConfig,
) -> PaceDecision {
    if !cfg.enabled {
        return PaceDecision::Proceed {
            source: Source::None,
            worst_percent: 0.0,
        };
    }

    let collector_worst = worst(
        binding(&collector.five_hour, now, cfg),
        binding(&collector.seven_day, now, cfg),
    );

    let (source, picked) = match collector_worst {
        Some(found) => (Source::Collector, Some(found)),
        None if cfg.estimator => (
            Source::Estimator,
            estimator.and_then(|windows| worst(windows.five_hour.as_ref(), windows.seven_day.as_ref())),
        ),
        None => (Source::None, None),
    };

    let Some((name, window)) = picked else {
        return PaceDecision::Unknown;
    };

    if window.used_percentage < cfg.max_percent {
        return PaceDecision::Proceed {
            source,
            worst_percent: window.used_percentage,
        };
    }

    PaceDecision::WaitUntil {
        reset_at: if window.resets_at == 0 {
            None
        } else {
            Some(window.resets_at)
        },
        window: name,
        percent: window.used_percentage,
        source,
    }
}

/// Deterministic spread so several supervisors on one machine do not all wake
/// in the same second. Not cryptographic, just decorrelating.
pub fn apply_jitter(until: u64, jitter_secs: u64, seed: u64) -> u64 {
    if jitter_secs == 0 {
        return until;
    }
    let mixed = seed
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    until + (mixed >> 33) % jitter_secs
}

/// How long the named window itself lasts. An unrecognized name is treated as
/// the shorter window, so a future adapter cannot accidentally buy itself a
/// week-long wait.
pub fn window_length(window: &str) -> u64 {
    match window {
        "seven_day" => SEVEN_DAY_SECS,
        _ => FIVE_HOUR_SECS,
    }
}

/// Safety cap for waiting on a window: its own length plus head room, unless an
/// operator set an absolute override. Scaling to the window is the point: a
/// seven-day trip legitimately needs days, and a fixed cap would resume early
/// and spend tokens against a window that has not reset.
pub fn wait_cap(window: &str, cfg: &PaceConfig) -> u64 {
    cfg.max_wait_secs
        .unwrap_or_else(|| window_length(window) + cfg.wait_slack_secs)
}

/// Concrete wake-up time for a waiting decision: the reset when it is known and
/// still ahead, the fallback delay otherwise, jittered, and capped by
/// `wait_cap` for the window that tripped.
pub fn wait_deadline(
    decision: &PaceDecision,
    now: u64,
    cfg: &PaceConfig,
    seed: u64,
) -> Option<u64> {
    let PaceDecision::WaitUntil {
        reset_at, window, ..
    } = decision
    else {
        return None;
    };

    let target = match reset_at {
        Some(at) if *at > now => *at,
        _ => now + cfg.fallback_delay_secs,
    };
    let jittered = apply_jitter(target, cfg.jitter_secs, seed);
    Some(jittered.min(now + wait_cap(window, cfg)))
}

pub fn describe(decision: &PaceDecision) -> String {
    match decision {
        PaceDecision::Proceed {
            source,
            worst_percent,
        } => format!(
            "usage {worst_percent:.1}% of the limit ({} data), proceeding",
            source.as_str()
        ),
        PaceDecision::WaitUntil {
            reset_at,
            window,
            percent,
            source,
        } => {
            let reset = match reset_at {
                Some(at) => format!("resets at unix {at}"),
                None => "reset time unknown".to_string(),
            };
            format!(
                "{window} window at {percent:.1}% ({} data, {reset}), waiting before the next run",
                source.as_str()
            )
        }
        PaceDecision::Unknown => {
            "usage state unknown (no fresh collector reading and no configured estimator budget), proceeding without pacing".to_string()
        }
    }
}
```

- [ ] **Step 8: Run them and see them pass**

Run: `cargo test ctx::pace 2>&1 | tail -20`
Expected: PASS, 25 tests.

- [ ] **Step 9: Check formatting and lints**

Run: `cargo fmt -- --check && cargo clippy --all-targets -- -D warnings 2>&1 | tail -20`
Expected: clean. If clippy objects to `fresh`'s lifetime elision, keep the explicit `'a`: the borrow must outlive the returned reference.

- [ ] **Step 10: Commit**

```bash
git add src/commands/ctx/pace.rs src/commands/ctx/config.rs src/commands/ctx/mod.rs
git commit -m "feat(ctx): deterministic usage pacing gate with layered data sources"
```

---

### Task E4: Supervisor integration and the limit-hit circuit breaker

**Files:**
- Modify: `src/commands/ctx/pace.rs` (add `wait_for_window`)
- Modify: `src/commands/ctx/supervise.rs:25-31` (add `spawn_tapped`, `OutputTap`)
- Modify: `src/commands/ctx/exec.rs:154-160` (spawn), `:178-200` (outcome arms), `:246-260` (restart tail)
- Modify: `src/commands/ctx/run_loop.rs:96-135` (cycle body)
- Modify: `tests/fixtures/fake-agent.sh` (add a `limit` mode)

**Interfaces:**
- Consumes: `decide`, `wait_deadline`, `describe`, `is_limit_hit`, `Source`, `PaceDecision` (E3); `window::{load, sum_transcripts, estimate_windows, projects_root}` (E1/E2); `StateDir`, `now_secs`, `log::{append, Decision}` (implemented); `supervise::{spawn, supervise_child, Tick, Outcome, Watcher, terminate}` (`supervise.rs:12-116`, as implemented); `exec::EXIT_ROT_EXHAUSTED`/`EXIT_TIMEOUT` (`exec.rs:16,18`); `run_loop::EXIT_FAILED` (`run_loop.rs:13`).
- Produces:
  - `pace.rs`: `pub struct PaceOutcome { pub waited_secs: u64, pub source: Source }`; `pub fn wait_for_window<W: Write>(w: &mut W, state: &StateDir, cfg: &PaceConfig, verb: &'static str, session: &str, now_fn: &dyn Fn() -> u64, sleep_fn: &dyn Fn(std::time::Duration)) -> PaceOutcome`; `pub fn current_windows(state: &StateDir, cfg: &PaceConfig, now: u64) -> (UsageWindows, Option<UsageWindows>)`
  - `supervise.rs`: `pub struct OutputTap { rx: std::sync::mpsc::Receiver<String> }` with `pub fn try_lines(&self) -> Vec<String>`; `pub fn spawn_tapped(command: Command) -> CtxResult<(Child, OutputTap)>`
  - `exec.rs`: a `limit` branch that parks and relaunches without incrementing `restarts`
  - `run_loop.rs`: a pacing gate before each cycle and a `limit-park` action that is not a cycle failure

`spawn` keeps inheriting stdout and stderr for every other caller; only the pacing-aware path uses `spawn_tapped`, which pipes the child's output, forwards every byte onward unchanged, and copies whole lines to a channel for matching. Output passthrough is a hard requirement: the operator must still see the agent's output exactly as before.

- [ ] **Step 1: Write the failing tap tests**

Add to the `mod tests` in `src/commands/ctx/supervise.rs`:

```rust
    #[test]
    fn a_tapped_child_still_reports_its_exit_code() {
        let (mut child, _tap) = spawn_tapped(sh("printf hello\\n; exit 4")).expect("spawn");
        let outcome = supervise_child(
            &mut child,
            Instant::now() + Duration::from_secs(10),
            Duration::from_millis(20),
            &mut || Tick::Continue,
        )
        .expect("supervise");
        assert_eq!(outcome, Outcome::Exited(4));
    }

    #[test]
    fn tapped_lines_reach_the_matcher() {
        let (mut child, tap) = spawn_tapped(sh("printf 'one\\ntwo\\n'; exit 0")).expect("spawn");
        let mut seen: Vec<String> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while seen.len() < 2 && Instant::now() < deadline {
            seen.extend(tap.try_lines());
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.wait();
        assert!(seen.iter().any(|l| l.contains("one")), "got {seen:?}");
        assert!(seen.iter().any(|l| l.contains("two")), "got {seen:?}");
    }

    #[test]
    fn stderr_is_tapped_too_because_notices_can_land_there() {
        let (mut child, tap) = spawn_tapped(sh("printf 'oops\\n' >&2; exit 0")).expect("spawn");
        let mut seen: Vec<String> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while seen.is_empty() && Instant::now() < deadline {
            seen.extend(tap.try_lines());
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.wait();
        assert!(seen.iter().any(|l| l.contains("oops")), "got {seen:?}");
    }

    #[test]
    fn try_lines_is_empty_when_nothing_was_written() {
        let (mut child, tap) = spawn_tapped(sh("exit 0")).expect("spawn");
        let _ = child.wait();
        // Drain whatever arrived; a silent child must not block or panic.
        let _ = tap.try_lines();
        assert!(tap.try_lines().is_empty());
    }
```

- [ ] **Step 2: Run them and see them fail**

Run: `cargo test ctx::supervise 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find function spawn_tapped`.

- [ ] **Step 3: Write the tap**

Add to `src/commands/ctx/supervise.rs`:

```rust
/// Whole lines of a supervised child's output, for matching against known
/// notices. The bytes are always forwarded onward first: tapping must never
/// change what the operator sees.
pub struct OutputTap {
    rx: std::sync::mpsc::Receiver<String>,
}

impl OutputTap {
    pub fn try_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        while let Ok(line) = self.rx.try_recv() {
            lines.push(line);
        }
        lines
    }
}

/// Like `spawn`, but the child's stdout and stderr are piped so they can be
/// matched. Each stream is forwarded to this process's corresponding stream
/// unchanged, line by line.
pub fn spawn_tapped(mut command: Command) -> CtxResult<(Child, OutputTap)> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let (tx, rx) = std::sync::mpsc::channel::<String>();

    if let Some(stdout) = child.stdout.take() {
        forward(stdout, tx.clone(), false);
    }
    if let Some(stderr) = child.stderr.take() {
        forward(stderr, tx, true);
    }

    Ok((child, OutputTap { rx }))
}

fn forward<R: std::io::Read + Send + 'static>(
    stream: R,
    tx: std::sync::mpsc::Sender<String>,
    is_stderr: bool,
) {
    use std::io::BufRead;

    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stream);
        for line in reader.lines().map_while(Result::ok) {
            if is_stderr {
                let mut sink = std::io::stderr();
                let _ = writeln!(sink, "{line}");
            } else {
                let mut sink = std::io::stdout();
                let _ = writeln!(sink, "{line}");
                let _ = sink.flush();
            }
            if tx.send(line).is_err() {
                return;
            }
        }
    });
}
```

`use std::io::Write;` is already needed by `forward`; add it to the module imports if it is not there.

- [ ] **Step 4: Run them and see them pass**

Run: `cargo test ctx::supervise -- --test-threads=1 2>&1 | tail -20`
Expected: PASS, 14 tests.

- [ ] **Step 5: Write the failing wait-helper tests**

Add to the `mod tests` in `src/commands/ctx/pace.rs`:

```rust
    use crate::commands::ctx::state::StateDir;
    use crate::commands::ctx::window;
    use std::cell::RefCell;

    /// Fake clock: `now` advances by whatever the code sleeps, so a test can
    /// observe pacing without waiting for real time.
    struct FakeClock {
        now: RefCell<u64>,
        slept: RefCell<Vec<u64>>,
    }

    impl FakeClock {
        fn new(start: u64) -> Self {
            Self {
                now: RefCell::new(start),
                slept: RefCell::new(Vec::new()),
            }
        }
    }

    fn state_with(collector: UsageWindows) -> (tempfile::TempDir, StateDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        window::store(&state, &collector).expect("store");
        (tmp, state)
    }

    #[test]
    fn a_healthy_window_does_not_wait() {
        let (_tmp, state) = state_with(collector(10.0));
        let clock = FakeClock::new(NOW);
        let mut out = Vec::new();

        let outcome = wait_for_window(
            &mut out,
            &state,
            &PaceConfig::default(),
            "loop",
            "sess",
            &|| *clock.now.borrow(),
            &|d| {
                clock.slept.borrow_mut().push(d.as_secs());
                *clock.now.borrow_mut() += d.as_secs();
            },
        );

        assert_eq!(outcome.waited_secs, 0);
        assert_eq!(outcome.source, Source::Collector);
        assert!(clock.slept.borrow().is_empty(), "no sleeping when healthy");
    }

    #[test]
    fn an_exhausted_window_waits_past_the_reset_then_proceeds() {
        // Observed just now, at the ceiling, resetting in 10 minutes.
        let exhausted = UsageWindows {
            five_hour: window(99.5, NOW + 600, NOW),
            seven_day: None,
        };
        let (_tmp, state) = state_with(exhausted);
        let clock = FakeClock::new(NOW);
        let mut out = Vec::new();

        let cfg = PaceConfig {
            jitter_secs: 0,
            ..PaceConfig::default()
        };
        let outcome = wait_for_window(
            &mut out,
            &state,
            &cfg,
            "loop",
            "sess",
            &|| *clock.now.borrow(),
            &|d| {
                clock.slept.borrow_mut().push(d.as_secs());
                *clock.now.borrow_mut() += d.as_secs();
            },
        );

        assert!(outcome.waited_secs >= 600, "waited {}", outcome.waited_secs);
        assert!(!clock.slept.borrow().is_empty(), "it must actually sleep");
        assert!(
            *clock.now.borrow() >= NOW + 600,
            "the clock advanced past the reset"
        );

        let printed = String::from_utf8(out).expect("utf8");
        assert!(printed.contains("five_hour"), "explain the pause: {printed}");
        assert!(printed.contains("waiting"), "got {printed}");
    }

    #[test]
    fn waiting_is_recorded_in_the_decision_log() {
        let exhausted = UsageWindows {
            five_hour: window(100.0, NOW + 120, NOW),
            seven_day: None,
        };
        let (_tmp, state) = state_with(exhausted);
        let clock = FakeClock::new(NOW);
        let mut out = Vec::new();

        wait_for_window(
            &mut out,
            &state,
            &PaceConfig {
                jitter_secs: 0,
                ..PaceConfig::default()
            },
            "exec",
            "sess-1",
            &|| *clock.now.borrow(),
            &|d| *clock.now.borrow_mut() += d.as_secs(),
        );

        let log = std::fs::read_to_string(state.logs().join("decisions.jsonl")).expect("log");
        assert!(log.contains("\"verb\":\"exec\""), "got {log}");
        assert!(log.contains("\"action\":\"pace-wait\""), "got {log}");
        assert!(log.contains("sess-1"), "got {log}");
        assert_eq!(
            log.lines().filter(|l| l.contains("pace-wait")).count(),
            1,
            "one audit line per pause, not one per sleep chunk: {log}"
        );
    }

    #[test]
    fn an_absolute_override_bounds_the_total_wait() {
        let far = UsageWindows {
            five_hour: window(100.0, NOW + 10_000_000, NOW),
            seven_day: None,
        };
        let (_tmp, state) = state_with(far);
        let clock = FakeClock::new(NOW);
        let mut out = Vec::new();

        let cfg = PaceConfig {
            max_wait_secs: Some(120),
            jitter_secs: 0,
            ..PaceConfig::default()
        };
        let outcome = wait_for_window(
            &mut out,
            &state,
            &cfg,
            "loop",
            "sess",
            &|| *clock.now.borrow(),
            &|d| *clock.now.borrow_mut() += d.as_secs(),
        );

        assert!(
            outcome.waited_secs <= 150,
            "bounded by the override, waited {}",
            outcome.waited_secs
        );
        let printed = String::from_utf8(out).expect("utf8");
        assert!(
            printed.contains("proceeding"),
            "after the cap it proceeds rather than exiting: {printed}"
        );
    }

    #[test]
    fn a_bogus_five_hour_reset_is_bounded_by_the_window_not_by_six_hours() {
        // With no override, the cap comes from the window: 5h plus 1h slack.
        let bogus = UsageWindows {
            five_hour: window(100.0, NOW + 10_000_000, NOW),
            seven_day: None,
        };
        let (_tmp, state) = state_with(bogus);
        let clock = FakeClock::new(NOW);
        let mut out = Vec::new();

        let outcome = wait_for_window(
            &mut out,
            &state,
            &PaceConfig {
                jitter_secs: 0,
                ..PaceConfig::default()
            },
            "loop",
            "sess",
            &|| *clock.now.borrow(),
            &|d| *clock.now.borrow_mut() += d.as_secs(),
        );

        assert!(
            (18_000..=21_700).contains(&outcome.waited_secs),
            "expected roughly the five-hour cap, waited {}",
            outcome.waited_secs
        );
    }

    #[test]
    fn an_exhausted_week_waits_for_the_real_reset_rather_than_resuming_early() {
        // Five days out. This is the case a fixed six-hour valve got wrong: it
        // would resume roughly twenty times before the week actually reset.
        let exhausted_week = UsageWindows {
            five_hour: None,
            seven_day: window(100.0, NOW + 432_000, NOW),
        };
        let (_tmp, state) = state_with(exhausted_week);
        let clock = FakeClock::new(NOW);
        let mut out = Vec::new();

        let outcome = wait_for_window(
            &mut out,
            &state,
            &PaceConfig {
                jitter_secs: 0,
                ..PaceConfig::default()
            },
            "loop",
            "sess",
            &|| *clock.now.borrow(),
            &|d| *clock.now.borrow_mut() += d.as_secs(),
        );

        assert!(
            outcome.waited_secs >= 432_000,
            "it must wait out the week, waited {}",
            outcome.waited_secs
        );
        let printed = String::from_utf8(out).expect("utf8");
        assert!(
            !printed.contains("proceeding anyway"),
            "the real reset arrived inside the cap, so no valve message: {printed}"
        );
    }

    #[test]
    fn unknown_usage_proceeds_without_waiting() {
        let (_tmp, state) = state_with(UsageWindows::default());
        let clock = FakeClock::new(NOW);
        let mut out = Vec::new();

        let outcome = wait_for_window(
            &mut out,
            &state,
            &PaceConfig::default(),
            "loop",
            "sess",
            &|| *clock.now.borrow(),
            &|d| *clock.now.borrow_mut() += d.as_secs(),
        );

        assert_eq!(outcome.waited_secs, 0);
        assert_eq!(outcome.source, Source::None);
    }

    #[test]
    fn pacing_disabled_skips_the_gate_entirely() {
        let exhausted = UsageWindows {
            five_hour: window(100.0, NOW + 600, NOW),
            seven_day: None,
        };
        let (_tmp, state) = state_with(exhausted);
        let clock = FakeClock::new(NOW);
        let mut out = Vec::new();

        let outcome = wait_for_window(
            &mut out,
            &state,
            &PaceConfig {
                enabled: false,
                ..PaceConfig::default()
            },
            "loop",
            "sess",
            &|| *clock.now.borrow(),
            &|d| *clock.now.borrow_mut() += d.as_secs(),
        );
        assert_eq!(outcome.waited_secs, 0);
        assert!(String::from_utf8_lossy(&out).is_empty(), "silent when disabled");
    }

    #[test]
    fn the_estimator_is_only_consulted_when_a_budget_is_set() {
        let (_tmp, state) = state_with(UsageWindows::default());
        let cfg = PaceConfig::default();
        let (collector_windows, estimated) = current_windows(&state, &cfg, NOW);
        assert_eq!(collector_windows, UsageWindows::default());
        assert!(
            estimated.is_none(),
            "with both budgets at zero there is nothing to estimate against"
        );

        let with_budget = PaceConfig {
            five_hour_budget_tokens: 1000,
            ..PaceConfig::default()
        };
        let (_, estimated) = current_windows(&state, &with_budget, NOW);
        assert!(estimated.is_some(), "a configured budget turns the estimator on");
    }
```

- [ ] **Step 6: Run them and see them fail**

Run: `cargo test ctx::pace 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find function wait_for_window`.

- [ ] **Step 7: Write the wait helper**

Append to `src/commands/ctx/pace.rs`:

```rust
use std::io::Write;
use std::time::Duration;

use super::state::{StateDir, now_secs};
use super::{log, window as usage_window};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaceOutcome {
    pub waited_secs: u64,
    pub source: Source,
}

/// Longest single sleep, so a supervisor rechecks state (a live session may have
/// refreshed the collector) rather than sleeping blind for hours.
const SLEEP_CHUNK_SECS: u64 = 30;

/// Reads the collector file and, only when a budget is configured, the
/// estimator. Walking every transcript is not free, so it is skipped whenever
/// its result could not be used.
pub fn current_windows(
    state: &StateDir,
    cfg: &PaceConfig,
    now: u64,
) -> (UsageWindows, Option<UsageWindows>) {
    let collector = usage_window::load(state);

    let budgeted = cfg.five_hour_budget_tokens > 0 || cfg.seven_day_budget_tokens > 0;
    if !cfg.estimator || !budgeted {
        return (collector, None);
    }

    let estimated = usage_window::projects_root()
        .ok()
        .map(|root| usage_window::sum_transcripts(&root, now, cfg.count_cache_reads))
        .map(|sums| {
            usage_window::estimate_windows(
                &sums,
                now,
                cfg.five_hour_budget_tokens,
                cfg.seven_day_budget_tokens,
            )
        });
    (collector, estimated)
}

/// Blocks until the window has room, then returns. Never exits the process and
/// never returns an error: pacing failing closed would be worse than pacing not
/// happening, so every unknown proceeds.
pub fn wait_for_window<W: Write>(
    w: &mut W,
    state: &StateDir,
    cfg: &PaceConfig,
    verb: &'static str,
    session: &str,
    now_fn: &dyn Fn() -> u64,
    sleep_fn: &dyn Fn(Duration),
) -> PaceOutcome {
    if !cfg.enabled {
        return PaceOutcome {
            waited_secs: 0,
            source: Source::None,
        };
    }

    let started = now_fn();
    let mut announced: Option<(String, Option<u64>)> = None;

    loop {
        let now = now_fn();
        let (collector, estimated) = current_windows(state, cfg, now);
        let decision = decide(&collector, estimated.as_ref(), now, cfg);

        let source = match &decision {
            PaceDecision::Proceed { source, .. } => *source,
            PaceDecision::WaitUntil { source, .. } => *source,
            PaceDecision::Unknown => Source::None,
        };

        let Some(deadline) = wait_deadline(&decision, now, cfg, std::process::id() as u64 ^ now)
        else {
            return PaceOutcome {
                waited_secs: now.saturating_sub(started),
                source,
            };
        };

        // The safety valve, scaled to the window that tripped: a seven-day trip
        // may legitimately wait days, a five-hour trip may not.
        let cap = match &decision {
            PaceDecision::WaitUntil { window, .. } => wait_cap(window, cfg),
            _ => 0,
        };
        if now.saturating_sub(started) >= cap {
            let _ = writeln!(
                w,
                "zirv ctx {verb}: usage still high after waiting {}s (cap {cap}s), proceeding anyway",
                now.saturating_sub(started)
            );
            return PaceOutcome {
                waited_secs: now.saturating_sub(started),
                source,
            };
        }

        // Announce once per distinct decision, not once per sleep chunk: a
        // seven-day park would otherwise write thousands of identical audit
        // lines and scroll the operator's terminal for days.
        let fingerprint = match &decision {
            PaceDecision::WaitUntil {
                window, reset_at, ..
            } => Some(((*window).to_string(), *reset_at)),
            _ => None,
        };
        if announced != fingerprint {
            announced = fingerprint;
            let _ = writeln!(w, "zirv ctx {verb}: {}", describe(&decision));
            let _ = log::append(
                state,
                &log::Decision {
                    ts: now_secs(),
                    session,
                    verb,
                    verdict: "paced",
                    score: 0,
                    action: "pace-wait",
                    detail: &describe(&decision),
                },
            );
        }

        let remaining = deadline.saturating_sub(now).min(cap);
        if remaining == 0 {
            return PaceOutcome {
                waited_secs: now.saturating_sub(started),
                source,
            };
        }
        sleep_fn(Duration::from_secs(remaining.min(SLEEP_CHUNK_SECS)));
    }
}
```

The `describe` line is written before sleeping so the operator immediately sees why a supervisor went quiet.

- [ ] **Step 8: Run them and see them pass**

Run: `cargo test ctx::pace -- --test-threads=1 2>&1 | tail -20`
Expected: PASS, 34 tests. If `an_exhausted_window_waits_past_the_reset_then_proceeds` loops forever, the fake clock is not advancing: the `sleep_fn` closure must add the slept seconds to `clock.now`. `an_exhausted_week_waits_for_the_real_reset_rather_than_resuming_early` drives about 14000 fake-clock iterations at the 30s chunk, which is fast because nothing really sleeps; if it is slow, check that `current_windows` is not walking transcripts (with no budget configured the estimator must be skipped entirely).

- [ ] **Step 9: Add a limit-hit mode to the fake agent**

In `tests/fixtures/fake-agent.sh`, extend the mode documentation and the final `case` so a run can emit the documented notice and exit non-zero:

```sh
#   limit    writes a healthy transcript, prints the documented limit-hit
#            notice on stdout, then exits 1 the way an exhausted window would
```

and in the trailing `case "$mode"` block, before the `*)` arm:

```sh
  limit)
    printf "You've hit your session limit · resets 3:45pm\n"
    exit 1
    ;;
```

Run: `FAKE_AGENT_MODE=limit HOME=$(mktemp -d) sh tests/fixtures/fake-agent.sh -p x --session-id 11111111-2222-4333-8444-555555555555; echo "exit=$?"`
Expected: the notice line and `exit=1`.

- [ ] **Step 10: Write the failing exec integration tests**

Add to the `mod tests` in `src/commands/ctx/exec.rs`:

```rust
    use crate::commands::ctx::window::{self, UsageWindows, Window};

    fn store_collector(state_dir: &std::path::Path, percent: f64, resets_in: u64) {
        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.to_path_buf());
        let now = crate::commands::ctx::state::now_secs();
        window::store(
            &state,
            &UsageWindows {
                five_hour: Some(Window {
                    used_percentage: percent,
                    resets_at: now + resets_in,
                    observed_at: now,
                }),
                seven_day: None,
            },
        )
        .expect("store collector state");
    }

    #[test]
    fn a_limit_hit_parks_and_relaunches_without_spending_the_restart_budget() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "99999999-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        // A reset one second out plus no jitter keeps the park short; the point
        // is that it parks and relaunches, not how long it waits.
        env.insert("ZIRV_CTX_PACE_JITTER_SECS".to_string(), "0".to_string());
        env.insert("ZIRV_CTX_PACE_FALLBACK_SECS".to_string(), "1".to_string());
        env.insert("ZIRV_CTX_PACE_MAX_WAIT_SECS".to_string(), "2".to_string());

        // First child hits the limit, second runs clean.
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "limit\nhealthy\n").expect("write modes");

        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            // Zero budget: a limit hit must park even with no restarts allowed,
            // because a park is not a restart.
            max_restarts: Some(0),
            timeout_secs: Some(60),
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
        }

        assert_eq!(
            code.expect("runs"),
            0,
            "the relaunched child finished cleanly, so exec exits with its code"
        );

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"limit-park\""), "got {log}");
        assert!(
            !log.contains("\"action\":\"give-up\""),
            "a park must not consume the restart budget: {log}"
        );
        assert_eq!(
            transcripts_in(&home).len(),
            2,
            "the relaunch is a new session with its own transcript"
        );
    }

    #[test]
    fn an_exhausted_window_delays_the_first_spawn() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "aaaaaaaa-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE_JITTER_SECS".to_string(), "0".to_string());
        env.insert("ZIRV_CTX_PACE_MAX_WAIT_SECS".to_string(), "2".to_string());
        store_collector(&state, 100.0, 1);

        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(60),
            command: fake_agent_command(session),
        };
        let started = std::time::Instant::now();
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), 0, "a pause is never an exit");
        assert!(
            started.elapsed() >= std::time::Duration::from_secs(1),
            "it should have waited before spawning"
        );
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"pace-wait\""), "got {log}");
    }

    #[test]
    fn a_healthy_window_adds_no_delay() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "bbbbbbbb-2222-4333-8444-555555555555";
        let env = base_env(&state);
        store_collector(&state, 5.0, 3600);

        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(60),
            command: fake_agent_command(session),
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 0);
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).unwrap_or_default();
        assert!(!log.contains("pace-wait"), "nothing to wait for: {log}");
    }
```

- [ ] **Step 11: Run them and see them fail**

Run: `cargo test ctx::exec -- --test-threads=1 2>&1 | tail -25`
Expected: FAIL. `a_limit_hit_parks_and_relaunches_without_spending_the_restart_budget` gets exit `1` (the limit-hit child's own code) with no `limit-park` log line, because `exec` does not yet look for the notice.

- [ ] **Step 12: Wire pacing into `exec`**

In `src/commands/ctx/exec.rs`:

1. Add `use super::pace;` to the imports. `PaceConfig` is reached as `cfg.pace` from the already-loaded `CtxConfig`, so it needs no import.
2. Add a real-clock pair once, above the loop, so the gate can be called from both places. `now_secs` is already imported directly at `exec.rs:10`, so use the bare symbol rather than a `state::` path:

```rust
    let now_fn = now_secs;
    let sleep_fn = |d: Duration| std::thread::sleep(d);
```

3. Immediately before `let mut child = supervise::spawn(command)?;`, gate the spawn and switch to the tapped spawn:

```rust
        pace::wait_for_window(
            w,
            &state,
            &cfg.pace,
            "exec",
            session.as_str(),
            &now_fn,
            &sleep_fn,
        );

        let (mut child, tap) = supervise::spawn_tapped(command)?;
```

4. Add a `limit_hit` flag next to `rotted`, and pass the tap into `supervise_run` so its tick checks the output first:

```rust
        let mut limit_hit = false;
```

In `supervise_run`, add `tap: &supervise::OutputTap` and `limit_hit: &mut bool` parameters, and put this at the top of the tick closure, before the signal and transcript checks:

```rust
        if tap.try_lines().iter().any(|line| pace::is_limit_hit(line)) {
            *limit_hit = true;
            return Tick::Stop("limit");
        }
```

A trip is authoritative regardless of the other layers, which is why it is checked before scoring.

5. In the outcome handling, branch on `limit_hit` before the rot/timeout reason is computed, park, and relaunch without touching `restarts`:

```rust
        if limit_hit {
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: "limit",
                    score: 100,
                    action: "limit-park",
                    detail: "agent reported a usage limit; parking until the window resets",
                },
            );
            writeln!(
                w,
                "zirv ctx exec: the agent reported a usage limit, parking until the window resets"
            )?;

            pace::wait_for_window(
                w,
                &state,
                &cfg.pace,
                "exec",
                session.as_str(),
                &now_fn,
                &sleep_fn,
            );

            let Some(prompt_text) = prompt.clone() else {
                writeln!(
                    w,
                    "zirv ctx exec: usage limit hit and the original prompt is unknown, so it cannot relaunch. Pass --prompt to enable parking."
                )?;
                return Ok(EXIT_ROT_EXHAUSTED);
            };

            // A park is not a restart: the budget is for rot, not for waiting.
            session = SessionId::new_v4();
            transcript = derive_transcript(&session);
            command = adapter.headless_cmd(&prompt_text, &session, &[]);
            command.current_dir(repo);
            for (key, value) in &turn_env {
                command.env(key, value);
            }
            continue;
        }
```

Place this immediately after the `match outcome { Outcome::Exited(code) => return Ok(code), ... }` block, so a child that exited on its own is still reported normally. Note that a limit-hit child is stopped by the tick, so it reaches this branch rather than the `Exited` arm.

- [ ] **Step 13: Run them and see them pass**

Run: `cargo test ctx::exec -- --test-threads=1 2>&1 | tail -25`
Expected: PASS, 16 tests.

- [ ] **Step 14: Write the failing loop integration tests**

Add to the `mod tests` in `src/commands/ctx/run_loop.rs`:

```rust
    use crate::commands::ctx::window::{self, UsageWindows, Window};

    fn store_collector(state_dir: &std::path::Path, percent: f64, resets_in: u64) {
        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.to_path_buf());
        let now = crate::commands::ctx::state::now_secs();
        window::store(
            &state,
            &UsageWindows {
                five_hour: Some(Window {
                    used_percentage: percent,
                    resets_at: now + resets_in,
                    observed_at: now,
                }),
                seven_day: None,
            },
        )
        .expect("store collector state");
    }

    #[test]
    fn each_cycle_passes_the_pacing_gate_first() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE_JITTER_SECS".to_string(), "0".to_string());
        env.insert("ZIRV_CTX_PACE_MAX_WAIT_SECS".to_string(), "2".to_string());
        store_collector(&state, 100.0, 1);

        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_TURNS", "2");
        }
        let started = std::time::Instant::now();
        let mut out = Vec::new();
        let code = run_with(&args_for(1), &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_TURNS");
        }

        assert_eq!(code.expect("runs"), 0, "a pause is never an exit");
        assert!(started.elapsed() >= std::time::Duration::from_secs(1), "it waited");
        assert_eq!(transcripts_in(&home).len(), 1, "the cycle still ran");

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"pace-wait\""), "got {log}");
    }

    #[test]
    fn a_limit_hit_cycle_is_parked_and_is_not_a_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE_JITTER_SECS".to_string(), "0".to_string());
        env.insert("ZIRV_CTX_PACE_FALLBACK_SECS".to_string(), "1".to_string());
        env.insert("ZIRV_CTX_PACE_MAX_WAIT_SECS".to_string(), "2".to_string());

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "limit\nhealthy\n").expect("write modes");

        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_TURNS", "2");
        }
        let mut args = args_for(2);
        // One failure would end the loop, so this proves a park is not a failure.
        args.max_failures = Some(1);
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_TURNS");
        }

        assert_eq!(
            code.expect("runs"),
            0,
            "a usage limit is not a cycle failure: the window just needed time"
        );
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"limit-park\""), "got {log}");
        assert!(!log.contains("\"action\":\"give-up\""), "got {log}");
        assert_eq!(transcripts_in(&home).len(), 2, "both cycles ran");
    }
```

- [ ] **Step 15: Run them and see them fail**

Run: `cargo test ctx::run_loop -- --test-threads=1 2>&1 | tail -25`
Expected: FAIL. `each_cycle_passes_the_pacing_gate_first` finds no `pace-wait` line, and `a_limit_hit_cycle_is_parked_and_is_not_a_failure` exits `75` because the limit-hit cycle counts as a failure.

- [ ] **Step 16: Wire pacing into `loop`**

In `src/commands/ctx/run_loop.rs`:

1. Add `use super::pace;` and the clock pair above the loop, exactly as in `exec`. `now_secs` is already imported directly at `run_loop.rs:8`:

```rust
    let now_fn = now_secs;
    let sleep_fn = |d: Duration| std::thread::sleep(d);
```

2. After `cycle += 1;` and before the session id is generated:

```rust
        pace::wait_for_window(
            w,
            &state,
            &cfg.pace,
            "loop",
            "loop",
            &now_fn,
            &sleep_fn,
        );
```

The session id does not exist yet at gate time, so the log entry uses `"loop"` as its session, matching the existing `give-up` entry.

3. Switch the cycle to the tapped spawn and add the limit check to the tick:

```rust
        let (mut child, tap) = supervise::spawn_tapped(command)?;
        let mut watcher = Watcher::new(transcript.clone());
        let mut rotted = false;
        let mut limit_hit = false;
```

and at the top of the existing tick closure:

```rust
                if tap.try_lines().iter().any(|line| pace::is_limit_hit(line)) {
                    limit_hit = true;
                    return Tick::Stop("limit");
                }
```

4. Extend the outcome mapping so a limit hit is hygiene, not failure, exactly like rot:

```rust
        let (action, failed) = match outcome {
            // A usage limit is the window's fault, not the cycle's: park and
            // let the next cycle do the work.
            Outcome::StoppedByTick(_) if limit_hit => ("limit-park", false),
            // Rot is hygiene, not failure: the next cycle is the restart.
            Outcome::StoppedByTick(_) if rotted => ("rot-kill", false),
            Outcome::StoppedByTick(reason) => (reason, true),
            Outcome::TimedOut => ("timeout-kill", true),
            Outcome::Exited(0) => ("ok", false),
            Outcome::Exited(_) => ("nonzero-exit", true),
        };
```

5. When `limit_hit`, wait for the window before the next cycle rather than falling through to the interval sleep:

```rust
        if limit_hit {
            pace::wait_for_window(
                w,
                &state,
                &cfg.pace,
                "loop",
                session.as_str(),
                &now_fn,
                &sleep_fn,
            );
        }
```

Place it immediately after the `log::append` for the cycle outcome and before `handle_cycle_outcome`.

- [ ] **Step 17: Run them and see them pass**

Run: `cargo test ctx::run_loop -- --test-threads=1 2>&1 | tail -25`
Expected: PASS, 12 tests.

- [ ] **Step 18: Run the whole suite and the lints**

Run: `cargo test --verbose -- --test-threads=1 2>&1 | tail -15`
Expected: PASS.
Run: `cargo fmt -- --check && cargo clippy --all-targets -- -D warnings 2>&1 | tail -20`
Expected: clean.

- [ ] **Step 19: Verify output passthrough survived the tap**

Run:

```bash
cargo build
WORK=$(mktemp -d) && cd "$WORK"
HOME="$WORK/home" FAKE_AGENT_MODE=healthy FAKE_AGENT_TURNS=2 \
ZIRV_CTX_AGENT_BIN="$OLDPWD/tests/fixtures/fake-agent.sh" \
ZIRV_CTX_STATE_DIR="$WORK/state" \
  "$OLDPWD/target/debug/zirv" ctx loop --prompt probe --cycles 1 --interval-secs 0
```

Expected: the cycle line appears and the run exits 0. The fake agent prints nothing, so to check passthrough specifically:

```bash
ZIRV_CTX_STATE_DIR="$WORK/state" "$OLDPWD/target/debug/zirv" ctx exec --agent claude \
  --prompt probe --max-restarts 0 -- sh -c 'printf "AGENT SAYS HELLO\n"; exit 0'
```

Expected: `AGENT SAYS HELLO` appears on the terminal. If it does not, `spawn_tapped` is swallowing output and the forward threads are wrong.

- [ ] **Step 20: Record the empirical follow-up**

Append to `docs/superpowers/notes/2026-07-31-claude-usage-window-facts.md`, under the limit-hit section:

```markdown
- FOLLOW-UP (opened with Phase E, task E4): the matcher in `src/commands/ctx/pace.rs`
  (`LIMIT_HIT_PATTERNS`) ships with exactly the three strings documented above and
  nothing else. Two plausible phrasings ("hit your sonnet limit", "hit your usage
  limit") are listed as commented-out candidates in that constant's doc comment
  and are deliberately NOT matched. Confirm empirically the next time a window is
  genuinely exhausted: capture the exact stdout/stderr line and the exit code of
  `claude -p` under an exhausted window, then promote the observed string into the
  list and record the exit code here. Until then a limit hit is detected by output
  text alone, never by exit code.
```

- [ ] **Step 21: Commit**

```bash
git add src/commands/ctx/pace.rs src/commands/ctx/supervise.rs src/commands/ctx/exec.rs src/commands/ctx/run_loop.rs tests/fixtures/fake-agent.sh docs/superpowers/notes/2026-07-31-claude-usage-window-facts.md
git commit -m "feat(ctx): pace supervised runs against usage windows and park on limit hits"
```

---

### Task E5: `zirv ctx usage` report and documentation

**Files:**
- Modify: `src/commands/ctx/usage.rs`
- Modify: `README.md` (usage pacing section)
- Modify: `src/commands/ctx/status.rs:16-70` (one usage line in the status report)

**Interfaces:**
- Consumes: `window::{load, UsageWindows, Window, age_secs}` (E1); `pace::{current_windows, decide, describe, wait_cap, PaceDecision, Source}` (E3/E4); `CtxConfig` (`config.rs`); `StateDir` (`state.rs`).
- Produces: `pub fn report<W: Write>(w: &mut W, collector: &UsageWindows, estimator: Option<&UsageWindows>, now: u64, cfg: &PaceConfig) -> CtxResult<()>` and a working `run_with` for the no-subcommand case.

- [ ] **Step 1: Write the failing report tests**

Add to the `mod tests` in `src/commands/ctx/usage.rs`:

```rust
    use crate::commands::ctx::config::PaceConfig;
    use crate::commands::ctx::window::{UsageWindows, Window};

    const NOW: u64 = 1_785_507_315;

    fn collector_at(percent: f64, age: u64) -> UsageWindows {
        UsageWindows {
            five_hour: Some(Window {
                used_percentage: percent,
                resets_at: NOW + 1800,
                observed_at: NOW - age,
            }),
            seven_day: None,
        }
    }

    #[test]
    fn the_report_names_each_window_and_its_freshness() {
        let mut out = Vec::new();
        report(&mut out, &collector_at(63.0, 42), None, NOW, &PaceConfig::default())
            .expect("report");
        let text = String::from_utf8(out).expect("utf8");

        assert!(text.contains("five_hour"), "got {text}");
        assert!(text.contains("63"), "got {text}");
        assert!(text.contains("42s ago"), "freshness must be visible: {text}");
        assert!(text.contains("seven_day"), "absent windows are still listed: {text}");
        assert!(!text.contains('\u{2014}'));
    }

    #[test]
    fn an_absent_window_says_so_rather_than_showing_zero() {
        let mut out = Vec::new();
        report(&mut out, &UsageWindows::default(), None, NOW, &PaceConfig::default())
            .expect("report");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("not reported"),
            "no data must never look like 0%: {text}"
        );
        assert!(
            text.contains("statusline") || text.contains("zirv ctx usage tee"),
            "tell the user how to start collecting: {text}"
        );
    }

    #[test]
    fn a_stale_collector_reading_is_labeled_stale() {
        let mut out = Vec::new();
        report(&mut out, &collector_at(50.0, 100_000), None, NOW, &PaceConfig::default())
            .expect("report");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("stale"), "got {text}");
    }

    #[test]
    fn estimator_output_is_labeled_an_approximation() {
        let estimated = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 12.5,
                resets_at: NOW + 600,
                observed_at: NOW,
            }),
            seven_day: None,
        };
        let mut out = Vec::new();
        report(
            &mut out,
            &UsageWindows::default(),
            Some(&estimated),
            NOW,
            &PaceConfig::default(),
        )
        .expect("report");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("approximation"), "got {text}");
        assert!(text.contains("12.5"), "got {text}");
    }

    #[test]
    fn the_report_ends_with_the_pacing_verdict() {
        let mut out = Vec::new();
        report(&mut out, &collector_at(99.5, 10), None, NOW, &PaceConfig::default())
            .expect("report");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("waiting") || text.contains("would wait"), "got {text}");
        assert!(text.contains("99"), "got {text}");
    }

    #[test]
    fn the_report_explains_the_per_window_wait_bound() {
        let mut out = Vec::new();
        report(&mut out, &collector_at(50.0, 10), None, NOW, &PaceConfig::default())
            .expect("report");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("wait bound"), "got {text}");
        assert!(text.contains("21600"), "five hours plus slack: {text}");
        assert!(text.contains("608400"), "seven days plus slack: {text}");
        assert!(text.contains("own length plus slack"), "got {text}");
    }

    #[test]
    fn the_report_flags_an_absolute_wait_override() {
        let cfg = PaceConfig {
            max_wait_secs: Some(7200),
            ..PaceConfig::default()
        };
        let mut out = Vec::new();
        report(&mut out, &collector_at(50.0, 10), None, NOW, &cfg).expect("report");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("7200"), "got {text}");
        assert!(text.contains("override in effect"), "got {text}");
    }

    #[test]
    fn the_report_says_when_pacing_is_switched_off() {
        let cfg = PaceConfig {
            enabled: false,
            ..PaceConfig::default()
        };
        let mut out = Vec::new();
        report(&mut out, &collector_at(99.9, 10), None, NOW, &cfg).expect("report");
        assert!(String::from_utf8_lossy(&out).contains("pacing is disabled"));
    }

    #[test]
    fn the_verb_reports_without_a_subcommand() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            tmp.path().join("state").display().to_string(),
        )]
        .into();

        let mut out = Vec::new();
        let code = run_with(
            &UsageArgs { action: None },
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
        )
        .expect("report runs with no state at all");
        assert_eq!(code, 0);
        assert!(String::from_utf8_lossy(&out).contains("not reported"));
    }
```

- [ ] **Step 2: Run them and see them fail**

Run: `cargo test ctx::usage 2>&1 | tail -20`
Expected: FAIL to compile, `cannot find function report`.

- [ ] **Step 3: Write the report**

Add to `src/commands/ctx/usage.rs`:

```rust
use super::config::{CtxConfig, PaceConfig};
use super::pace::{self, PaceDecision};
use super::window::{UsageWindows, Window, age_secs};

fn line_for(
    w: &mut impl Write,
    name: &str,
    window: Option<&Window>,
    now: u64,
    cfg: &PaceConfig,
    label: &str,
) -> CtxResult<()> {
    match window {
        Some(found) => {
            let age = age_secs(found, now);
            let freshness = if age > cfg.collector_max_age_secs {
                format!("{age}s ago, stale")
            } else {
                format!("{age}s ago")
            };
            let reset = if found.resets_at == 0 {
                "reset time unknown".to_string()
            } else {
                format!("resets at unix {}", found.resets_at)
            };
            writeln!(
                w,
                "  {name}: {:.1}% used ({label}, observed {freshness}, {reset})",
                found.used_percentage
            )?;
        }
        None => writeln!(w, "  {name}: not reported")?,
    }
    Ok(())
}

pub fn report<W: Write>(
    w: &mut W,
    collector: &UsageWindows,
    estimator: Option<&UsageWindows>,
    now: u64,
    cfg: &PaceConfig,
) -> CtxResult<()> {
    writeln!(w, "collector (server-authoritative, from the statusline tee):")?;
    line_for(w, "five_hour", collector.five_hour.as_ref(), now, cfg, "collector")?;
    line_for(w, "seven_day", collector.seven_day.as_ref(), now, cfg, "collector")?;

    if collector.five_hour.is_none() && collector.seven_day.is_none() {
        writeln!(
            w,
            "  no readings yet. Wire your statusline through `zirv ctx usage tee -- <your statusline command>`; Claude reports these fields only for Pro and Max sessions, after the first response."
        )?;
    }

    match estimator {
        Some(windows) => {
            writeln!(w, "\nestimator (approximation from local transcripts):")?;
            line_for(w, "five_hour", windows.five_hour.as_ref(), now, cfg, "approximation")?;
            line_for(w, "seven_day", windows.seven_day.as_ref(), now, cfg, "approximation")?;
            writeln!(
                w,
                "  token class weighting is undocumented, so treat these as an approximation, never ground truth."
            )?;
        }
        None => {
            writeln!(w, "\nestimator: off (set pace.five_hour_budget_tokens or pace.seven_day_budget_tokens to enable it)")?;
        }
    }

    writeln!(w, "\npacing:")?;
    if !cfg.enabled {
        writeln!(w, "  pacing is disabled (pace.enabled = false)")?;
        return Ok(());
    }
    writeln!(w, "  ceiling {:.1}%", cfg.max_percent)?;
    writeln!(
        w,
        "  wait bound: five_hour up to {}s, seven_day up to {}s{}",
        pace::wait_cap("five_hour", cfg),
        pace::wait_cap("seven_day", cfg),
        if cfg.max_wait_secs.is_some() {
            " (absolute override in effect)"
        } else {
            " (each window's own length plus slack)"
        }
    )?;
    let decision = pace::decide(collector, estimator, now, cfg);
    let verb = match decision {
        PaceDecision::WaitUntil { .. } => "would wait:",
        _ => "verdict:",
    };
    writeln!(w, "  {verb} {}", pace::describe(&decision))?;
    Ok(())
}
```

Replace the `None` arm of `run_with`:

```rust
        None => {
            let cfg = CtxConfig::load(repo, env)?;
            let state = StateDir::resolve(env)?;
            let now = now_secs();
            let (collector, estimator) = pace::current_windows(&state, &cfg.pace, now);
            report(w, &collector, estimator.as_ref(), now, &cfg.pace)?;
            Ok(0)
        }
```

- [ ] **Step 4: Run them and see them pass**

Run: `cargo test ctx::usage -- --test-threads=1 2>&1 | tail -20`
Expected: PASS, 19 tests.

- [ ] **Step 5: Write the failing status-line test**

Add to the `mod tests` in `src/commands/ctx/status.rs`:

```rust
    #[test]
    fn status_mentions_the_usage_windows() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());

        crate::commands::ctx::window::store(
            &state,
            &crate::commands::ctx::window::UsageWindows {
                five_hour: Some(crate::commands::ctx::window::Window {
                    used_percentage: 77.0,
                    resets_at: 1_785_509_000,
                    observed_at: crate::commands::ctx::state::now_secs(),
                }),
                seven_day: None,
            },
        )
        .expect("store");

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("usage"), "got {text}");
        assert!(text.contains("77"), "got {text}");
    }
```

- [ ] **Step 6: Run it and see it fail**

Run: `cargo test ctx::status 2>&1 | tail -20`
Expected: FAIL, the status output has no usage line.

- [ ] **Step 7: Add the usage line to `status`**

In `src/commands/ctx/status.rs::run_with`, after the supervised-sessions block and before the handoff block:

```rust
    let windows = crate::commands::ctx::window::load(&state);
    let describe = |name: &str, window: Option<&crate::commands::ctx::window::Window>| match window {
        Some(found) => format!("{name} {:.0}%", found.used_percentage),
        None => format!("{name} unknown"),
    };
    writeln!(
        w,
        "\nusage windows: {}, {} (see `zirv ctx usage` for detail)",
        describe("five_hour", windows.five_hour.as_ref()),
        describe("seven_day", windows.seven_day.as_ref())
    )?;
```

- [ ] **Step 8: Run it and see it pass**

Run: `cargo test ctx::status 2>&1 | tail -20`
Expected: PASS, 5 tests.

- [ ] **Step 9: Document usage pacing in the README**

Phase E lands before Task D1, so the `## Context Management (zirv ctx)` section may not exist yet. If it does not, create it now with just the heading, the TOC entry `- [Context Management (zirv ctx)](#context-management-zirv-ctx)` after `- [Shortcuts](#shortcuts)`, and this verb table row; D1 then **extends** that section rather than replacing it:

```markdown
| `zirv ctx usage` | Show usage-window state, or `usage tee` to collect it from the statusline |
```

Add this subsection at the end of that section (D1 places "### Configuration" above it):

```markdown
### Usage pacing

Long autonomous runs die if a subscription window (5 hour rolling, 7 day) runs
dry mid-task. `zirv ctx loop` and `zirv ctx exec` consult a pacing gate before
every spawn and every restart, and wait instead of exiting when a window is at
or above `pace.max_percent` (default 99).

Three data layers, best available wins:

1. **Collector**, server-authoritative. Claude Code's statusline input carries
   `rate_limits.five_hour` and `rate_limits.seven_day` for Pro and Max sessions
   after the first response. Wire your statusline through the tee and every live
   session keeps machine-wide state fresh:

   ```json
   {
     "statusLine": {
       "type": "command",
       "command": "zirv ctx usage tee -- bash ~/.claude/statusline-command.sh"
     }
   }
   ```

   The tee records the fields, then runs your original command unchanged. It
   always exits 0 and always prints a statusline, so a failure here can never
   leave you looking at a blank one.

2. **Estimator**, an approximation. When no fresh collector reading exists, zirv
   sums token usage across local transcripts (including subagent files) over the
   trailing window. It is off until you set a budget, because a plan's real token
   allowance is undocumented and a made-up default would read as data:

   ```toml
   [pace]
   five_hour_budget_tokens = 0   # set to enable the 5h estimate
   seven_day_budget_tokens = 0   # set to enable the 7d estimate
   count_cache_reads = false     # cache reads are discounted, so excluded
   ```

3. **Circuit breaker**, authoritative on trip. If the agent prints a documented
   limit-hit notice, that is treated as 100% no matter what the other layers say:
   the run is parked until the window resets and then relaunched, **without
   consuming the restart budget**.

Full pacing configuration:

```toml
[pace]
enabled = true
max_percent = 99.0
collector_max_age_secs = 900
estimator = true
jitter_secs = 30
fallback_delay_secs = 900    # used when a window's reset time is unknown
wait_slack_secs = 3600       # head room added to the window's own length
# max_wait_secs = 7200       # optional absolute override, see below
```

#### How long a pause can last

The wait is bounded per window, not by one global clock: at most the window's own
length plus `wait_slack_secs`, so a five-hour trip is bounded near six hours and
a seven-day trip is allowed to wait out the week. That distinction matters,
because resuming a seven-day window every few hours would spend tokens against a
window that has not reset, which is exactly what pacing exists to prevent.

When a window's reset time is known and lands inside that bound, the pause ends
at the reset (plus jitter) and not before. Set `max_wait_secs` only if you would
rather a supervisor give up waiting and proceed after a fixed time; it replaces
the per-window bound entirely and is unset by default.

A pause is announced once, not once per check, and appears in the decision log as
a single `pace-wait` entry. Parks and relaunches are logged too. Check the
current picture, including how fresh each reading is, with `zirv ctx usage`.
```

- [ ] **Step 10: Verify the docs against reality**

Run: `cargo run --quiet -- ctx usage 2>&1 | tail -20`
Expected: the three-section report. With no statusline tee wired yet it says `not reported` and names the tee command, which is the honest answer rather than a fabricated percentage.

Run: `cargo run --quiet -- ctx --help 2>&1 | tail -20`
Expected: nine verbs, matching the README table.

Run: `grep -n '\u{2014}' README.md src/commands/ctx/usage.rs src/commands/ctx/pace.rs src/commands/ctx/window.rs`
Expected: no output.

- [ ] **Step 11: Run the full pipeline as CI does**

Run: `cargo test --verbose -- --test-threads=1 2>&1 | tail -15`
Expected: PASS.
Run: `cargo fmt -- --check && cargo clippy --all-targets -- -D warnings 2>&1 | tail -10`
Expected: clean.
Run: `cargo build --release 2>&1 | tail -5`
Expected: success.

- [ ] **Step 12: Commit**

```bash
git add src/commands/ctx/usage.rs src/commands/ctx/status.rs README.md
git commit -m "feat(ctx): zirv ctx usage report and pacing documentation"
```

---

### Task D1: Version bump to 2.5.0 and documentation

**Files:**
- Modify: `Cargo.toml:3`
- Modify: `README.md` (new `zirv ctx` section, table of contents, install example version)
- Modify: `CLAUDE.md` (architecture list, conventions)
- Modify: `src/commands/help.rs` (list the built-in in shortcut help)

**Interfaces:**
- Consumes: every verb from Phases A to C.
- Produces: a released 2.5.0 with documentation covering all eight verbs, config layering, hook registration, the shell alias, and the migration recipe.

The CD pipeline reads the version from `Cargo.toml` only (`grep '^version' Cargo.toml`), so nothing else needs bumping; `chocolatey/zirv/zirv.nuspec` and the Homebrew formula are rewritten by `scripts/update_chocolatey.ps1` and `scripts/update_homebrew.sh` from that value.

- [ ] **Step 1: Write the failing test**

Add to the test module in `src/commands/help.rs`:

```rust
    /// `zirv ctx` is a built-in, so it belongs in the shortcut list next to
    /// init, create, version and help.
    #[test]
    fn test_show_help_lists_the_ctx_builtin() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let temp_path = temp_dir.path().to_path_buf();
        let zirv_dir = setup_zirv_dir(&temp_path);
        write(zirv_dir.join("test.yaml"), "name: \"Test\"\ncommands: []\n")?;
        write(zirv_dir.join(".shortcuts.yaml"), "shortcuts:\n  t: \"test.yaml\"\n")?;

        let original_dir = env::current_dir()?;
        env::set_current_dir(&temp_path)?;
        let mut buffer = Cursor::new(Vec::new());
        let result = show_help(&mut buffer);
        env::set_current_dir(original_dir)?;
        result?;

        let output = String::from_utf8(buffer.into_inner())?;
        assert!(
            output.contains("ctx -> context management"),
            "got {output}"
        );
        Ok(())
    }
```

Add to the test module in `src/commands/version.rs`:

```rust
    #[test]
    fn test_version_is_at_least_2_5_0() {
        let version = env!("CARGO_PKG_VERSION");
        let parts: Vec<u32> = version
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect();
        assert!(parts.len() >= 2, "semantic version expected, got {version}");
        assert!(
            (parts[0], parts[1]) >= (2, 5),
            "zirv ctx ships in 2.5.0, got {version}"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test help::tests::test_show_help_lists_the_ctx_builtin version:: 2>&1 | tail -20`
Expected: both FAIL. The help output has no `ctx` line, and the version is still `2.4.0`.

- [ ] **Step 3: Bump the version and the help listing**

In `Cargo.toml` change `version = "2.4.0"` to `version = "2.5.0"`.

In `src/commands/help.rs::show_help`, add the line to the built-in block that already lists the other built-ins:

```rust
            writeln!(writer, "  i -> init")?;
            writeln!(writer, "  c -> create")?;
            writeln!(writer, "  v -> version")?;
            writeln!(writer, "  h -> help")?;
            writeln!(writer, "  ctx -> context management (score, loop, exec, wrap, handoff, resume, hook, status, usage)")?;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test help:: version:: 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Document `zirv ctx` in the README**

Task E5 may already have created `## Context Management (zirv ctx)` with its TOC entry, the `usage` verb row and the "### Usage pacing" subsection. **Extend that section, do not replace it:** keep the usage row and the pacing subsection, and add everything below around them. If the section does not exist yet, add `- [Context Management (zirv ctx)](#context-management-zirv-ctx)` to the table of contents after `- [Shortcuts](#shortcuts)`.

Change the pinned install example from `2.4.0` to `2.5.0`, and place this section just before `## Supported Platforms`:

```markdown
## Context Management (zirv ctx)

`zirv ctx` watches AI coding agent sessions (Claude Code, Codex) for context rot
and intervenes before quality drops: it advises, compacts early, or restarts the
session with a distilled handoff. Scoring is deterministic, and every decision is
logged.

### Verbs

| Command | What it does |
|---|---|
| `zirv ctx score --transcript <path>` | Rot-scores a transcript and prints JSON |
| `zirv ctx loop --prompt <text>` | Runs a fresh headless session per cycle, so the orchestrator cannot rot |
| `zirv ctx exec -- <agent command>` | Supervises one headless run: kill, distill, restart |
| `zirv ctx wrap -- claude` | Supervises an interactive TUI through a PTY |
| `zirv ctx handoff --transcript <path>` | Distills a handoff and stores it |
| `zirv ctx resume` | Starts a clean session with the latest handoff injected |
| `zirv ctx hook <stop\|prompt\|pre-compact\|notify>` | Agent hook entrypoints |
| `zirv ctx status` | Shows supervised sessions, recent decisions and handoffs |
| `zirv ctx usage` | Shows usage-window state, or `usage tee` to collect it from the statusline |

### Signals and verdicts

Four signals over the trailing window (default 10 turns):

1. **Context size** (a gate, not a vote). Below 100000 tokens the verdict is always
   `healthy`; at or above 160000 it is at least `compact`.
2. **Tool-failure rate** (weight 40).
3. **Repetition loops**, three or more identical tool calls with identical input
   (weight 30).
4. **Reply-marker misses** on final answers (weight 30, active only when the
   marker hook is installed and the session is at least 10 turns old).

Verdicts: score 40 or more is `advise`, 60 or more is `compact`, 80 or more is
`restart`. At the token ceiling a score of 60 or more escalates to `restart`.
Without the marker signal (Codex, or Claude without the prompt hook) behavioral
signals top out at 70, so a restart there comes only from the token ceiling.

### Configuration

Layered, lowest priority first: `~/.zirv/ctx.toml`, then `<repo>/.zirv/ctx.toml`,
then `ZIRV_CTX_*` environment variables, then flags.

```toml
# .zirv/ctx.toml
agent = "claude"

[score]
window = 10
min_turns = 10
token_floor = 100000
token_ceiling = 160000
marker = "[zirv]"
advise_at = 40
compact_at = 60
restart_at = 80

[wrap]
debounce_ms = 3000
inject_timeout_ms = 20000

[supervise]
max_restarts = 2
interval_secs = 900
max_cycle_secs = 3600
max_failures = 5

[handoff]
model = "haiku"
tail_items = 5
```

Handoffs, sockets and logs live in the platform state directory under
`zirv/ctx/`, never in the repo. Override with `ZIRV_CTX_STATE_DIR`.

### Hook registration (Claude Code)

Add to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "Stop": [{ "hooks": [{ "type": "command", "command": "zirv ctx hook stop" }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "zirv ctx hook prompt" }] }],
    "PreCompact": [{ "hooks": [{ "type": "command", "command": "zirv ctx hook pre-compact" }] }]
  }
}
```

The Stop hook forwards verdicts to a supervising `wrap` or `exec` when one owns
the session, and otherwise prints a non-blocking advisory. It never blocks a
stop. For Codex, point the `notify` program in `~/.codex/config.toml` at
`zirv ctx hook notify`.

### Interactive use

```bash
alias claude='zirv ctx wrap -- claude'
```

The wrapped session is byte-for-byte identical to an unwrapped one until an
intervention, injection happens only at a turn boundary while you are idle, and
any supervision failure drops it back to pure passthrough.

### Exit codes for headless supervision

| Code | Meaning |
|---|---|
| the child's own code | the run finished on its own |
| `75` | the restart budget was spent, or the loop hit its failure cap |
| `76` | a wall-clock timeout with no restarts left |

### Migrating an existing loop

Replace a long-lived orchestrator session with a stateless loop, and wrap worker
dispatch so individual runs get restarted rather than merely killed:

```yaml
# .zirv/issue-loop.yaml
name: Issue Loop
commands:
  - command: zirv ctx loop --prompt-file .zirv/issue-loop-prompt.md --interval-secs 900
```

```bash
zirv ctx exec --prompt "$WORKER_PROMPT" -- claude -p "$WORKER_PROMPT" --session-id "$SID"
```

Durable state must live outside the session (GitHub issues and labels, for
example), because every cycle starts with a clean context. Once `zirv ctx hook
stop` is registered, remove any older canary Stop hook from
`~/.claude/settings.json`: two Stop hooks scoring the same session is noise, and
the older one blocks stops, which this one deliberately never does.
```

- [ ] **Step 6: Update CLAUDE.md**

Add to the Architecture list:

```markdown
- `src/commands/ctx/` — Context management for AI agent sessions (`zirv ctx <verb>`)
  - `mod.rs` — Verb tree and dispatch, intercepted in `main.rs` before script lookup
  - `config.rs` / `state.rs` / `log.rs` — Layered config, platform state dir, decision log
  - `event.rs` / `rot.rs` — Normalized events and the pure deterministic rot engine
  - `adapters/` — `AgentAdapter` trait plus the claude and codex adapters
  - `score.rs` / `handoff.rs` / `resume.rs` / `hook.rs` / `status.rs` — One module per verb
  - `run_loop.rs` / `exec.rs` / `wrap.rs` — The three supervisors (`loop` is a keyword)
  - `signal.rs` / `supervise.rs` / `term.rs` — Turn-signal sockets, process primitives, raw mode
  - `usage.rs` / `window.rs` / `pace.rs` — Usage pacing: statusline tee, window state and estimator, the gate
```

Add to Conventions:

```markdown
- `zirv ctx` is a built-in resolved in `main.rs` before YAML script lookup, so a
  `.zirv/ctx.yaml` script named `ctx` is shadowed. `.zirv/ctx.toml` is the ctx
  config file and is excluded from script listing in `help.rs`.
- The rot engine is pure: no clock, no filesystem, no environment reads inside
  `rot.rs`, so the same events always produce the same verdict.
- `wrap` must never make a session worse. No `unwrap`/`expect` on its hot path,
  raw-mode restore happens in explicit arms (the release profile is
  `panic = "abort"`), and any supervision failure degrades to pure passthrough.
- Test fixtures under `tests/fixtures/` are data files only; tests stay inline in
  `#[cfg(test)] mod tests`. Re-record the claude fixture with
  `scripts/record-claude-fixture.py`.
```

- [ ] **Step 7: Verify the docs match reality**

Run: `cargo run --quiet -- ctx --help 2>&1 | tail -20`
Expected: all eight verbs listed, matching the README table exactly. Fix any name drift in the README.

Run: `grep -c '2\.4\.0' README.md`
Expected: `0`.

Run: `grep -n '\u{2014}' README.md src/commands/ctx/*.rs src/commands/ctx/adapters/*.rs`
Expected: no output (no em dashes in user-facing copy or docs added by this work).

- [ ] **Step 8: Run the full pipeline exactly as CI does**

Run: `cargo test --verbose -- --test-threads=1 2>&1 | tail -20`
Expected: PASS, everything.
Run: `cargo fmt -- --check`
Expected: no output.
Run: `cargo clippy --all-targets -- -D warnings 2>&1 | tail -20`
Expected: no warnings.
Run: `cargo build --release 2>&1 | tail -5`
Expected: success (this is the profile with `panic = "abort"`, so it proves no code depends on unwinding).

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml Cargo.lock README.md CLAUDE.md src/commands/help.rs src/commands/version.rs
git commit -m "feat(ctx): release 2.5.0 with zirv ctx documentation"
```

---

## Self-Review

Run after the plan is written, before execution starts. Findings below were fixed inline.

**1. Spec coverage.** Every spec section maps to at least one task:

| Spec section | Tasks |
|---|---|
| Architecture overview, command family in `src/commands/ctx/` | A1 |
| `AgentAdapter` trait | A6 |
| claude adapter (all rows of the v1 table) | A7, A8 |
| codex adapter (verified, not assumed) | A9, A10 |
| Rot engine signals | A11 |
| Verdicts, token gates, canary case parity | A12 |
| `zirv ctx score` | A13 |
| Interactive supervision, injection preconditions | C3 |
| Escalation ladder (advise, compact, restart), cooldown, verified injection | C4, C5 |
| `zirv ctx loop` | B4, B5 |
| `zirv ctx exec` | B2, B3 |
| Handoff distillation, format, structural fallback, storage | A17, A18, A19 |
| `zirv ctx resume` | A20 |
| Hooks integration (all four rows) | A15, A16 |
| Configuration layering and tunables | A2 |
| State dir | A3 |
| Error handling (`Result` everywhere, `panic = "abort"`, passthrough degradation, verified injection, bounded restarts, decision log) | A3, B2, B3, B5, C1, C4, C6 |
| Testing strategy (all five categories) | A5 (fixtures), A7/A10 (adapters), A11/A12 (engine), B1 to B5 (loop/exec), C2 to C6 (wrap), A18 (handoff) |
| Usage pacing: collector layer (statusline `rate_limits`, tee into shared state, chain unchanged) | E1 |
| Usage pacing: estimator layer (trailing sums over `projects/**/*.jsonl` including `subagents/`, labeled an approximation, never overrides collector) | E2 |
| Usage pacing: `pace_max_percent` default 99, best-available-wins gate, decision-log entries | E3 |
| Usage pacing: `loop` before each cycle and `exec` before each spawn and restart, wait-not-exit, jitter, fallback delay when unknown | E4 |
| Usage pacing: circuit breaker (documented limit strings, authoritative on trip, park and relaunch without consuming the restart budget) | E4 |
| Usage pacing: `zirv ctx usage` window state, honest about staleness and absence | E5 |
| Migration and rollout | D1 |
| Versioning 2.5.0 | D1 |
| Dependencies (`portable-pty`, `uuid`) | A1 header, plus `libc` justified in Global Constraints |

**Two spec items are deliberately not implemented as written, both because verified reality contradicts the spec's sketch. Both are called out where they occur:**

- The spec's hook table promises PreCompact "compaction focus instructions". PreCompact hooks cannot inject instructions (docs-confirmed), so the focus text is delivered as arguments to `wrap`'s injected `/compact <focus>` command (Task C4) and `hook pre-compact` is observational (Task A16).
- The spec's trait sketch declares `fn detect(command: &[String]) -> bool` as an associated function, which is not dyn-compatible. It takes `&self` and selection walks a registry (Task A6).

**One spec step is out of this repository's scope:** migration step 2 edits `.zirv/issue-loop.yaml` in the zirv-fitness-tracking repo. Task D1 documents the recipe; the cross-repo change itself cannot be a task here.

**2. Placeholder scan.** No "TBD", no "add error handling", no "similar to Task N", no "write tests for the above". Every code step carries real code; every test step carries real assertions; every run step names the exact command and the expected outcome. Three tasks contain conditional branches, each with both sides spelled out concretely rather than deferred: A9/A10 (codex installed or not), C1 Step 1 (pty fd probe available or not), C2 Step 4 (`take_writer` callable once or twice).

**3. Type consistency.** Fixed during review:

- `Signals` gained `max_repeat` in A11 because `repetition_component` in A12 needs it; the A11 test asserts it, and `Score` serialization in A13 includes it.
- `AgentAdapter` gained `ready()`, `quit_sequence()`, `distiller_cmd(model)` and `structural_context(jsonl, last_n)` in A6, all four consumed later (A13 selection, C5 quit ladder, A18 distillation, A19 handoff). `distiller_cmd` takes only a model because the prompt goes over stdin.
- `StructuralContext` carries `user_messages`, `assistant_texts`, `files_touched`, `tool_errors`, used identically in A8, A17, A18 and A19.
- `Verdict` derives `Ord` (A12) because `verdict_for` uses `.max()` and `Deserialize` because `TurnSignal` (A14) round-trips it.
- Verb signature is uniformly `run<W: Write>(args, w) -> CtxResult<i32>` plus a `run_with(args, w, repo, env)` seam, established in A1 and honored by A13, A15, A16, A19, A20, A21, B2, B4 and C2.
- `EXIT_ROT_EXHAUSTED` and `EXIT_FAILED` are both `75` by design (`exec` and `loop` are separate binaries' worth of policy but share the caller contract); documented in the README table in D1.
- Test-fixture bug fixed: the rot-engine test helper originally used one constant tool input, which silently fired the repetition signal in tests asserting marker-only scores. Split into `turns` (distinct inputs) and `looping_turns` (identical inputs), and every affected assertion was recomputed.
- `handle_cycle_outcome` gained a `failures: &mut u32` parameter in B5; B4 introduces the function with the parameter list B5 extends, and B5 Step 1 says so explicitly.
- `wrap`'s PTY writer is a single `Arc<Mutex<Box<dyn Write + Send>>>` shared by the stdin pump and the injector, because `take_writer` can only be called once.

**Second review pass, four findings, all fixed:**

- **B2 watched a dead transcript after a restart.** `transcript` was computed once from the original session while `session` was reassigned per restart, so from the second child onward the watcher polled the killed child's file and re-read its rotted content. `run_with` now derives the transcript through a `derive_transcript` closure and reassigns it alongside `session` inside the loop, matching what B4 already did. `--transcript` is documented as describing the caller's first child only, since every restart is an adapter-launched session. A new test, `a_restart_supervises_the_new_sessions_transcript`, scripts the fake agent to rot then run healthy (new `FAKE_AGENT_MODE_FILE` support in B1) and asserts exit `0` plus two distinct transcript files; under the old code the healthy second child was killed and the run exited `75`.
- **A16's `run_notify` aliased `run_stop`,** silently assuming codex's notify payload uses claude's field names. A renamed field would have parsed as an empty `transcript_path` and dropped every codex turn signal with no diagnostic. There is now a real mapping, `notify_payload_to_hook`, driven by `NOTIFY_TRANSCRIPT_KEYS`, which errors when no known field is present and logs a `notify-unmapped` decision instead of scoring nothing. A9 gained a notify-recorder step that captures the real payload, and A10 gained a step that replaces the explicitly marked `CODEX_NOTIFY_SAMPLE` placeholder and the key list with verified values, plus an end-to-end check that the decision log does not say `notify-unmapped`. `HookPayload` gained `Serialize` for the round trip.
- **The multi-word `agent_bin` split moved from C5 to A8** and now applies to `headless_cmd`, `interactive_cmd` and `distiller_cmd` through a shared `base()`, so exec restarts and handoff distillation work with `ZIRV_CTX_AGENT_BIN="sh /tmp/stub.sh"` and not just wrap relaunches. Struct fields are `program` plus `bin_args`; A6's scaffold is marked as such, A9's codex interface mirrors it, and C5 now references A8 instead of introducing the behavior.
- **`HandoffConfig.last_assistant_texts` was dead config** and `last_user_messages` misdescribed what it limited, since `structural_context` applies one limit to user messages, assistant texts and tool errors alike. The two fields collapsed into one, `tail_items` (default 5), documented as such, asserted in A2's defaults test, exposed in the README config example, and read at all three call sites (A19, B2, C5).

**Phase E review pass (usage pacing), against the real Phase A and B code on this branch:**

- **Spec coverage:** every claim in the spec's "Usage pacing" section maps to a task in the table above. Both BLOCKED facts from the verified notes file are handled the Task A9 way rather than guessed: the estimator ships off by default (budgets `0`) and labeled an approximation everywhere it surfaces, and the limit-hit matcher ships against the three documented strings with an empirical follow-up appended to the notes file in E4 Step 20.
- **Placeholder scan:** no TBD, no "add error handling", no "similar to task N". Every step carries real code, a real command and an expected RED or GREEN. The one intentionally provisional item is the E4 matcher, and it is provisional in the notes file with a written follow-up rather than in the code as a hole.
- **Type consistency against the implemented tree, verified by reading it:** `StateDir::{from_root, resolve, root, logs}` and `now_secs` exist at `state.rs:15,41,47,57,70`; `log::{append, Decision}` at `log.rs:14,25`; `supervise::{spawn, supervise_child, Tick, Outcome, Watcher}` at `supervise.rs:12-116`; `EnvLookup` and `env_from_process` at `config.rs:14,18`; `crate::utils::home_dir` at `utils.rs:16`. E4's edits cite the real line ranges in `exec.rs` and `run_loop.rs`, and use the bare `now_secs` symbol because both files import it directly rather than via a `state::` path.
- **Real-code mismatches caught while writing Phase E, each fixed in the task text:** `supervise::spawn` inherits stdout and stderr (`supervise.rs:25-31`), so a limit-hit matcher is impossible without a new capture path; E4 adds `spawn_tapped` plus `OutputTap` and keeps `spawn` untouched for every other caller, with a manual passthrough check in E4 Step 19. `ExecArgs::command` uses `allow_hyphen_values` + `last` **without** `trailing_var_arg` (`exec.rs:40`, from commit d3f0ede, where the third attribute tripped a clap debug assertion that aborts the process); `UsageAction::Tee` copies that exact combination and says why. `config.rs`'s `EnvKind` had only `Int` and `Str`, so E3 adds `Float` and `Bool` with tests for both, including the case where `ZIRV_CTX_PACE_MAX_PERCENT=75` must load as a float rather than an integer.
- **Ordering fix:** Phase E lands before D1, but E5 documents pacing in the README section D1 creates. E5 now creates the heading and TOC entry when absent, and D1 explicitly extends rather than replaces it and carries the `usage` row in its verb table and the built-in help line.
- **Two deliberate judgment calls, both recorded in the task text:** cache-read tokens are excluded from the estimator by default (verified as the dominant class at 108427 of 108886 tokens in one real event, and API-discounted), with `count_cache_reads` to flip it; and estimator percentages exist only once an operator sets a budget, because no source documents a plan's real allowance and a default would be a guess presented as data.
- **No test touches the network or spends usage:** fixtures are `statusline-with-limits.json`, `statusline-no-limits.json`, `fake-statusline.sh`, synthetic transcripts built in-test, and the existing `fake-agent.sh` with a new `limit` mode. Timing is driven by an injected fake clock in the `pace` tests, so the gate's waiting behavior is asserted without real sleeping.

**Phase E second review pass, two findings, both fixed:**

- **The safety valve was a global six hours,** which quietly broke the spec's wait-until-reset semantics for the seven-day window: an exhausted week would resume roughly every six hours and spend tokens against a window that had not reset. The cap is now scaled to the window that tripped, `window_length + wait_slack_secs` (5h or 7d, plus 1h), via `pace::window_length` and `pace::wait_cap`, and `max_wait_secs` became `Option<u64>` defaulting to `None`, meaning it is now an explicit absolute override rather than an always-on ceiling. `wait_deadline` reads the window name it already carries in `PaceDecision::WaitUntil`, so no signature changed. Tests moved with it: `the_deadline_is_capped_by_max_wait` split into `the_cap_is_scaled_to_the_window_that_tripped`, `a_seven_day_trip_may_wait_days_not_hours`, `a_five_hour_trip_is_capped_near_five_hours` and `an_absolute_override_replaces_the_per_window_cap` in E3, plus `an_absolute_override_bounds_the_total_wait`, `a_bogus_five_hour_reset_is_bounded_by_the_window_not_by_six_hours` and `an_exhausted_week_waits_for_the_real_reset_rather_than_resuming_early` in E4; the defaults test now asserts `None` and `wait_slack_secs`. E5 reports the per-window bound and flags an override, and the README gained a "How long a pause can last" subsection.
- **Writing the seven-day test surfaced a second problem in my own design:** `wait_for_window` logged and printed on every 30-second chunk, so a five-day park would have written about 14000 identical audit lines. It now announces once per distinct decision (window plus reset time) and re-announces only when that changes, asserted by a `pace-wait` line count in `waiting_is_recorded_in_the_decision_log`.
- **`LIMIT_HIT_PATTERNS` carried five strings when the notes file documents three.** It now ships exactly the documented session, weekly and Opus phrasings; "hit your sonnet limit" (plausible by symmetry, undocumented) and "hit your usage limit" (invented) are commented candidates in the constant's doc comment, `only_the_documented_patterns_ship` pins the count at three and asserts both candidates do **not** match, and E4's follow-up note in the notes file says to promote a candidate only after observing it.

**4. Ordering.** `hook notify` (A16) depends on codex's real notify contract, so codex verification (A9) is sequenced before it. Fixtures (A5) precede the parser that reads them (A7). Supervision primitives (B1) precede both supervisors. `term.rs` (C1) precedes the PTY pump (C2).
