use clap::Parser;
use commands::{help::show_help, init::init_zirv, version::get_version};

mod commands;
mod run;
mod structs;
mod utils;

use run::run as run_yaml;
use structs::file::File;
use utils::find_script_file;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Parse CLI arguments.
    let cli = File::parse();

    // Check for built-in commands before attempting to find a script file.
    if cli.name == "help" || cli.name == "h" {
        show_help(&mut std::io::stdout())?;
        return Ok(());
    } else if cli.name == "version" || cli.name == "v" {
        get_version(&mut std::io::stdout())?;
        return Ok(());
    } else if cli.name == "init" || cli.name == "i" {
        init_zirv()?;
        return Ok(());
    }

    // For all other commands, attempt to find a script file.
    match find_script_file(&cli.name) {
        Ok(path) => run_yaml(&path, &cli.params).await,
        Err(e) => {
            eprintln!("Error: {}", e);
            Ok(())
        }
    }
}
