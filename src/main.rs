use clap::Parser;
use serde::Deserialize;

mod shortcuts;
mod utils;
mod yaml;

use utils::find_script_file;
use yaml::run as run_yaml;

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
    /// A list of commands to execute.
    commands: Vec<CommandItem>,
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

    let file_path = find_script_file(&cli.name)?;
    
    run_yaml(&file_path, &cli.params).await
}