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

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("ctx") {
        std::process::exit(ctx::dispatch(&argv[1..]));
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
}
