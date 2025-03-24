use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "lowercase")]
pub enum OperatingSystem {
    #[serde(rename = "linux")]
    Linux,
    #[serde(rename = "windows")]
    Windows,
    #[serde(rename = "macos")]
    MacOS,
}

pub fn operating_system(op: OperatingSystem) -> String {
    match op {
        OperatingSystem::Linux => "linux".to_string(),
        OperatingSystem::Windows => "windows".to_string(),
        OperatingSystem::MacOS => "macos".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_operating_system() {
        assert_eq!(operating_system(OperatingSystem::Linux), "linux");
        assert_eq!(operating_system(OperatingSystem::Windows), "windows");
        assert_eq!(operating_system(OperatingSystem::MacOS), "macos");
    }
}
