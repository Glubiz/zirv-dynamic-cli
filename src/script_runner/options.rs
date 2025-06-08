use serde::Deserialize;

use super::operating_system::OperatingSystem;

/// A set of options that control how a command is executed.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct Options {
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
