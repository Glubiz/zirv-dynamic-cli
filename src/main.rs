use std::io::IsTerminal;
use std::path::Path;

use clap::Parser;
use commands::ctx;
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

/// `zirv chat` and `zirv agent` are top-level aliases for `zirv ctx chat`
/// and `zirv ctx agent`, checked against raw argv (like the `ctx`
/// interception above) so they run before clap ever sees `Input`. Returns
/// the ctx verb name to route to, or `None` when argv doesn't name one of
/// these aliases in the command slot.
fn top_level_ctx_alias(argv: &[String]) -> Option<&'static str> {
    match argv.get(1).map(String::as_str) {
        Some("chat") => Some("chat"),
        Some("agent") => Some("agent"),
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
/// interactive chat when this looks like a zirv-managed repo and stdin is a
/// real terminal, or fall back to the ordinary help listing otherwise. A
/// pure function of those two facts so the policy is testable without a pty
/// or a real filesystem/stdin check -- `zirv_dir_present` and
/// `std::io::IsTerminal` supply them at the call site.
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

fn bare_invocation_target(zirv_dir_exists: bool, stdin_is_tty: bool) -> BareTarget {
    if zirv_dir_exists && stdin_is_tty {
        BareTarget::Chat
    } else {
        BareTarget::Help
    }
}

/// True when either `./.zirv` (relative to `cwd`) or `~/.zirv` (relative to
/// `home`, when known) exists as a directory. The signal a bare `zirv`
/// invocation uses to decide there is something worth chatting about here,
/// separated from `bare_invocation_target` so each half (the filesystem
/// check, the policy) is testable on its own.
fn zirv_dir_present(cwd: &Path, home: Option<&Path>) -> bool {
    cwd.join(utils::SCRIPT_DIR_NAME).is_dir()
        || home.is_some_and(|h| h.join(utils::SCRIPT_DIR_NAME).is_dir())
}

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("ctx") {
        std::process::exit(ctx::dispatch(&argv[1..]));
    }

    if let Some(verb) = top_level_ctx_alias(&argv) {
        std::process::exit(ctx::dispatch(&rewrite_ctx_alias_args(verb, &argv)));
    }

    if argv.len() == 1 {
        let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
        let home = utils::home_dir().ok();
        let zirv_exists = zirv_dir_present(&cwd, home.as_deref());
        let stdin_is_tty = std::io::stdin().is_terminal();
        match bare_invocation_target(zirv_exists, stdin_is_tty) {
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

    #[test]
    fn a_bare_invocation_in_a_repo_with_a_zirv_directory_starts_chat() {
        let cwd = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(cwd.path().join(".zirv")).expect("mkdir");

        assert!(zirv_dir_present(cwd.path(), None));
        assert_eq!(bare_invocation_target(true, true), BareTarget::Chat);
    }

    #[test]
    fn a_bare_invocation_with_a_global_zirv_directory_starts_chat() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");

        assert!(zirv_dir_present(cwd.path(), Some(home.path())));
        assert_eq!(bare_invocation_target(true, true), BareTarget::Chat);
    }

    #[test]
    fn a_bare_invocation_with_no_zirv_directory_anywhere_shows_help() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");

        assert!(!zirv_dir_present(cwd.path(), Some(home.path())));
        assert_eq!(bare_invocation_target(false, true), BareTarget::Help);
    }

    #[test]
    fn a_bare_invocation_with_piped_stdin_shows_help_instead_of_opening_a_chat() {
        // A `.zirv` directory exists, but stdin is not a real terminal (e.g.
        // `echo hi | zirv`): help wins, so a bare invocation piped into a
        // script never blocks on an interactive chat session.
        assert_eq!(bare_invocation_target(true, false), BareTarget::Help);
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
}
