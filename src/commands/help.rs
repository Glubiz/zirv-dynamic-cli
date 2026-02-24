use std::{fs, io::Write, path::Path, path::PathBuf};

use crate::utils::{
    SCRIPT_DIR_NAME, SUPPORTED_EXTENSIONS, Shortcuts, home_dir, parse_script_content,
};

fn write_scripts<W: Write>(writer: &mut W, dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_file()
            && let Some(ext) = path.extension().and_then(|s| s.to_str())
            && SUPPORTED_EXTENSIONS.contains(&ext)
            && path.file_name().unwrap() != ".shortcuts.yaml"
        {
            let content = fs::read_to_string(&path)?;
            let script = parse_script_content(&content, ext)?;

            let file_name = path.file_name().unwrap().to_string_lossy();
            writeln!(writer, "-------------------------------------------------")?;
            writeln!(writer, "File: {file_name}")?;
            writeln!(writer, "  Name: {}", script.name)?;
            if let Some(desc) = script.description {
                writeln!(writer, "  Description: {desc}")?;
            }
            if let Some(params) = &script.params {
                writeln!(writer, "  Required Parameters:")?;
                for param in params {
                    writeln!(writer, "    {param}")?;
                }
            }
        }
    }

    Ok(())
}

fn write_shortcuts<W: Write>(writer: &mut W, dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let shortcuts_path = dir.join(".shortcuts.yaml");
    if shortcuts_path.exists() {
        let content = fs::read_to_string(shortcuts_path)?;
        let shortcuts: Shortcuts = serde_yaml::from_str(&content)?;
        for (key, value) in shortcuts.shortcuts {
            writeln!(writer, "  {key} -> {value}")?;
        }
    }
    Ok(())
}

pub fn show_help<W: Write>(writer: &mut W) -> Result<(), Box<dyn std::error::Error>> {
    let base_dir = PathBuf::from(SCRIPT_DIR_NAME);

    if base_dir.exists() {
        writeln!(writer, "\nAvailable Scripts:")?;
        write_scripts(writer, &base_dir)?;

        let shortcuts_path = base_dir.join(".shortcuts.yaml");
        if shortcuts_path.exists() {
            writeln!(writer, "\nAvailable Shortcuts:")?;
            write_shortcuts(writer, &base_dir)?;
            writeln!(writer, "  i -> init")?;
            writeln!(writer, "  c -> create")?;
            writeln!(writer, "  v -> version")?;
            writeln!(writer, "  h -> help")?;
        }
    }

    let root = home_dir()?.join(SCRIPT_DIR_NAME);

    if root.exists() {
        writeln!(writer, "\nGlobal Base Scripts:")?;
        writeln!(
            writer,
            "Global scripts are overwritten by above mentioned scripts if they share name."
        )?;
        writeln!(writer, "Home Directory: {root:?}")?;
        write_scripts(writer, &root)?;

        let shortcuts_path = root.join(".shortcuts.yaml");
        if shortcuts_path.exists() {
            writeln!(writer, "\nGlobal Shortcuts:")?;
            write_shortcuts(writer, &root)?;
        }
    } else {
        writeln!(
            writer,
            "No scripts found. Please create a .zirv directory in {root:?}."
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs::{create_dir_all, write};
    use std::io::Cursor;
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    /// Helper function to create a temporary .zirv directory with optional files.
    fn setup_zirv_dir(temp_dir: &Path) -> PathBuf {
        let zirv_dir = temp_dir.join(".zirv");
        create_dir_all(&zirv_dir).unwrap();
        zirv_dir
    }

    /// Test that a local script file is listed correctly.
    #[test]
    fn test_show_help_with_script() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let temp_path = temp_dir.path().to_path_buf();
        let zirv_dir = setup_zirv_dir(&temp_path);

        // Create a dummy script file (YAML) in .zirv.
        let script_content = r#"
name: "Test Script"
description: "A dummy script for testing."
params: []
commands: []
        "#;
        let script_file = zirv_dir.join("test.yaml");
        write(&script_file, script_content)?;

        let original_dir = env::current_dir()?;
        env::set_current_dir(&temp_path)?;

        let mut buffer = Cursor::new(Vec::new());
        show_help(&mut buffer)?;
        let output = String::from_utf8(buffer.into_inner())?;

        assert!(output.contains("File:"), "Output should contain 'File:'");

        assert!(
            output.contains("Test Script"),
            "Output should contain the script name 'Test Script'"
        );

        assert!(
            output.contains("Description:"),
            "Output should contain 'Description:'"
        );

        env::set_current_dir(original_dir)?;

        Ok(())
    }

    /// Test that shortcuts are listed in the help output.
    #[test]
    fn test_show_help_with_shortcuts() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let temp_path = temp_dir.path().to_path_buf();
        let zirv_dir = setup_zirv_dir(&temp_path);

        // Create a dummy script file.
        let script_content = r#"
name: "Test Script"
description: "A dummy script for testing shortcuts."
params: []
commands: []
        "#;
        let script_file = zirv_dir.join("test.yaml");
        write(&script_file, script_content)?;

        // Create a shortcuts file mapping "t" to "test.yaml".
        let shortcuts_content = r#"
shortcuts:
  t: "test.yaml"
        "#;
        let shortcuts_file = zirv_dir.join(".shortcuts.yaml");
        write(&shortcuts_file, shortcuts_content)?;

        let original_dir = env::current_dir()?;
        env::set_current_dir(&temp_path)?;

        let mut buffer = Cursor::new(Vec::new());
        show_help(&mut buffer)?;
        let output = String::from_utf8(buffer.into_inner())?;

        assert!(
            output.contains("Available Shortcuts:"),
            "Output should list shortcuts"
        );

        assert!(
            output.contains("t -> test.yaml"),
            "Output should contain the shortcut mapping 't -> test.yaml'"
        );

        assert!(
            output.contains("h -> help"),
            "Output should include a help shortcut"
        );

        env::set_current_dir(original_dir)?;

        Ok(())
    }
}
