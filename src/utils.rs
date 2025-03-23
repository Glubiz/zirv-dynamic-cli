use crate::{shortcuts::Shortcuts, OperatingSystem};
use std::{collections::HashMap, env, path::PathBuf};

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

/// Attempts to find a script file for the given command name in the .zirv directory.
/// 1. First, it checks for a file named "{command}.{ext}" in the local .zirv folder.
/// 2. Next, it looks in the shortcuts file (.zirv/.shortcuts.yaml) for a mapping.
/// 3. Finally, it falls back to the user's home directory, using HOME (or USERPROFILE) to locate ~/.zirv.
pub fn find_script_file(base_name: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let base_dir = PathBuf::from(".zirv");
    let extensions = ["yaml", "yml", "json", "toml"];

    // 1. Look in the local .zirv folder.
    for ext in &extensions {
        let path = base_dir.join(format!("{}.{}", base_name, ext));
        if path.exists() {
            return Ok(path.canonicalize()?);
        }
    }

    // 2. Look in the shortcuts file.
    let shortcuts_path = base_dir.join(".shortcuts.yaml");
    if shortcuts_path.exists() {
        let content = std::fs::read_to_string(&shortcuts_path)?;
        let shortcuts: Shortcuts = serde_yaml::from_str(&content)?;
        if let Some(mapped_file) = shortcuts.shortcuts.get(base_name) {
            let path = base_dir.join(mapped_file);
            if path.exists() {
                return Ok(path.canonicalize()?);
            }
            for ext in &extensions {
                let path = base_dir.join(format!("{}.{}", mapped_file, ext));
                if path.exists() {
                    return Ok(path.canonicalize()?);
                }
            }
        }
    }

    // 3. Fallback: use HOME or USERPROFILE.
    let home_dir = PathBuf::from(env::var("HOME").or_else(|_| env::var("USERPROFILE"))?);

    let root = home_dir.join(".zirv");
    for ext in &extensions {
        let path = root.join(format!("{}.{}", base_name, ext));
        if path.exists() {
            return Ok(path.canonicalize()?);
        }
    }

    let shortcuts_path = root.join(".shortcuts.yaml");
    if shortcuts_path.exists() {
        let content = std::fs::read_to_string(&shortcuts_path)?;
        let shortcuts: Shortcuts = serde_yaml::from_str(&content)?;
        if let Some(mapped_file) = shortcuts.shortcuts.get(base_name) {
            let path = root.join(mapped_file);
            if path.exists() {
                return Ok(path.canonicalize()?);
            }
            for ext in &extensions {
                let path = root.join(format!("{}.{}", mapped_file, ext));
                if path.exists() {
                    return Ok(path.canonicalize()?);
                }
            }
        }
    }

    Err(format!("No script or shortcut found for '{}'", base_name).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::env;
    use std::fs::{create_dir_all, remove_dir_all, write};
    use std::path::PathBuf;
    use tempfile::tempdir;

    // --- Tests for operating_system ---

    #[test]
    fn test_operating_system() {
        assert_eq!(operating_system(OperatingSystem::Linux), "linux");
        assert_eq!(operating_system(OperatingSystem::Windows), "windows");
        assert_eq!(operating_system(OperatingSystem::MacOS), "macos");
    }

    // --- Tests for substitute_params ---

    #[test]
    fn test_substitute_params_single() {
        let mut params = HashMap::new();
        params.insert("key".to_string(), "value".to_string());
        let input = "test ${key}";
        let output = substitute_params(input, &params);

        assert_eq!(output, "test value");
    }

    #[test]
    fn test_substitute_params_multiple() {
        let mut params = HashMap::new();
        params.insert("a".to_string(), "one".to_string());
        params.insert("b".to_string(), "two".to_string());
        let input = "${a} and ${b}";
        let output = substitute_params(input, &params);

        assert_eq!(output, "one and two");
    }

    #[test]
    fn test_substitute_params_no_match() {
        let params = HashMap::new();
        let input = "no placeholders";
        let output = substitute_params(input, &params);
        assert_eq!(output, "no placeholders");
    }

    // --- Tests for find_script_file ---

    // Helper: create a file with given content.
    fn create_file(path: &PathBuf, content: &str) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(parent) = path.parent() {
            create_dir_all(parent)?;
        }
        write(path, content)?;

        Ok(())
    }

    #[test]
    fn test_local_file_found() -> Result<(), Box<dyn std::error::Error>> {
        // Create a temporary directory to simulate a project.
        let temp_dir = tempdir()?;
        let temp_path = temp_dir.path();

        // Create a local .zirv folder.
        let zirv_dir = temp_path.join(".zirv");
        create_dir_all(&zirv_dir)?;

        // Create file: .zirv/test.yaml.
        let file_path = zirv_dir.join("test.yaml");
        create_file(&file_path, "name: \"Test File\"")?;

        // Change current directory.
        let original_dir = env::current_dir()?;
        env::set_current_dir(temp_path)?;

        let found = find_script_file("test")?;
        assert_eq!(found, file_path.canonicalize()?);

        env::set_current_dir(original_dir)?;

        Ok(())
    }

    #[test]
    fn test_shortcut_mapping_with_extension() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let temp_path = temp_dir.path();

        let zirv_dir = temp_path.join(".zirv");
        create_dir_all(&zirv_dir)?;

        // Create .zirv/.shortcuts.yaml mapping "test" -> "example.yaml".
        let shortcuts_path = zirv_dir.join(".shortcuts.yaml");
        let shortcuts_content = r#"
shortcuts:
  test: "example.yaml"
"#;
        create_file(&shortcuts_path, shortcuts_content)?;

        // Create mapped file: .zirv/example.yaml.
        let mapped_file = zirv_dir.join("example.yaml");
        create_file(&mapped_file, "name: \"Example File\"")?;

        let original_dir = env::current_dir()?;
        env::set_current_dir(temp_path)?;

        let found = find_script_file("test")?;
        assert_eq!(found, mapped_file.canonicalize()?);

        env::set_current_dir(original_dir)?;

        Ok(())
    }

    #[test]
    fn test_shortcut_mapping_without_extension() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let temp_path = temp_dir.path();

        let zirv_dir = temp_path.join(".zirv");
        create_dir_all(&zirv_dir)?;

        // Create .zirv/.shortcuts.yaml mapping "test" -> "example" (no extension).
        let shortcuts_path = zirv_dir.join(".shortcuts.yaml");
        let shortcuts_content = r#"
shortcuts:
  test: "example"
"#;
        create_file(&shortcuts_path, shortcuts_content)?;

        // Create file: .zirv/example.json.
        let mapped_file = zirv_dir.join("example.json");
        create_file(&mapped_file, "{\"name\": \"Example JSON\"}")?;

        let original_dir = env::current_dir()?;
        env::set_current_dir(temp_path)?;

        let found = find_script_file("test")?;
        assert_eq!(found, mapped_file.canonicalize()?);

        env::set_current_dir(original_dir)?;

        Ok(())
    }

    #[test]
    fn test_fallback_home() -> Result<(), Box<dyn std::error::Error>> {
        // Simulate a fake home directory.
        let fake_home_dir = tempdir()?;
        let fake_home_path = fake_home_dir.path().join(".zirv");
        create_dir_all(&fake_home_path)?;

        // Create file in fake home: test.toml.
        let home_file = fake_home_path.join("test.toml");
        create_file(&home_file, "name = \"Home File\"")?;

        // Create a temporary directory with no local .zirv.
        let temp_dir = tempdir()?;
        let temp_path = temp_dir.path();
        let local_zirv = temp_path.join(".zirv");
        if local_zirv.exists() {
            remove_dir_all(&local_zirv)?;
        }

        // Override HOME/USERPROFILE.
        let original_dir = env::current_dir()?;
        let original_home = env::var("HOME")
            .ok()
            .or_else(|| env::var("USERPROFILE").ok());

        env::set_var("HOME", fake_home_dir.path());
        env::set_var("USERPROFILE", fake_home_dir.path());
        env::set_current_dir(temp_path)?;

        let found = find_script_file("test")?;

        assert_eq!(found, home_file.canonicalize()?);

        env::set_current_dir(original_dir)?;

        if let Some(home) = original_home {
            env::set_var("HOME", home.clone());
            env::set_var("USERPROFILE", home);
        } else {
            env::remove_var("HOME");
            env::remove_var("USERPROFILE");
        }

        Ok(())
    }

    #[test]
    fn test_no_script_found() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let temp_path = temp_dir.path();

        // Ensure no .zirv folder exists.
        let local_zirv = temp_path.join(".zirv");
        if local_zirv.exists() {
            remove_dir_all(&local_zirv)?;
        }

        // Set HOME to a directory with no .zirv.
        let fake_home = tempdir()?;
        let original_dir = env::current_dir()?;
        let original_home = env::var("HOME")
            .ok()
            .or_else(|| env::var("USERPROFILE").ok());

        env::set_var("HOME", fake_home.path());
        env::set_var("USERPROFILE", fake_home.path());
        env::set_current_dir(temp_path)?;

        let res = find_script_file("nosuchfile");
        assert!(res.is_err(), "Expected error for missing script");

        env::set_current_dir(original_dir)?;

        if let Some(home) = original_home {
            env::set_var("HOME", home.clone());
            env::set_var("USERPROFILE", home);
        } else {
            env::remove_var("HOME");
            env::remove_var("USERPROFILE");
        }

        Ok(())
    }
}
