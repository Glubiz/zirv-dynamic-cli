use serde::{Deserialize, Serialize};

use crate::script_runner::fallback_command::FallbackCommand;

use super::operating_system::OperatingSystem;

/// A set of options that control how a command is executed.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
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
    /// Optional commands to be executed if the command fails.
    pub fallback: Option<Vec<FallbackCommand>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The struct field (and thus the YAML/JSON/TOML key) is
    /// `operating_system`, not `os`: the `#[serde(rename = "os")]` on the
    /// `OperatingSystem` enum only affects how that type names itself, not
    /// the key `Options` exposes it under. Guards the README's schema
    /// examples against drifting back to the wrong key.
    #[test]
    fn test_options_operating_system_key_is_the_field_name() {
        let opts: Options = serde_yaml_ng::from_str("operating_system: linux\n").unwrap();
        assert_eq!(opts.operating_system, Some(OperatingSystem::Linux));
    }

    /// `os` is not the field name, so it is silently ignored as an unknown
    /// key rather than raising an error: a script written with `os: linux`
    /// (as older docs suggested) parses "successfully" but the OS filter is
    /// never applied and the step runs on every platform.
    #[test]
    fn test_options_silently_ignores_os_as_an_unknown_key() {
        let opts: Options = serde_yaml_ng::from_str("os: linux\n").unwrap();
        assert_eq!(opts.operating_system, None);
    }
}
