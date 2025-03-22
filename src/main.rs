use std::io::Cursor;

use clap::Parser;
use help::show_help;
use serde::Deserialize;

mod help;
mod run;
mod shortcuts;
mod utils;

use run::run as run_yaml;
use utils::find_script_file;

/// Represents a YAML script.
#[derive(Debug, Deserialize, Parser)]
struct File {
    /// A descriptive name for the script.
    name: String,
    /// Optional parameters (positional arguments) that will be mapped to the script's expected params.
    #[arg(num_args = 0..)]
    params: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Script {
    /// A descriptive name for the script.
    name: String,
    // A description of what the script does.
    description: Option<String>,
    /// Optional list of expected parameter names (in order).
    params: Option<Vec<String>>,
    /// Optional list of secret definitions.
    secrets: Option<Vec<SecretDefinition>>,
    /// A list of commands to execute.
    commands: Vec<CommandItem>,
}

/// Represents a secret definition in the script.
#[derive(Debug, Deserialize, Clone)]
struct SecretDefinition {
    /// The placeholder name to be substituted (e.g. "commit_password").
    name: String,
    /// The environment variable name where the secret value is stored (e.g. "COMMIT_PASSWORD").
    env_var: String,
}

/// Represents a single command in the YAML script.
#[derive(Debug, Deserialize, Clone)]
struct CommandItem {
    /// The shell command to execute.
    command: String,
    /// An optional description of what the command does.
    description: Option<String>,
    /// Optional options that control the behavior of the command.
    options: Option<CommandOptions>,
}

/// A set of options that control how a command is executed.
#[derive(Debug, Deserialize, Clone)]
struct CommandOptions {
    /// If true, the script continues even if this command fails.
    #[serde(default)]
    proceed_on_failure: bool,
    /// Optional delay in milliseconds after executing this command.
    #[serde(default)]
    delay_ms: Option<u64>,
    /// If true, the command is executed in interactive mode.
    #[serde(default)]
    interactive: bool,
    /// If provided, the command is only executed on the specified operating system
    /// (e.g. "linux", "windows", "macos").
    operating_system: Option<OperatingSystem>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "lowercase")]
enum OperatingSystem {
    #[serde(rename = "linux")]
    Linux,
    #[serde(rename = "windows")]
    Windows,
    #[serde(rename = "macos")]
    MacOS,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Get the CLI arguments.
    let cli = File::parse();

    match find_script_file(&cli.name) {
        Ok(path) => run_yaml(&path, &cli.params).await,
        Err(e) => {
            if cli.name == "help" || cli.name == "h" {
                let mut buffer = Cursor::new(Vec::new());
                show_help(&mut buffer)?;

                return Ok(());
            }

            eprintln!("Error: {}", e);
            return Ok(());
        }
    }
}
