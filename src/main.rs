use clap::Parser;
use commands::{
    create::create_script_interactive, help::show_help, init::init_zirv, version::get_version,
};

mod commands;
mod input;
mod script_runner;
mod utils;

use input::Input;
use script_runner::execute;
use utils::file_to_script;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Parse CLI arguments.
    let input = Input::parse();

    // Check for built-in commands before attempting to find a script file.
    if input.command == "help" || input.command == "h" {
        show_help(&mut std::io::stdout())?;
        return Ok(());
    } else if input.command == "version" || input.command == "v" {
        get_version(&mut std::io::stdout())?;
        return Ok(());
    } else if input.command == "init" || input.command == "i" {
        init_zirv()?;
        return Ok(());
    } else if input.command == "create" || input.command == "c" {
        create_script_interactive()?;
        return Ok(());
    }

    // For all other commands, attempt to find a script file.
    let file_path = input.get_file_path()?;

    let script = file_to_script(&file_path)?;

    match execute(&script, &input.params).await {
        Ok(_) => Ok(()),
        Err(e) => {
            eprintln!("Error executing script: {}", e);
            Err(e.into())
        }
    }
}
