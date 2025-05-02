use hashbrown::HashMap;
use std::fs;
use std::process::Stdio;
use tokio::process::Command;
use tokio::time::{Duration, sleep};

use crate::structs::os::operating_system;
use crate::structs::script::{CommandItem, Script};
use crate::utils::substitute_params;

/// Reads and executes a YAML/JSON/TOML script from the given path.
/// Supports `capture` (capture stdout into a variable) and
/// `on_failure` (run fallback commands then retry once).
pub async fn run(
    path: &std::path::Path,
    cli_params: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    // Load file
    let content = fs::read_to_string(path)?;
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    let script: Script = match ext.as_str() {
        "yaml" | "yml" => serde_yaml::from_str(&content)?,
        "json" => serde_json::from_str(&content)?,
        "toml" => toml::from_str(&content)?,
        other => return Err(format!("Unsupported extension: {}", other).into()),
    };

    println!("\nRunning script: {}", script.name);
    if let Some(desc) = &script.description {
        println!("Description: {}", desc);
    }

    // Build initial context from params + secrets
    let mut context: HashMap<String, String> = {
        // params
        let params = if let Some(names) = &script.params {
            if names.len() != cli_params.len() {
                return Err(format!(
                    "Expected {} parameters, got {}",
                    names.len(),
                    cli_params.len()
                )
                .into());
            }

            names
                .iter()
                .cloned()
                .zip(cli_params.iter().cloned())
                .collect()
        } else {
            HashMap::new()
        };

        // secrets
        let mut map = params;
        if let Some(secret_defs) = &script.secrets {
            for sd in secret_defs {
                let val = std::env::var(&sd.env_var).map_err(|_| {
                    format!("Secret '{}' not found in env '{}'", sd.name, sd.env_var)
                })?;
                map.insert(sd.name.clone(), val);
            }
        }
        map
    };

    // Helper to run a single invocation of a step
    async fn invoke(
        cmd_str: &str,
        step: &CommandItem,
    ) -> Result<Option<(String, String)>, Box<dyn std::error::Error>> {
        // pick shell
        let mut child = if cfg!(windows) {
            let mut c = Command::new("powershell");
            c.arg("-Command").arg(cmd_str);
            c
        } else {
            let mut c = Command::new("sh");
            c.arg("-c").arg(cmd_str);
            c
        };

        // interactive I/O
        if step.options.interactive {
            child
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
        }

        // decide capture vs status
        if let Some(var) = &step.capture {
            let out = child.output().await?;
            if !out.status.success() {
                return Err(format!("`{}` failed", cmd_str).into());
            }
            let val = String::from_utf8_lossy(&out.stdout).trim().to_string();
            Ok(Some((var.clone(), val)))
        } else {
            let status = child.status().await?;
            if !status.success() {
                return Err(format!("`{}` failed", cmd_str).into());
            }
            Ok(None)
        }
    }

    // Main loop over steps
    for step in &script.commands {
        // OS filter
        if let Some(os) = &step.options.operating_system {
            if operating_system(os.clone()) != std::env::consts::OS.to_lowercase() {
                continue;
            }
        }

        // substitute
        let cmd_str = substitute_params(&step.command, &context);

        println!("\n> {}", cmd_str);
        if let Some(d) = &step.description {
            println!("  # {}", d);
        }

        // first attempt
        let first = invoke(&cmd_str, step).await;

        if let Err(err) = first {
            eprintln!("Step error: {:?}", err);

            if let Some(commands) = &step.options.on_failure {
                // on_failure chain
                for fb in commands {
                    let fb_cmd = substitute_params(&fb.command, &context);
                    let _ = invoke(&fb_cmd, fb).await.map_err(|e| {
                        eprintln!("  on_failure `{}` errored: {:?}", fb_cmd, e);
                    });
                }

                // retry once
                match invoke(&cmd_str, step).await {
                    Ok(Some((k, v))) => {
                        context.insert(k, v);
                    }
                    Ok(None) => {
                        // success on retry
                    }
                    Err(err2) => {
                        eprintln!("Retry also failed: {:?}", err2);
                        if !step.options.proceed_on_failure {
                            return Err(err2);
                        }
                    }
                }
            }

            if !step.options.proceed_on_failure {
                return Err(err);
            }
        } else if let Ok(Some((k, v))) = first {
            // captured on first run
            context.insert(k, v);
        }

        // optional delay
        if let Some(d) = step.options.delay_ms {
            sleep(Duration::from_millis(d)).await;
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

    /// Run the script in a freshly-created tempdir (with `.zirv`),
    /// but *return* the tempdir so we can inspect files *after* run().
    async fn run_in_temp<F>(
        filename: &str,
        content: &str,
        params: &[String],
        test_body: F,
    ) -> Result<(), Box<dyn std::error::Error>>
    where
        F: FnOnce(&std::path::Path) -> Result<(), Box<dyn std::error::Error>>,
    {
        let tmp = tempdir()?;
        let root = tmp.path();
        // make .zirv
        let zirv = root.join(".zirv");
        fs::create_dir_all(&zirv)?;
        // write script
        let script = zirv.join(filename);
        fs::write(&script, content)?;
        // cd into root
        let old = env::current_dir()?;
        env::set_current_dir(root)?;
        // run
        let res = run(&script, params).await;
        // restore cwd
        env::set_current_dir(old)?;
        // let test_body inspect root
        test_body(root)?;
        // propagate run() result
        res
    }

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
        unsafe {
            env::set_var("COMMIT_PASSWORD", "secret_value");
        }

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
        unsafe {
            // Remove the secret from the environment to simulate a missing secret.
            env::remove_var("COMMIT_PASSWORD");
        }

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

    #[tokio::test]
    async fn test_on_failure_chain_executes_and_bails() -> Result<(), Box<dyn std::error::Error>> {
        // First step fails, on_failure writes fallback2.txt,
        // proceed_on_failure=false so run() returns Err.
        let fail = if cfg!(windows) {
            "exit 1"
        } else {
            "sh -c 'exit 1'"
        };

        let yaml = format!(
            r#"
name: "OnFailure Bail"
commands:
  - command: "{}"
    options:
      proceed_on_failure: false
      interactive: false
      on_failure:
        - command: "echo BAILBACK > fallback2.txt"
          options:
            proceed_on_failure: false
            interactive: false
"#,
            fail
        );

        let res = run_in_temp("fail2.yaml", &yaml, &[], |root| {
            // fallback2.txt should still be created
            let fb = fs::read_to_string(root.join("fallback2.txt"))?;
            assert_eq!(fb.trim(), "BAILBACK");
            Ok(())
        })
        .await;

        assert!(res.is_err(), "Expected run() to Err after retry");
        Ok(())
    }
}
