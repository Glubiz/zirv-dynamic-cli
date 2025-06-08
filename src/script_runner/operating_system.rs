use serde::Deserialize;

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename = "os")]
pub enum OperatingSystem {
    #[serde(rename = "linux")]
    Linux,
    #[serde(rename = "windows")]
    Windows,
    #[serde(rename = "macos")]
    MacOS,
}

impl OperatingSystem {
    /// Returns the current operating system as an `OperatingSystem` enum.
    pub fn current() -> Self {
        match std::env::consts::OS {
            "linux" => OperatingSystem::Linux,
            "windows" => OperatingSystem::Windows,
            "macos" => OperatingSystem::MacOS,
            _ => panic!("Unsupported operating system"),
        }
    }

    pub fn is_current(&self) -> bool {
        *self == Self::current()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_operating_system_current() {
        let current_os = OperatingSystem::current();
        match std::env::consts::OS {
            "linux" => assert_eq!(current_os, OperatingSystem::Linux),
            "windows" => assert_eq!(current_os, OperatingSystem::Windows),
            "macos" => assert_eq!(current_os, OperatingSystem::MacOS),
            _ => panic!("Unsupported operating system"),
        }
    }
}
