use clap::Parser;
use commands::{
    create::create_script_interactive, help::show_help, init::init_zirv, version::get_version,
};

mod commands;
mod input;
mod output;
mod script_runner;
mod utils;

use input::Input;
use script_runner::execute;
use utils::file_to_script;

#[tokio::main]
async fn main() {
    let input = Input::parse();

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
            if let Err(e) = create_script_interactive() {
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
