use serde::Deserialize;

use super::os::OperatingSystem;

#[derive(Debug, Deserialize)]
pub struct Script {
    /// A descriptive name for the script.
    pub name: String,
    // A description of what the script does.
    pub description: Option<String>,
    /// Optional list of expected parameter names (in order).
    pub params: Option<Vec<String>>,
    /// Optional list of secret definitions.
    pub secrets: Option<Vec<SecretDefinition>>,
    /// A list of commands to execute.
    pub commands: Vec<CommandItem>,
}

/// Represents a secret definition in the script.
#[derive(Debug, Deserialize, Clone)]
pub struct SecretDefinition {
    /// The placeholder name to be substituted (e.g. "commit_password").
    pub name: String,
    /// The environment variable name where the secret value is stored (e.g. "COMMIT_PASSWORD").
    pub env_var: String,
}

/// Represents a single command in the YAML script.
#[derive(Debug, Deserialize, Clone)]
pub struct CommandItem {
    /// The shell command to execute.
    pub command: String,
    /// An optional description of what the command does.
    pub description: Option<String>,
    /// Optional options that control the behavior of the command.
    pub options: Option<CommandOptions>,
}

/// A set of options that control how a command is executed.
#[derive(Debug, Deserialize, Clone)]
pub struct CommandOptions {
    /// If true, the script continues even if this command fails.
    #[serde(default)]
    pub proceed_on_failure: bool,
    /// Optional delay in milliseconds after executing this command.
    #[serde(default)]
    pub delay_ms: Option<u64>,
    /// If true, the command is executed in interactive mode.
    #[serde(default)]
    pub interactive: bool,
    /// If provided, the command is only executed on the specified operating system
    /// (e.g. "linux", "windows", "macos").
    pub operating_system: Option<OperatingSystem>,
}
