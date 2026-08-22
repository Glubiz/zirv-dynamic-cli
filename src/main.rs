use std::io::IsTerminal;
use std::path::Path;

use clap::Parser;
use commands::ctx;
use commands::workflow;
use commands::{
    create::{CreateOptions, create_script},
    help::show_help,
    init::init_zirv,
    version::get_version,
};

mod commands;
mod input;
mod output;
mod script_runner;
mod settings;
mod utils;

use input::Input;
use script_runner::execute;
use utils::file_to_script;

/// True when the top-level invocation is exactly `zirv --help`/`zirv -h`,
/// i.e. the help flag stands in for the command itself rather than being a
/// script's own parameter. Checked against raw argv, before clap parses
/// anything, so clap's auto-generated help for the `Input` struct never
/// fires and `zirv help`'s rich script listing is shown instead.
fn is_top_level_help(argv: &[String]) -> bool {
    matches!(argv.get(1).map(String::as_str), Some("--help") | Some("-h"))
}

/// True when argv[1] names the `ctx` built-in, compared **case-insensitively**
/// to match `utils::is_reserved_command`/`RESERVED_COMMANDS`: on NTFS/APFS a
/// script file `Ctx.yaml` resolves the same as `ctx.yaml`, so a case-sensitive
/// interception would let `zirv Ctx` fall through to script lookup and run a
/// repo `.zirv/Ctx.yaml` that `zirv help` simultaneously reports as shadowed
/// and unreachable. `dispatch` skips argv[1] itself, so the exact casing the
/// user typed never reaches the verb tree.
fn is_top_level_ctx(argv: &[String]) -> bool {
    argv.get(1).is_some_and(|s| s.eq_ignore_ascii_case("ctx"))
}

/// Workflow commands have their own clap tree and must be intercepted before
/// the legacy script runner resolves a same-named file under `.zirv/`.
fn is_top_level_workflow_command(argv: &[String]) -> bool {
    argv.get(1).is_some_and(|name| {
        workflow::TOP_LEVEL_COMMANDS
            .iter()
            .any(|command| name.eq_ignore_ascii_case(command))
    })
}

/// True when argv[1] names the `memory` built-in, compared case-insensitively
/// like the `ctx` check above, so a repo `.zirv/Memory.yaml` (or any
/// differently-cased script) can never shadow it. Unlike `chat`/`agent`
/// below, `memory` is not a 1:1 alias into a single `ctx` verb: it has its
/// own verb tree (`status`/`list`/`recall`/`remember`/`forget`/`verify`),
/// dispatched by `commands::ctx::memory_cli::dispatch` directly rather than
/// through `ctx::dispatch`.
fn is_top_level_memory(argv: &[String]) -> bool {
    argv.get(1)
        .is_some_and(|s| s.eq_ignore_ascii_case("memory"))
}

/// `zirv chat` and `zirv agent` are top-level aliases for `zirv ctx chat`
/// and `zirv ctx agent`, checked against raw argv (like the `ctx`
/// interception above) so they run before clap ever sees `Input`. Returns
/// the ctx verb name to route to, or `None` when argv doesn't name one of
/// these aliases in the command slot. Matched **case-insensitively**, the
/// same rule `is_top_level_ctx`/`is_reserved_command` use, so `zirv Chat`
/// cannot slip past into a repo `Chat.yaml` script.
fn top_level_ctx_alias(argv: &[String]) -> Option<&'static str> {
    match argv.get(1).map(String::as_str) {
        Some(s) if s.eq_ignore_ascii_case("chat") => Some("chat"),
        Some(s) if s.eq_ignore_ascii_case("agent") => Some("agent"),
        _ => None,
    }
}

/// Rewrites argv for a top-level alias into the shape `ctx::dispatch`
/// expects: `args[0]` literally `"ctx"` (matching how the raw `ctx`
/// interception already calls it), the resolved verb, then whatever
/// followed the alias on the command line. Reusing `ctx::dispatch` this way
/// means the alias gets the full ctx verb tree for free: subcommand
/// parsing, `--help` exiting 0, and the same parse-failure classification.
fn rewrite_ctx_alias_args(verb: &str, argv: &[String]) -> Vec<String> {
    let mut args = vec!["ctx".to_string(), verb.to_string()];
    args.extend(argv.iter().skip(2).cloned());
    args
}

/// What a bare `zirv` invocation (no arguments at all) does: open an
/// interactive chat when this looks like a zirv-managed repo and *both*
/// stdin and stdout are a real terminal, or fall back to the ordinary help
/// listing otherwise. A pure function of those three facts so the policy is
/// testable without a pty or a real filesystem/stdin/stdout check --
/// `zirv_dir_present` and `std::io::IsTerminal` supply them at the call site.
///
/// Both stdin *and* stdout have to be a terminal, not just stdin: `zirv |
/// less` (or any other redirect of stdout alone) has an interactive stdin
/// but would otherwise launch a chat session into a pipe nothing is reading
/// interactively.
///
/// This is a deliberate behavior change from clap's own bare-invocation
/// handling: `Input::command` is a required positional, so before this a
/// bare `zirv` was a clap usage error exiting 2. `BareTarget::Help` instead
/// exits 0, matching top-level `zirv --help`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BareTarget {
    Chat,
    Help,
}

fn bare_invocation_target(
    zirv_dir_exists: bool,
    stdin_is_tty: bool,
    stdout_is_tty: bool,
) -> BareTarget {
    if zirv_dir_exists && stdin_is_tty && stdout_is_tty {
        BareTarget::Chat
    } else {
        BareTarget::Help
    }
}

/// True when `./.zirv` (relative to `cwd`) exists as a directory. **Local
/// only**, deliberately: a global `~/.zirv` says something about the
/// operator's machine, not about the directory zirv happens to be run from,
/// and treating it as "this is a zirv-managed repo" would open a chat
/// session for a bare `zirv` in literally any directory for anyone who has
/// ever run `zirv create --global` once. Matches the approved design's own
/// wording -- "in a repo with a `.zirv` directory" -- and mirrors
/// `SCRIPT_DIR_NAME` resolution order elsewhere: local is checked, global is
/// not consulted for this decision. `zirv chat`/`zirv ctx chat` remain
/// unaffected and still work anywhere, with or without a local `.zirv`.
fn zirv_dir_present(cwd: &Path) -> bool {
    cwd.join(utils::SCRIPT_DIR_NAME).is_dir()
}

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if is_top_level_ctx(&argv) {
        std::process::exit(ctx::dispatch(&argv[1..]));
    }

    if is_top_level_workflow_command(&argv) {
        std::process::exit(workflow::dispatch(&argv));
    }

    if is_top_level_memory(&argv) {
        std::process::exit(ctx::memory_cli::dispatch(&argv[1..]));
    }

    if let Some(verb) = top_level_ctx_alias(&argv) {
        std::process::exit(ctx::dispatch(&rewrite_ctx_alias_args(verb, &argv)));
    }

    if argv.len() == 1 {
        let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
        let zirv_exists = zirv_dir_present(&cwd);
        let stdin_is_tty = std::io::stdin().is_terminal();
        let stdout_is_tty = std::io::stdout().is_terminal();
        match bare_invocation_target(zirv_exists, stdin_is_tty, stdout_is_tty) {
            BareTarget::Chat => {
                std::process::exit(ctx::dispatch(&["ctx".to_string(), "chat".to_string()]));
            }
            BareTarget::Help => {
                if let Err(e) = show_help(&mut std::io::stdout()) {
                    output::error(e);
                    std::process::exit(1);
                }
                return;
            }
        }
    }

    if is_top_level_help(&argv) {
        if let Err(e) = show_help(&mut std::io::stdout()) {
            output::error(e);
            std::process::exit(1);
        }
        return;
    }

    let input = Input::parse();

    // These live on the shared `Input` struct, so clap accepts them for every
    // command and quietly eats the argument after them: `zirv deploy --name
    // staging` handed `staging` to `--name` and then complained the script got
    // no parameters. Refuse instead of swallowing.
    if !matches!(input.command.as_str(), "create" | "c")
        && let Some(flag) = input.misplaced_create_flag()
    {
        output::error(format!(
            "{flag} belongs to `zirv create`; '{}' does not take it",
            input.command
        ));
        std::process::exit(1);
    }

    match input.command.as_str() {
        "help" | "h" => {
            if let Err(e) = show_help(&mut std::io::stdout()) {
                output::error(e);
                std::process::exit(1);
            }
            return;
        }
        "version" | "v" => {
            if let Err(e) = get_version(&mut std::io::stdout()) {
                output::error(e);
                std::process::exit(1);
            }
            return;
        }
        "init" | "i" => {
            if let Err(e) = init_zirv() {
                output::error(e);
                std::process::exit(1);
            }
            return;
        }
        "create" | "c" => {
            let opts = CreateOptions {
                name: input.name.clone(),
                shortcut: input.shortcut.clone(),
                global: input.global,
            };
            if let Err(e) = create_script(opts) {
                output::error(e);
                std::process::exit(1);
            }
            return;
        }
        _ => {}
    }

    // A command that case-folds to a reserved built-in but did not match one of
    // the exact-case arms above (e.g. `zirv Help`, `zirv CREATE`) must never
    // fall through to script lookup: on NTFS/APFS a repo `.zirv/Help.yaml`
    // resolves the same as `help.yaml` and would otherwise execute under a
    // reserved name that `zirv help` reports as shadowed and unreachable. The
    // pre-clap `ctx`/`chat`/`agent` interceptions already fold case; this closes
    // the same gap for the clap-dispatched built-ins.
    if utils::is_reserved_command(&input.command) {
        output::error(format!(
            "'{}' is a reserved command name and cannot be run as a script; \
             use the lowercase built-in",
            input.command
        ));
        std::process::exit(1);
    }

    let file_path = match input.get_file_path() {
        Ok(p) => p,
        Err(e) => {
            output::error(e);
            std::process::exit(1);
        }
    };

    let script = match file_to_script(&file_path) {
        Ok(s) => s,
        Err(e) => {
            output::error(e);
            std::process::exit(1);
        }
    };

    if let Err(e) = execute(&script, &input.params, input.dry_run).await {
        output::error(&e);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_is_top_level_help_matches_long_flag() {
        assert!(is_top_level_help(&argv(&["zirv", "--help"])));
    }

    #[test]
    fn test_is_top_level_help_matches_short_flag() {
        assert!(is_top_level_help(&argv(&["zirv", "-h"])));
    }

    #[test]
    fn test_is_top_level_help_ignores_script_commands() {
        assert!(!is_top_level_help(&argv(&["zirv", "build"])));
        assert!(!is_top_level_help(&argv(&["zirv", "help"])));
        assert!(!is_top_level_help(&argv(&["zirv"])));
    }

    #[test]
    fn test_is_top_level_help_does_not_match_flag_as_script_param() {
        // `--help` passed as a parameter to a script command (not in the
        // command slot itself) is not top-level help.
        assert!(!is_top_level_help(&argv(&["zirv", "build", "--help"])));
    }

    /// FINDING 2: the pre-clap `ctx`/`chat`/`agent` interceptions are matched
    /// case-insensitively (like `utils::is_reserved_command`), so a mis-cased
    /// invocation is routed to the built-in and can never fall through to a
    /// same-named repo script (`.zirv/Chat.yaml`, `Ctx.yaml`, ...).
    #[test]
    fn reserved_verbs_are_intercepted_case_insensitively() {
        assert!(is_top_level_ctx(&argv(&["zirv", "ctx", "status"])));
        assert!(is_top_level_ctx(&argv(&["zirv", "CTX", "status"])));
        assert!(is_top_level_ctx(&argv(&["zirv", "Ctx"])));
        assert!(!is_top_level_ctx(&argv(&["zirv", "context"])));
        assert!(!is_top_level_ctx(&argv(&["zirv"])));

        assert_eq!(top_level_ctx_alias(&argv(&["zirv", "chat"])), Some("chat"));
        assert_eq!(top_level_ctx_alias(&argv(&["zirv", "Chat"])), Some("chat"));
        assert_eq!(top_level_ctx_alias(&argv(&["zirv", "CHAT"])), Some("chat"));
        assert_eq!(
            top_level_ctx_alias(&argv(&["zirv", "Agent"])),
            Some("agent")
        );
        assert_eq!(top_level_ctx_alias(&argv(&["zirv", "build"])), None);

        assert!(is_top_level_workflow_command(&argv(&["zirv", "skill"])));
        assert!(is_top_level_workflow_command(&argv(&["zirv", "SKILL"])));
        assert!(is_top_level_workflow_command(&argv(&["zirv", "frontend"])));
        assert!(is_top_level_workflow_command(&argv(&["zirv", "FRONTEND"])));
        assert!(!is_top_level_workflow_command(&argv(&["zirv", "build"])));
    }

    /// FINDING 2: every reserved command name -- whatever its casing -- is
    /// recognised by the guard that gates script dispatch, so `zirv Help`,
    /// `zirv CREATE`, `zirv Chat` are all refused as scripts rather than run a
    /// repo file that case-folds to a built-in.
    #[test]
    fn mis_cased_reserved_command_names_are_recognised_by_the_guard() {
        for name in [
            "Help", "HELP", "Version", "CREATE", "Init", "Ctx", "Chat", "Agent",
        ] {
            assert!(
                utils::is_reserved_command(name),
                "'{name}' must be recognised as reserved so it can never run as a script"
            );
        }
        assert!(!utils::is_reserved_command("build"));
        assert!(!utils::is_reserved_command("deploy"));
    }

    #[test]
    fn a_bare_invocation_in_a_repo_with_a_zirv_directory_starts_chat() {
        let cwd = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(cwd.path().join(".zirv")).expect("mkdir");

        assert!(zirv_dir_present(cwd.path()));
        assert_eq!(bare_invocation_target(true, true, true), BareTarget::Chat);
    }

    /// S5: a *global* `~/.zirv` alone must not turn a bare `zirv` into a chat
    /// launch in an arbitrary directory -- only a local `./.zirv` counts.
    #[test]
    fn a_bare_invocation_with_only_a_global_zirv_directory_shows_help() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");

        assert!(
            !zirv_dir_present(cwd.path()),
            "a global .zirv is not a local one"
        );
        assert_eq!(bare_invocation_target(false, true, true), BareTarget::Help);
    }

    #[test]
    fn a_bare_invocation_with_no_zirv_directory_anywhere_shows_help() {
        let cwd = tempfile::tempdir().expect("tempdir");

        assert!(!zirv_dir_present(cwd.path()));
        assert_eq!(bare_invocation_target(false, true, true), BareTarget::Help);
    }

    #[test]
    fn a_bare_invocation_with_piped_stdin_shows_help_instead_of_opening_a_chat() {
        // A `.zirv` directory exists, but stdin is not a real terminal (e.g.
        // `echo hi | zirv`): help wins, so a bare invocation piped into a
        // script never blocks on an interactive chat session.
        assert_eq!(bare_invocation_target(true, false, true), BareTarget::Help);
    }

    /// S5: stdin alone being a terminal is not enough -- `zirv | less` has an
    /// interactive stdin but a redirected stdout, and must not launch a chat
    /// session into the pipe on the other end.
    #[test]
    fn a_bare_invocation_with_piped_stdout_shows_help_instead_of_opening_a_chat_into_the_pipe() {
        assert_eq!(bare_invocation_target(true, true, false), BareTarget::Help);
    }

    #[test]
    fn an_explicit_chat_command_is_not_subject_to_the_tty_rule() {
        // `zirv chat` is routed by `top_level_ctx_alias`, an entirely
        // different code path from the bare-invocation TTY check: it takes
        // no stdin-is-a-terminal parameter at all, so a piped `zirv chat`
        // still launches chat (and lets `wrap`/the adapter itself decide
        // what to do with a non-interactive terminal).
        assert_eq!(top_level_ctx_alias(&argv(&["zirv", "chat"])), Some("chat"));
        assert_eq!(
            rewrite_ctx_alias_args("chat", &argv(&["zirv", "chat"])),
            vec!["ctx".to_string(), "chat".to_string()]
        );
        assert_eq!(
            rewrite_ctx_alias_args("agent", &argv(&["zirv", "agent", "claude", "go"])),
            vec![
                "ctx".to_string(),
                "agent".to_string(),
                "claude".to_string(),
                "go".to_string()
            ]
        );
    }

    #[test]
    fn the_help_flag_still_wins_over_the_bare_alias() {
        // `zirv --help` has argv len 2, not 1, so it never reaches the bare-
        // invocation check at all, and it is not a `chat`/`agent` alias
        // either.
        assert!(is_top_level_help(&argv(&["zirv", "--help"])));
        assert_eq!(top_level_ctx_alias(&argv(&["zirv", "--help"])), None);
    }

    #[test]
    fn chat_and_agent_are_reserved_so_a_script_can_never_shadow_them() {
        assert!(utils::is_reserved_command("chat"));
        assert!(utils::is_reserved_command("agent"));
    }

    #[test]
    fn memory_is_intercepted_case_insensitively_and_reserved() {
        assert!(is_top_level_memory(&argv(&["zirv", "memory", "status"])));
        assert!(is_top_level_memory(&argv(&["zirv", "Memory"])));
        assert!(is_top_level_memory(&argv(&["zirv", "MEMORY", "list"])));
        assert!(!is_top_level_memory(&argv(&["zirv", "memories"])));
        assert!(!is_top_level_memory(&argv(&["zirv"])));
        assert!(utils::is_reserved_command("memory"));
        assert!(utils::is_reserved_command("Memory"));
    }

    /// A repo `.zirv/Memory.yaml` (any casing) must never be reachable as a
    /// script -- `memory` is intercepted against raw argv before clap runs,
    /// the same guarantee `ctx_is_intercepted_before_script_lookup` pins for
    /// `ctx`.
    #[test]
    fn memory_is_intercepted_before_script_lookup() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            dir.path().join(".zirv/Memory.yaml"),
            "commands:\n  - command: echo not-a-built-in\n",
        )
        .expect("write");

        let exe = std::env::current_exe().expect("current_exe");
        let bin = exe
            .parent()
            .and_then(|p| p.parent())
            .expect("target/debug")
            .join("zirv");

        let out = std::process::Command::new(&bin)
            .args(["memory", "--help"])
            .current_dir(dir.path())
            .output()
            .expect("run zirv");

        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains("status") && text.contains("remember"),
            "built-in memory help expected, got: {text}"
        );
    }
}
