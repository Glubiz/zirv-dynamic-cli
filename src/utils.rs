use crate::OperatingSystem;

pub fn operating_system(op: OperatingSystem) -> String {
    match op {
        OperatingSystem::Linux => "linux".to_string(),
        OperatingSystem::Windows => "windows".to_string(),
        OperatingSystem::MacOS => "macos".to_string(),
    }
}