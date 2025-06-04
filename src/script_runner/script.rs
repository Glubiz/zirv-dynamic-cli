use hashbrown::HashMap;
use serde::Deserialize;

use super::{command_content::CommandContent, secret::Secret};

#[derive(Debug, Deserialize)]
pub struct Script {
    /// A descriptive name for the script.
    pub name: String,
    // A description of what the script does.
    pub description: Option<String>,
    /// Optional list of expected parameter names (in order).
    pub params: Option<Vec<String>>,
    /// Optional list of secret definitions.
    pub secrets: Option<Vec<Secret>>,
    /// A list of commands to execute.
    pub commands: Vec<CommandContent>,
}

impl Script {
    pub async fn run(&self, context: &mut HashMap<String, String>) -> Result<(), String> {
        // Execution loop
        for step in &self.commands {
            match step.execute(context).await {
                Ok(output) => {
                    if output.is_some() {
                        // If the command returns output, you can handle it here
                        println!("Command output: {}", output.unwrap());
                    }
                }
                Err(e) => {
                    return Err(format!(
                        "Error executing command in script '{}': {}",
                        self.name, e
                    ));
                }
            }
        }

        Ok(())
    }
}
