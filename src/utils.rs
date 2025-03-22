use std::collections::HashMap;

use crate::OperatingSystem;

pub fn operating_system(op: OperatingSystem) -> String {
    match op {
        OperatingSystem::Linux => "linux".to_string(),
        OperatingSystem::Windows => "windows".to_string(),
        OperatingSystem::MacOS => "macos".to_string(),
    }
}

/// Substitutes parameter placeholders in the command string.
/// Placeholders are of the form `${param_name}`.
pub fn substitute_params(command: &str, params: &HashMap<String, String>) -> String {
    let mut result = command.to_string();
    for (key, value) in params {
        let placeholder = format!("${{{}}}", key);
        result = result.replace(&placeholder, value);
    }
    result
}