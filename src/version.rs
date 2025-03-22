use std::io::Write;

use serde::Deserialize;

#[derive(Deserialize)]
struct CargoToml {
    package: Package,
}

#[derive(Deserialize)]
struct Package {
    version: String,
}

pub fn get_version<W: Write>(writer: &mut W) -> Result<(), Box<dyn std::error::Error>> {
    // Read version from Cargo.toml in the current directory.
    let content = std::fs::read_to_string("Cargo.toml")?;
    let toml: CargoToml = toml::from_str(&content)?;

    let version_line = toml.package.version;

    writeln!(writer, "Version: {}", version_line.trim())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs::write;
    use std::io::Cursor;
    use tempfile::tempdir;

    #[test]
    fn test_get_version_success() -> Result<(), Box<dyn std::error::Error>> {
        // Create a temporary directory to simulate the project root.
        let temp_dir = tempdir()?;
        let temp_path = temp_dir.path();

        // Prepare a fake Cargo.toml with a version line.
        let cargo_toml_content = r#"
[package]
name = "example"
version = "0.1.0"
edition = "2021"
"#;
        let cargo_toml_path = temp_path.join("Cargo.toml");
        write(&cargo_toml_path, cargo_toml_content)?;

        // Save original directory and change to our temporary directory.
        let original_dir = env::current_dir()?;
        env::set_current_dir(temp_path)?;

        // Capture the output.
        let mut buffer = Cursor::new(Vec::new());
        get_version(&mut buffer)?;

        let output = String::from_utf8(buffer.into_inner())?;

        // We expect the output to contain "version = " and the version string.
        assert!(
            output.contains("Version: 0.1.0"),
            "Output should contain the version line"
        );

        // Restore original directory.
        env::set_current_dir(&original_dir)?;
        Ok(())
    }

    #[test]
    fn test_get_version_missing_version() -> Result<(), Box<dyn std::error::Error>> {
        // Create a temporary directory with a Cargo.toml that does NOT have a version line.
        let temp_dir = tempdir()?;
        let temp_path = temp_dir.path();

        let cargo_toml_content = r#"
[package]
name = "example"
edition = "2021"
"#;
        let cargo_toml_path = temp_path.join("Cargo.toml");
        write(&cargo_toml_path, cargo_toml_content)?;

        // Change current directory.
        let original_dir = env::current_dir()?;
        env::set_current_dir(temp_path)?;

        // Capture output.
        let mut buffer = Cursor::new(Vec::new());
        let result = get_version(&mut buffer);

        // We expect an error because the version line is missing.
        assert!(result.is_err(), "Expected error when version is missing");

        env::set_current_dir(&original_dir)?;
        Ok(())
    }
}
