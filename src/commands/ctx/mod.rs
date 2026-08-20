use clap::{Parser, Subcommand};

pub mod adapters;
pub mod agent;
pub mod announce;
pub mod chat;
pub mod chrome;
pub mod config;
pub mod dash;
pub mod event;
pub mod exec;
pub mod handoff;
pub mod hook;
pub mod log;
pub mod mail;
pub mod memory;
pub mod memory_cli;
pub mod optimize;
pub mod pace;
pub mod poll;
pub mod prompt;
pub mod resume;
pub mod rot;
pub mod run_loop;
pub mod score;
pub mod sessions;
pub mod signal;
pub mod state;
pub mod status;
pub mod supervise;
pub mod term;
pub mod usage;
pub mod window;
pub mod wrap;

/// Shared helpers for the supervisor tests, which drive real child processes
/// and therefore have to steer process-wide state carefully.
#[cfg(test)]
pub(crate) mod testenv {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    /// A temp directory whose path carries no symlink. Production hands the
    /// supervisors `std::env::current_dir`, which is always fully resolved, and
    /// the agent files its transcript under a slug of its own working
    /// directory. On macOS the temp dir sits behind the `/var` to
    /// `/private/var` symlink, so an unresolved repo path leaves the supervisor
    /// watching a slug the agent never writes to.
    pub(crate) struct TestRepo {
        _dir: tempfile::TempDir,
        path: PathBuf,
    }

    impl TestRepo {
        pub(crate) fn path(&self) -> &Path {
            &self.path
        }
    }

    pub(crate) fn repo() -> TestRepo {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = std::fs::canonicalize(dir.path()).expect("resolve tempdir");
        TestRepo { _dir: dir, path }
    }

    /// Points the home directory (and optionally the working directory) at a
    /// test directory, putting every one of them back on drop.
    ///
    /// All of this is process-wide, so restoring has to survive a panicking
    /// assertion: a test that leaks `HOME` naming a deleted temp dir breaks
    /// every later pty spawn in the same run, because portable-pty starts its
    /// child in `$HOME` unless the caller sets a working directory and the
    /// `chdir` fails before the program is ever reached. A leaked working
    /// directory is worse still. Restoring on the happy path only -- which is
    /// what a `let result = test(); restore; result` helper does -- gets this
    /// exactly backwards: the failing test is the one that leaks.
    pub(crate) struct EnvGuard {
        home: Option<OsString>,
        userprofile: Option<OsString>,
        cwd: Option<PathBuf>,
    }

    impl EnvGuard {
        pub(crate) fn set(home: &Path, cwd: Option<&Path>) -> Self {
            let guard = Self {
                home: std::env::var_os("HOME"),
                userprofile: std::env::var_os("USERPROFILE"),
                cwd: cwd.and(std::env::current_dir().ok()),
            };
            // SAFETY: CI runs tests single-threaded.
            unsafe {
                std::env::set_var("HOME", home);
                std::env::set_var("USERPROFILE", home);
            }
            if let Some(cwd) = cwd {
                std::env::set_current_dir(cwd).expect("enter the test working directory");
            }
            guard
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(previous) = self.cwd.take() {
                let _ = std::env::set_current_dir(previous);
            }
            // SAFETY: CI runs tests single-threaded.
            unsafe {
                for (key, previous) in [
                    ("HOME", self.home.take()),
                    ("USERPROFILE", self.userprofile.take()),
                ] {
                    match previous {
                        Some(previous) => std::env::set_var(key, previous),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    /// Sets (or clears) arbitrary environment variables for the duration of a
    /// test, putting every one of them back on drop -- including on a
    /// panicking assertion, for the same reason `EnvGuard` restores there.
    ///
    /// Needed by the tests that pin *inheritance* behavior: what a child
    /// process inherits is a fact about the real process environment, and
    /// `portable_pty::CommandBuilder::new` reads `std::env::vars_os` directly
    /// rather than through any injectable lookup, so there is nothing to fake.
    pub(crate) struct VarGuard(Vec<(String, Option<OsString>)>);

    impl VarGuard {
        pub(crate) fn set(vars: &[(&str, Option<&str>)]) -> Self {
            let previous = vars
                .iter()
                .map(|(key, _)| ((*key).to_string(), std::env::var_os(key)))
                .collect();
            // SAFETY: CI runs tests single-threaded.
            unsafe {
                for (key, value) in vars {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
            Self(previous)
        }
    }

    impl Drop for VarGuard {
        fn drop(&mut self) {
            // SAFETY: CI runs tests single-threaded.
            unsafe {
                for (key, previous) in self.0.drain(..) {
                    match previous {
                        Some(previous) => std::env::set_var(&key, previous),
                        None => std::env::remove_var(&key),
                    }
                }
            }
        }
    }

    /// Enters `dir` and returns to the previous working directory on drop --
    /// including on a panicking assertion, which is the whole point.
    ///
    /// The process-wide working directory is the single most damaging thing a
    /// test can leak: every later test that resolves a relative path, and
    /// every child process spawned without an explicit `current_dir`, picks
    /// it up. A `set_current_dir(original)` written at the *end* of a test
    /// body restores it only when the test passes, which gets it exactly
    /// backwards -- the failing test is the one that leaks, and the leak then
    /// shows up as a cascade of unrelated failures (often against a temp
    /// directory that no longer exists).
    pub(crate) struct CwdGuard(Option<PathBuf>);

    impl CwdGuard {
        pub(crate) fn enter(dir: &Path) -> std::io::Result<Self> {
            let previous = std::env::current_dir().ok();
            std::env::set_current_dir(dir)?;
            Ok(Self(previous))
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            if let Some(previous) = self.0.take() {
                let _ = std::env::set_current_dir(previous);
            }
        }
    }

    /// `EnvGuard` without the working directory, which is all most tests need.
    pub(crate) struct HomeGuard(#[allow(dead_code)] EnvGuard);

    impl HomeGuard {
        pub(crate) fn set(home: &Path) -> Self {
            Self(EnvGuard::set(home, None))
        }
    }
}

/// Every ctx entry point returns this. Matches the error style used by the
/// rest of the crate (`Box<dyn std::error::Error>`).
pub type CtxResult<T> = Result<T, Box<dyn std::error::Error>>;

// Item 7: named here, in the text `zirv ctx --help` actually prints, so
// nothing implies an unready adapter works today. `readiness_note` generates
// the not-ready clause from the registry's own `ready()` calls rather than a
// literal, so it never drifts from adapters::codex::CodexAdapter::ready --
// the same wording a user hits directly via `--agent codex`.
//
// Perf: `clap`'s derive bakes `about` into `CtxCli::command()`, which runs on
// *every* `try_parse_from` -- i.e. every `dispatch()` call, whether or not
// help text is ever displayed. `readiness_note()` calls `ready()` on every
// registered adapter, and on Windows that walks `PATH`/`PATHEXT` per
// adapter, so an ordinary `ctx hook Stop` (once per turn) or `ctx usage tee`
// (once per statusline render) used to pay that cost for text nobody was
// about to read. `ctx_about()` computes it once per process and caches it,
// which is free within one process (tests, in particular, call `dispatch`
// hundreds of times) even though a fresh `zirv ctx ...` invocation is still
// its own process either way.
fn ctx_about() -> String {
    static ABOUT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ABOUT
        .get_or_init(|| {
            format!(
                "Autonomous context management for AI coding agents. {}",
                adapters::readiness_note()
            )
        })
        .clone()
}

#[derive(Debug, Parser)]
#[command(name = "zirv ctx", about = ctx_about(), disable_help_subcommand = true)]
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
    /// Report usage windows, or tee the statusline to record them.
    Usage(usage::UsageArgs),
    /// Analyse the configuration surfaces that steer every session.
    Optimize(optimize::OptimizeArgs),
    /// Start an interactive orchestrator session on the resolved adapter.
    Chat(chat::ChatArgs),
    /// Run a supervised headless worker on another enabled harness.
    Agent(agent::AgentArgs),
    /// Leave a note for other agent sessions on this machine.
    Send(mail::SendArgs),
    /// Read notes other agent sessions left for this one.
    Inbox(mail::InboxArgs),
    /// Store a durable fact in this repository's memory bank.
    Remember(memory::RememberArgs),
    /// List durable facts from this repository's memory bank.
    Recall(memory::RecallArgs),
    /// Remove one or all facts from this repository's memory bank.
    Forget(memory::ForgetArgs),
    /// Interrupt a live session with a message: durable mail plus a wake-up.
    Nudge(sessions::NudgeArgs),
}

/// What a clap parse failure costs, which is not the same for every verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseFailure {
    /// The ordinary case: clap printed the error, the caller sees exit 2.
    Reject,
    /// Claude Code reads a Stop hook's exit 2 as "block the stop", so a
    /// mistyped hook invocation would wedge the agent it is meant to watch.
    Hook,
    /// Claude Code renders whatever the statusline command prints, so exiting
    /// without a line looks like a broken terminal to the user.
    Statusline,
}

/// Reads the verb straight from argv, because by the time this runs clap has
/// already refused to tell us what was meant.
pub fn classify_parse_failure(args: &[String]) -> ParseFailure {
    match (
        args.get(1).map(String::as_str),
        args.get(2).map(String::as_str),
    ) {
        (Some("hook"), _) => ParseFailure::Hook,
        (Some("usage"), Some("tee")) => ParseFailure::Statusline,
        _ => ParseFailure::Reject,
    }
}

fn read_stdin() -> String {
    use std::io::Read;
    let mut buffer = String::new();
    let _ = std::io::stdin().read_to_string(&mut buffer);
    buffer
}

/// `args[0]` is the literal "ctx" as it appeared in argv.
pub fn dispatch(args: &[String]) -> i32 {
    let argv = std::iter::once("zirv ctx".to_string()).chain(args.iter().skip(1).cloned());
    let cli = match CtxCli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            // clap represents `--help`/`--version` as an `Err` too, since
            // printing and exiting is the caller's job here; both are
            // informational, not a rejected invocation, and must exit 0 like
            // top-level `zirv --help` already does via `Parser::parse()`'s
            // own exit path. A genuine parse error keeps falling through to
            // `classify_parse_failure` below.
            if matches!(
                err.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                return 0;
            }
            let mut out = std::io::stdout();
            return match classify_parse_failure(args) {
                ParseFailure::Reject => 2,
                ParseFailure::Hook => 0,
                // The same fallback the tee itself uses when its chained
                // command is missing or broken.
                ParseFailure::Statusline => usage::run_tee(&mut out, &read_stdin(), &[], None, 0),
            };
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
        CtxVerb::Usage(a) => usage::run(a, &mut out),
        CtxVerb::Optimize(a) => optimize::run(a, &mut out),
        CtxVerb::Chat(a) => chat::run(a, &mut out),
        CtxVerb::Agent(a) => agent::run(a, &mut out),
        CtxVerb::Send(a) => mail::run_send(a, &mut out),
        CtxVerb::Inbox(a) => mail::run_inbox(a, &mut out),
        CtxVerb::Remember(a) => memory::run_remember(a, &mut out),
        CtxVerb::Recall(a) => memory::run_recall(a, &mut out),
        CtxVerb::Forget(a) => memory::run_forget(a, &mut out),
        CtxVerb::Nudge(a) => sessions::run_nudge(a, &mut out),
    };

    match result {
        Ok(code) => code,
        Err(e) => {
            crate::output::error(e);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Item 7: `zirv ctx --help` is the first thing a curious user reads, so
    /// it must say plainly which adapters are not ready yet, the same
    /// honesty an adapter's own `ready()` gives a user who tries `--agent
    /// <name>` directly. Pinned as a property over the registry (every
    /// adapter whose own `ready()` fails must be named, with a not-ready
    /// indication, and -- the other direction -- nothing is named not-ready
    /// when every adapter's own `ready()` succeeds, true today now that
    /// `CodexAdapter::ready` mirrors claude's) rather than a literal
    /// sentence, so wiring up a real adapter -- or adding a third one that is
    /// not ready -- keeps this test honest without an edit.
    #[test]
    fn the_top_level_help_names_every_adapter_that_is_not_ready() {
        use clap::CommandFactory;
        let about = CtxCli::command()
            .get_about()
            .map(|s| s.to_string())
            .unwrap_or_default();

        let not_ready: Vec<_> = adapters::all(None)
            .into_iter()
            .filter(|a| a.ready().is_err())
            .collect();
        for adapter in &not_ready {
            assert!(
                about.contains(adapter.name()),
                "about must name not-ready adapter '{}': {about}",
                adapter.name()
            );
        }
        let claims_not_ready = about.to_lowercase().contains("not implemented yet")
            || about.to_lowercase().contains("not ready");
        assert_eq!(
            claims_not_ready,
            !not_ready.is_empty(),
            "about's not-ready claim must match the registry's own ready() calls: {about}"
        );
    }

    /// Item 16: the property test above reads `about` off `ctx_about()`'s
    /// process-wide `OnceLock` -- on any real machine both adapters are
    /// `ready()`, so its `for adapter in &not_ready` loop body has never
    /// once executed, and by the time this test runs the cache may already
    /// be warmed by an *earlier* test's own `CtxCli::try_parse_from` call
    /// (this module has several, and so do `optimize.rs`/`usage.rs`), which
    /// a same-test PATH rig cannot retroactively change. Rigged directly
    /// against `adapters::readiness_note()` instead -- the exact function
    /// `ctx_about()` wraps and caches, so this is the same substance without
    /// the caching hazard -- using the identical PATH/PATHEXT rig `adapters::
    /// tests::readiness_note_and_the_fallback_skip_both_stay_covered_when_
    /// an_adapter_is_genuinely_unready` already established for exactly this
    /// "force claude genuinely unready" shape.
    #[cfg(windows)]
    #[test]
    fn readiness_note_names_a_genuinely_unready_adapter() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("claude.py"), "print('x')\n").expect("write");

        let path = std::env::var("PATH").unwrap_or_default();
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[
            (
                "PATH",
                Some(format!("{};{}", dir.path().display(), path).as_str()),
            ),
            ("PATHEXT", Some(".EXE;.CMD;.PY")),
        ]);

        let not_ready: Vec<_> = adapters::all(None)
            .into_iter()
            .filter(|a| a.ready().is_err())
            .collect();
        assert!(
            !not_ready.is_empty(),
            "the rig must genuinely make claude unready"
        );

        let note = adapters::readiness_note();
        for adapter in &not_ready {
            assert!(
                note.contains(adapter.name()),
                "readiness_note (what ctx_about's cached `about` is built from) must name \
                 not-ready adapter '{}': {note}",
                adapter.name()
            );
        }
        assert!(note.to_lowercase().contains("not ready"), "got {note}");
    }

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

    /// `exec`'s own flags come before `--`, the headless agent command after.
    /// This exercises real argv parsing (not a struct literal), which is the
    /// only way clap's `trailing_var_arg` + `last` debug assertion is checked:
    /// a bad attribute combination on `ExecArgs::command` panics here instead
    /// of surfacing as a normal parse error, taking the whole process down.
    #[test]
    fn exec_verb_parses_own_flags_before_the_separator_and_command_after() {
        let cli = CtxCli::try_parse_from([
            "zirv ctx",
            "exec",
            "--agent",
            "claude",
            "--max-restarts",
            "2",
            "--",
            "claude",
            "-p",
            "hi",
        ])
        .expect("exec should parse flags before -- and a command after it");
        match cli.verb {
            CtxVerb::Exec(args) => {
                assert_eq!(args.agent, Some("claude".to_string()));
                assert_eq!(args.max_restarts, Some(2));
                assert_eq!(
                    args.command,
                    vec!["claude".to_string(), "-p".to_string(), "hi".to_string()]
                );
            }
            other => panic!("expected Exec, got {other:?}"),
        }
    }

    /// The trailing command can itself contain flag-shaped tokens (`--session-id`,
    /// `-p`) that must land in `command` verbatim, not be consumed as `exec`'s
    /// own flags: they appear after the `--` separator.
    #[test]
    fn exec_verb_preserves_hyphen_values_inside_the_trailing_command() {
        let cli = CtxCli::try_parse_from([
            "zirv ctx",
            "exec",
            "--timeout-secs",
            "30",
            "--",
            "claude",
            "--session-id",
            "abc",
            "-p",
            "hi",
        ])
        .expect("hyphen-prefixed values after -- must not be reparsed as exec flags");
        match cli.verb {
            CtxVerb::Exec(args) => {
                assert_eq!(args.timeout_secs, Some(30));
                assert_eq!(
                    args.command,
                    vec![
                        "claude".to_string(),
                        "--session-id".to_string(),
                        "abc".to_string(),
                        "-p".to_string(),
                        "hi".to_string()
                    ]
                );
            }
            other => panic!("expected Exec, got {other:?}"),
        }
    }

    /// `wrap`'s own flags come before `--`, the interactive agent command after.
    /// This exercises real argv parsing (not a struct literal), which is the
    /// only way clap's `trailing_var_arg` + `last` debug assertion is checked:
    /// a bad attribute combination on `WrapArgs::command` panics here instead
    /// of surfacing as a normal parse error, taking the whole process down.
    /// (`ExecArgs::command` hit exactly this bug; see d3f0ede.)
    #[test]
    fn wrap_verb_parses_own_flags_before_the_separator_and_command_after() {
        let cli = CtxCli::try_parse_from([
            "zirv ctx", "wrap", "--agent", "claude", "--", "claude", "-p", "hi",
        ])
        .expect("wrap should parse flags before -- and a command after it");
        match cli.verb {
            CtxVerb::Wrap(args) => {
                assert_eq!(args.agent, Some("claude".to_string()));
                assert!(!args.no_supervise);
                assert_eq!(
                    args.command,
                    vec!["claude".to_string(), "-p".to_string(), "hi".to_string()]
                );
            }
            other => panic!("expected Wrap, got {other:?}"),
        }
    }

    /// The trailing command can itself contain flag-shaped tokens (`--session-id`,
    /// `-p`) that must land in `command` verbatim, not be consumed as `wrap`'s
    /// own flags: they appear after the `--` separator.
    #[test]
    fn wrap_verb_preserves_hyphen_values_inside_the_trailing_command() {
        let cli = CtxCli::try_parse_from([
            "zirv ctx",
            "wrap",
            "--no-supervise",
            "--",
            "claude",
            "--session-id",
            "abc",
            "-p",
            "hi",
        ])
        .expect("hyphen-prefixed values after -- must not be reparsed as wrap flags");
        match cli.verb {
            CtxVerb::Wrap(args) => {
                assert!(args.no_supervise);
                assert_eq!(
                    args.command,
                    vec![
                        "claude".to_string(),
                        "--session-id".to_string(),
                        "abc".to_string(),
                        "-p".to_string(),
                        "hi".to_string()
                    ]
                );
            }
            other => panic!("expected Wrap, got {other:?}"),
        }
    }

    /// `--extra` carries the agent's own flags, which are hyphen-shaped almost
    /// by definition. Same clap bug class as the `ExecArgs::command` fix.
    #[test]
    fn loop_and_resume_accept_hyphen_shaped_extra_values() {
        let cli = CtxCli::try_parse_from([
            "zirv ctx", "loop", "--prompt", "go", "--extra", "--model", "--extra", "opus",
        ])
        .expect("loop --extra must accept the agent's own flags");
        match cli.verb {
            CtxVerb::Loop(args) => {
                assert_eq!(args.extra, vec!["--model".to_string(), "opus".to_string()])
            }
            other => panic!("expected Loop, got {other:?}"),
        }

        let cli = CtxCli::try_parse_from(["zirv ctx", "resume", "--extra", "--continue"])
            .expect("resume --extra must accept the agent's own flags");
        match cli.verb {
            CtxVerb::Resume(args) => assert_eq!(args.extra, vec!["--continue".to_string()]),
            other => panic!("expected Resume, got {other:?}"),
        }
    }

    #[test]
    fn unknown_verb_exits_two() {
        let code = dispatch(&["ctx".to_string(), "nope".to_string()]);
        assert_eq!(code, 2, "clap parse failure must map to exit code 2");
    }

    /// Bug (2026-08-02 validation of 2.5.0): clap represents `--help` as an
    /// `Err(...)` from `try_parse_from` (printing and exiting is the caller's
    /// job), and `dispatch` collapsed every parse failure to `classify_parse_
    /// failure`'s verdict, which only special-cases `hook` and `usage tee`.
    /// Every other verb's `--help` exited 2 instead of 0, breaking scripts
    /// that treat `--help` as success. Top-level `zirv --help` was never
    /// affected: it goes through `Parser::parse()`, which exits correctly on
    /// its own before any of this code runs.
    #[test]
    fn help_exits_zero_on_every_verb_and_bare_ctx() {
        for argv in [
            vec!["ctx", "--help"],
            vec!["ctx", "score", "--help"],
            vec!["ctx", "optimize", "--help"],
            vec!["ctx", "usage", "--help"],
            vec!["ctx", "status", "--help"],
            vec!["ctx", "wrap", "--help"],
            vec!["ctx", "exec", "--help"],
            vec!["ctx", "handoff", "--help"],
            vec!["ctx", "resume", "--help"],
            vec!["ctx", "loop", "--help"],
            vec!["ctx", "wrap", "-h"],
            vec!["ctx", "hook", "--help"],
        ] {
            let args: Vec<String> = argv.iter().map(|a| (*a).to_string()).collect();
            assert_eq!(dispatch(&args), 0, "--help must exit 0: {argv:?}");
        }
    }

    /// The invariant is "a hook always exits 0", and clap's own error path is
    /// part of the hook: exit 2 from a Stop hook blocks the agent's stop.
    #[test]
    fn a_hook_invocation_clap_rejects_still_exits_zero() {
        for argv in [
            vec!["ctx", "hook", "Stop"],
            vec!["ctx", "hook", "stop", "--bogus"],
            vec!["ctx", "hook", "notify", "-x"],
            vec!["ctx", "hook"],
        ] {
            let args: Vec<String> = argv.iter().map(|a| (*a).to_string()).collect();
            assert_eq!(
                dispatch(&args),
                0,
                "a hook must never block the agent: {argv:?}"
            );
        }
    }

    #[test]
    fn a_rejected_statusline_tee_still_exits_zero() {
        let args: Vec<String> = ["ctx", "usage", "tee", "--bogus"]
            .iter()
            .map(|a| (*a).to_string())
            .collect();
        assert_eq!(dispatch(&args), 0, "a statusline must never fail loudly");
    }

    #[test]
    fn only_hooks_and_the_statusline_survive_a_parse_failure() {
        let argv =
            |parts: &[&str]| -> Vec<String> { parts.iter().map(|p| (*p).to_string()).collect() };
        assert_eq!(
            classify_parse_failure(&argv(&["ctx", "hook", "Stop"])),
            ParseFailure::Hook
        );
        assert_eq!(
            classify_parse_failure(&argv(&["ctx", "usage", "tee", "--bogus"])),
            ParseFailure::Statusline
        );
        assert_eq!(
            classify_parse_failure(&argv(&["ctx", "usage", "--bogus"])),
            ParseFailure::Reject,
            "only the tee has a statusline to keep alive"
        );
        assert_eq!(
            classify_parse_failure(&argv(&["ctx", "exec", "--bogus"])),
            ParseFailure::Reject
        );
        assert_eq!(
            classify_parse_failure(&argv(&["ctx"])),
            ParseFailure::Reject
        );
    }

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
}
