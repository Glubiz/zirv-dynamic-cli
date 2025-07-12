use std::process::Stdio;

use serde::Deserialize;
use tokio::process::Command as TokioCommand;

use crate::script_runner::options::Options;

#[derive(Debug, Deserialize, Clone, Default)]
pub struct FallbackCommand {
    pub command: String,
    pub description: Option<String>,
    pub options: Option<Options>,
}

impl FallbackCommand {
    pub async fn invoke(&self) -> Result<(), Box<dyn std::error::Error>> {
        // Pick shell based on the OS
        let mut shell = if cfg!(windows) {
            let mut c = TokioCommand::new("powershell");
            c.arg("-Command").arg(&self.command);
            c
        } else {
            let mut c = TokioCommand::new("sh");
            c.arg("-c").arg(&self.command);
            c
        };

        println!("Executing command: {}", &self.command);
        if let Some(description) = &self.description {
            println!("Description: {description}");
        }

        if let Some(options) = &self.options {
            if options.interactive {
                shell
                    .stdin(Stdio::inherit())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit());
            }
        }

        let status = shell.status().await?;

        if !status.success() {
            return Err(format!("`{}` failed", &self.command).into());
        }

        Ok(())
    }
}
