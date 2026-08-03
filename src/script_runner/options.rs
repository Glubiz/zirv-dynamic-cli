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
    /// (e.g. "linux", "windows", "macos"). Accepts the legacy `os` key too, since
    /// the README documented that name before it was corrected.
    #[serde(alias = "os")]
    pub operating_system: Option<OperatingSystem>,
    /// Optional commands to be executed if the command fails.
    pub fallback: Option<Vec<FallbackCommand>>,
}

impl Options {
    /// True when an `operating_system` filter is set and does not match the
    /// current platform. Shared by ordinary commands and agent steps so both
    /// skip the same way.
    pub fn skip_for_os(&self) -> bool {
        self.operating_system
            .as_ref()
            .is_some_and(|os| !os.is_current())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The struct field (and thus the YAML/JSON/TOML key) is
    /// `operating_system`. The `#[serde(rename = "os")]` on the
    /// `OperatingSystem` enum only affects how that type names itself, not
    /// the key `Options` exposes it under. Guards the README's schema
    /// examples against drifting back to the wrong key.
    #[test]
    fn test_options_operating_system_key_is_the_field_name() {
        let opts: Options = serde_yaml_ng::from_str("operating_system: linux\n").unwrap();
        assert_eq!(opts.operating_system, Some(OperatingSystem::Linux));
    }

    /// `os` used to be silently ignored as an unknown key (a script written
    /// with `os: linux`, as the older docs suggested, parsed "successfully"
    /// but never applied the filter). It is now an explicit alias, so scripts
    /// written against the old docs behave as their authors intended instead
    /// of silently running on every platform.
    #[test]
    fn test_options_os_key_is_an_alias_for_operating_system() {
        let opts: Options = serde_yaml_ng::from_str("os: linux\n").unwrap();
        assert_eq!(opts.operating_system, Some(OperatingSystem::Linux));
    }
}
