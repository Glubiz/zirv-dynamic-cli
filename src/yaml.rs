use std::fs;
use std::process::Stdio;
use tokio::process::Command;
use tokio::time::{sleep, Duration};

use crate::utils::operating_system;
use crate::Script;

/// Reads and executes a YAML script from the given path.
/// If a `pre` script is specified, it is executed before the current script.
/// Commands with an `operating_system` option that does not match the current OS are skipped.
pub async fn run(name: String) -> Result<(), Box<dyn std::error::Error>> {
    let path = format!(".zirv/{}.yaml", name);

    // Read the YAML file.
    let content = fs::read_to_string(path)?;
    let script: Script = serde_yaml::from_str(&content)?;

    println!("\nRunning script: {}", script.name);

    if script.description.is_some() {
        println!("{}", script.description.unwrap());
    }

    // Process each command.
    for cmd_item in script.commands {
        // If options are provided, check the operating_system option.
        if let Some(ref opts) = cmd_item.options {
            if let Some(ref os) = opts.operating_system {
                // Compare lowercased OS names.
                if operating_system(os.to_owned()) != std::env::consts::OS.to_lowercase() {
                    println!("\nSkipping command '{}' due to operating_system mismatch (requires '{:?}', current OS is '{}').",
                             cmd_item.command, os, std::env::consts::OS);
                    continue;
                }
            }
        }
        
        println!("\nExecuting command: '{}'", cmd_item.command);

        // Print description if available.
        if let Some(desc) = cmd_item.description {
            println!("Description: {}", desc);
        }

        let mut command = if std::env::consts::OS.to_lowercase() == "windows" {
            let mut cmd = Command::new("cmd");
            cmd.arg("/C").arg(&cmd_item.command);
            cmd
        } else {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(&cmd_item.command);
            cmd
        };

        // If interactive option is set, inherit I/O.
        if let Some(ref opts) = cmd_item.options {
            if opts.interactive {
                command.stdin(Stdio::inherit())
                       .stdout(Stdio::inherit())
                       .stderr(Stdio::inherit());
            }
        }

        let status = command.status().await?;

        if !status.success() {
            eprintln!("\nCommand '{}' failed with status: {:?}", cmd_item.command, status);
            // Check proceed_on_failure option.
            let proceed = cmd_item.options
                .as_ref()
                .map(|o| o.proceed_on_failure)
                .unwrap_or(false);

            if !proceed {
                return Err(format!("\nCommand '{}' failed", cmd_item.command).into());
            } else {

                println!("\nContinuing despite failure as proceed_on_failure is true.");
            }
        }

        // Apply delay if specified.
        if let Some(delay) = cmd_item.options
            .as_ref()
            .and_then(|o| o.delay_ms) {
            sleep(Duration::from_millis(delay)).await;
        }
    }

    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs::{create_dir_all, write};
    use tempfile::tempdir;

    /// Helper function: writes the given script content into a file under a temporary `.zirv` directory,
    /// changes the current directory to that temp directory, runs the run() function, then restores the original directory.
    async fn run_script_with_content(filename: &str, content: &str) -> Result<(), Box<dyn std::error::Error>> {
        // Create a temporary directory.
        let temp_dir = tempdir()?;
        let temp_path = temp_dir.path();

        // Create a .zirv directory inside temp_dir.
        let zirv_path = temp_path.join(".zirv");
        create_dir_all(&zirv_path)?;

        // Write the script file.
        let file_path = zirv_path.join(filename);
        write(&file_path, content)?;

        // Save the original current directory.
        let original_dir = env::current_dir()?;
        // Change the current directory to the temporary directory.
        env::set_current_dir(temp_path)?;

        // Run the script (strip ".yaml" extension if provided).
        let name = filename.strip_suffix(".yaml").unwrap_or(filename).to_string();
        let result = run(name).await;

        // Restore the original current directory.
        env::set_current_dir(original_dir)?;
        // TempDir is dropped here, cleaning up.
        result
    }

    #[tokio::test]
    async fn test_run_success() {
        // Test a script that simply echoes "hello".
        let yaml = r#"
name: "Test Script"
description: "A test script that echoes hello"
commands:
  - command: "echo hello"
    options:
      proceed_on_failure: false
      interactive: false
"#;
        let res = run_script_with_content("test_success.yaml", yaml).await;
        assert!(res.is_ok(), "Expected script to succeed");
    }

    #[tokio::test]
    async fn test_run_os_mismatch() {
        // Test a script with an operating_system that does not match.
        let yaml = r#"
name: "OS Mismatch Script"
description: "A script that should be skipped due to OS mismatch"
commands:
  - command: "echo should not run"
    options:
      proceed_on_failure: false
      interactive: false
      operating_system: "linux"
"#;
        let res = run_script_with_content("test_os_mismatch.yaml", yaml).await;
        // The command should be skipped, so the script runs (returns Ok(())).
        assert!(res.is_ok(), "Expected script to succeed by skipping mismatched commands");
    }

    #[tokio::test]
    async fn test_run_failure_stops() {
        // Determine the appropriate failing command for the current OS.
        let fail_command = if cfg!(windows) {
            "cmd /C exit 1"
        } else {
            "sh -c 'exit 1'"
        };

        // Test that a failing command stops execution when proceed_on_failure is false.
        let yaml = format!(r#"
name: "Fail Script"
description: "A script that fails and stops"
commands:
  - command: "{}"
    options:
      proceed_on_failure: false
      interactive: false
"#, fail_command);
        let res = run_script_with_content("test_fail.yaml", &yaml).await;
        assert!(res.is_err(), "Expected script to fail and stop execution");
    }

    #[tokio::test]
    async fn test_run_failure_proceed() {
        // Determine the appropriate failing command for the current OS.
        let fail_command = if cfg!(windows) {
            "cmd /C exit 1"
        } else {
            "sh -c 'exit 1'"
        };

        // Test that a failing command is skipped when proceed_on_failure is true,
        // and the script continues to run subsequent commands.
        let yaml = format!(r#"
name: "Proceed Script"
description: "A script that fails but continues"
commands:
  - command: "{}"
    options:
      proceed_on_failure: true
      interactive: false
  - command: "echo continuing"
    options:
      proceed_on_failure: false
      interactive: false
"#, fail_command);
        let res = run_script_with_content("test_proceed.yaml", &yaml).await;
        // The script should succeed because the failure is ignored.
        assert!(res.is_ok(), "Expected script to continue despite a failure");
    }
}
