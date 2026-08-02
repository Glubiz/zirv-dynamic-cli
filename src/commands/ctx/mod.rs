use clap::{Parser, Subcommand};

pub mod adapters;
pub mod config;
pub mod event;
pub mod exec;
pub mod handoff;
pub mod hook;
pub mod log;
pub mod optimize;
pub mod pace;
pub mod prompt;
pub mod resume;
pub mod rot;
pub mod run_loop;
pub mod score;
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

    /// Points `HOME` at a test directory and puts the previous value back on
    /// drop. `HOME` is process-wide, so a test that leaks one naming a deleted
    /// temp dir breaks every later pty spawn in the same run: portable-pty
    /// starts its child in `$HOME` unless the caller sets a working directory,
    /// and the `chdir` fails before the program is ever reached.
    pub(crate) struct HomeGuard(Option<OsString>);

    impl HomeGuard {
        pub(crate) fn set(home: &Path) -> Self {
            let previous = std::env::var_os("HOME");
            // SAFETY: CI runs tests single-threaded.
            unsafe { std::env::set_var("HOME", home) };
            Self(previous)
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            // SAFETY: CI runs tests single-threaded.
            unsafe {
                match self.0.take() {
                    Some(previous) => std::env::set_var("HOME", previous),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }
}

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
    /// Report usage windows, or tee the statusline to record them.
    Usage(usage::UsageArgs),
    /// Analyse the configuration surfaces that steer every session.
    Optimize(optimize::OptimizeArgs),
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
