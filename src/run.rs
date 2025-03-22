use std::collections::HashMap;
use std::fs;
use std::process::Stdio;
use tokio::process::Command;
use tokio::time::{sleep, Duration};

use crate::utils::{operating_system, substitute_params};
use crate::Script;

/// Reads and executes a YAML/JSON/TOML script from the given path.
/// Commands with an `operating_system` option that does not match the current OS are skipped.
pub async fn run(
    path: &std::path::Path,
    cli_params: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    // Read file content.
    let content = fs::read_to_string(path)?;
    // Determine file extension.
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    // Parse the script.
    let script: Script = if ext == "yaml" || ext == "yml" {
        serde_yaml::from_str(&content)?
    } else if ext == "json" {
        serde_json::from_str(&content)?
    } else if ext == "toml" {
        toml::from_str(&content)?
    } else {
        return Err(format!("Unsupported file extension: {}", ext).into());
    };

    println!("\nRunning script: {}", script.name);
    if let Some(desc) = script.description {
        println!("Description: {}", desc);
    }

    // Build a parameter map if the script defines expected parameters.
    let param_map: Option<HashMap<String, String>> = if let Some(expected_params) = script.params {
        if expected_params.len() != cli_params.len() {
            return Err(format!(
                "Expected {} parameters, but got {}",
                expected_params.len(),
                cli_params.len()
            )
            .into());
        }

        Some(
            expected_params
                .into_iter()
                .zip(cli_params.iter().cloned())
                .collect(),
        )
    } else {
        None
    };

    // Build a secret map if the script defines expected secrets.
    let secret_map: Option<HashMap<String, String>> = if let Some(secret_defs) = script.secrets {
        let mut map = HashMap::new();
        for secret in secret_defs {
            // Try to read the secret from the environment.
            match std::env::var(&secret.env_var) {
                Ok(val) => {
                    map.insert(secret.name, val);
                }
                Err(_) => {
                    return Err(format!(
                        "Secret '{}' not found in environment variable '{}'",
                        secret.name, secret.env_var
                    )
                    .into());
                }
            }
        }
        Some(map)
    } else {
        None
    };

    // Combine parameters and secrets (secrets can override params if same key exists).
    let combined_map: HashMap<String, String> = match (param_map, secret_map) {
        (Some(params), Some(secrets)) => {
            let mut map = params;
            map.extend(secrets);
            map
        }
        (Some(params), None) => params,
        (None, Some(secrets)) => secrets,
        (None, None) => HashMap::new(),
    };

    // Process each command.
    for mut cmd_item in script.commands {
        // Substitute placeholders in the command.
        if !combined_map.is_empty() {
            cmd_item.command = substitute_params(&cmd_item.command, &combined_map);
        }

        // Check operating_system option.
        if let Some(ref opts) = cmd_item.options {
            if let Some(ref os) = opts.operating_system {
                if operating_system(os.to_owned()) != std::env::consts::OS.to_lowercase() {
                    println!("\nSkipping command '{}' due to operating_system mismatch (requires '{:?}', current OS is '{}').",
                             cmd_item.command, os, std::env::consts::OS);
                    continue;
                }
            }
        }

        println!("\nExecuting command: '{}'", cmd_item.command);
        if let Some(desc) = cmd_item.description.clone() {
            println!("Description: {}", desc);
        }

        // Wrap command in a shell for correct resolution.
        let mut command = if std::env::consts::OS.to_lowercase() == "windows" {
            let mut cmd = Command::new("powershell");
            cmd.arg("-Command").arg(&cmd_item.command);
            cmd
        } else {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(&cmd_item.command);
            cmd
        };

        // Interactive I/O if specified.
        if let Some(ref opts) = cmd_item.options {
            if opts.interactive {
                command
                    .stdin(Stdio::inherit())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit());
            }
        }

        let status = command.status().await?;
        if !status.success() {
            eprintln!(
                "\nCommand '{}' failed with status: {:?}",
                cmd_item.command, status
            );

            let proceed = cmd_item
                .options
                .as_ref()
                .map(|o| o.proceed_on_failure)
                .unwrap_or(false);
            if !proceed {
                return Err(format!("Command '{}' failed", cmd_item.command).into());
            } else {
                println!("\nContinuing despite failure as proceed_on_failure is true.");
            }
        }

        if let Some(delay) = cmd_item.options.as_ref().and_then(|o| o.delay_ms) {
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
    /// changes the current directory to that temporary directory, runs the run() function with provided parameters,
    /// then restores the original directory.
    async fn run_script_with_content(
        filename: &str,
        content: &str,
        params: &[String],
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Create a temporary directory.
        let temp_dir = tempdir()?;
        let temp_path = temp_dir.path();

        // Create the .zirv directory.
        let zirv_path = temp_path.join(".zirv");
        create_dir_all(&zirv_path)?;

        // Write the script file.
        let file_path = zirv_path.join(filename);
        write(&file_path, content)?;

        // Save original directory.
        let original_dir = env::current_dir()?;
        env::set_current_dir(temp_path)?;

        let result = run(&file_path, params).await;

        // Restore original directory.
        env::set_current_dir(original_dir)?;

        result
    }

    #[tokio::test]
    async fn test_run_success() {
        let yaml = r#"
name: "Test Script"
description: "A test script that echoes hello"
commands:
  - command: "echo hello"
    options:
      proceed_on_failure: false
      interactive: false
"#;
        let res = run_script_with_content("test_success.yaml", yaml, &[]).await;

        assert!(res.is_ok(), "Expected script to succeed");
    }

    #[tokio::test]
    async fn test_run_param_substitution() {
        let yaml = r#"
name: "Commit Script"
description: "A script that echoes a commit message"
params:
  - "commit_message"
commands:
  - command: "echo Commit message: ${commit_message}"
    description: "Echo the commit message"
    options:
      proceed_on_failure: false
      interactive: false
"#;
        let params = vec!["My test commit".to_string()];
        let res = run_script_with_content("test_commit.yaml", yaml, &params).await;

        assert!(
            res.is_ok(),
            "Expected script with parameter substitution to succeed"
        );
    }

    #[tokio::test]
    async fn test_run_param_mismatch() {
        let yaml = r#"
name: "Commit Script"
description: "A script that expects one parameter but none provided"
params:
  - "commit_message"
commands:
  - command: "echo Commit message: ${commit_message}"
    options:
      proceed_on_failure: false
      interactive: false
"#;
        let params: Vec<String> = vec![];
        let res = run_script_with_content("test_commit_mismatch.yaml", yaml, &params).await;

        assert!(
            res.is_err(),
            "Expected script to fail due to parameter mismatch"
        );
    }

    #[tokio::test]
    async fn test_run_os_mismatch() {
        let yaml = r#"
name: "OS Mismatch Script"
description: "A script that should skip the command due to OS mismatch"
commands:
  - command: "echo This should not run"
    options:
      proceed_on_failure: false
      interactive: false
      operating_system: "macos"
"#;
        let res = run_script_with_content("test_os_mismatch.yaml", yaml, &[]).await;

        // Command should be skipped, so the script should succeed.
        assert!(
            res.is_ok(),
            "Expected script to succeed by skipping mismatched command"
        );
    }

    #[tokio::test]
    async fn test_run_failure_stops() {
        let fail_command = if cfg!(windows) {
            "cmd /C exit 1"
        } else {
            "sh -c 'exit 1'"
        };

        let yaml = format!(
            r#"
name: "Fail Script"
description: "A script that fails and stops execution"
commands:
  - command: "{}"
    options:
      proceed_on_failure: false
      interactive: false
"#,
            fail_command
        );

        let res = run_script_with_content("test_fail.yaml", &yaml, &[]).await;

        assert!(res.is_err(), "Expected script to fail and stop execution");
    }

    #[tokio::test]
    async fn test_run_failure_proceed() {
        let fail_command = if cfg!(windows) {
            "cmd /C exit 1"
        } else {
            "sh -c 'exit 1'"
        };

        let yaml = format!(
            r#"
name: "Proceed Script"
description: "A script that fails but continues execution"
commands:
  - command: "{}"
    options:
      proceed_on_failure: true
      interactive: false
  - command: "echo continuing"
    options:
      proceed_on_failure: false
      interactive: false
"#,
            fail_command
        );

        let res = run_script_with_content("test_proceed.yaml", &yaml, &[]).await;

        assert!(res.is_ok(), "Expected script to continue despite a failure");
    }

    #[tokio::test]
    async fn test_secret_substitution_success() -> Result<(), Box<dyn std::error::Error>> {
        // Set the required secret.
        env::set_var("COMMIT_PASSWORD", "secret_value");

        let yaml = r#"
name: "Commit Changes"
description: "Commits changes with a provided commit message and secret password"
params:
  - "commit_message"
secrets:
  - name: "commit_password"
    env_var: "COMMIT_PASSWORD"
commands:
  - command: "echo ${commit_message} ${commit_password} > output.txt"
    description: "Write substituted output to file"
    options:
      proceed_on_failure: false
      interactive: false
"#;
        let params = vec!["My commit message".to_string()];
        let res = run_script_with_content("commit.yaml", yaml, &params).await;

        assert!(res.is_ok(), "Expected script to succeed");

        Ok(())
    }

    #[tokio::test]
    async fn test_secret_missing_failure() -> Result<(), Box<dyn std::error::Error>> {
        env::remove_var("COMMIT_PASSWORD");

        let yaml = r#"
name: "Commit Changes"
description: "Fails due to missing secret"
params:
  - "commit_message"
secrets:
  - name: "commit_password"
    env_var: "COMMIT_PASSWORD"
commands:
  - command: "echo ${commit_message} ${commit_password}"
    description: "This command should not execute"
    options:
      proceed_on_failure: false
      interactive: false
"#;
        let params = vec!["My commit message".to_string()];
        let res = run_script_with_content("commit.yaml", yaml, &params).await;

        assert!(res.is_err(), "Expected failure due to missing secret");

        Ok(())
    }
}
