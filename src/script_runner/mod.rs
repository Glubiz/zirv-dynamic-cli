use hashbrown::HashMap;
use script::Script;

mod command;
mod command_content;
mod command_options;
mod operating_system;
pub mod script;
mod secret;

pub async fn execute(script: &Script, params: &[String]) -> Result<(), String> {
    // Build the context from script parameters and secrets
    let mut context = build_context(script, params)?;

    // Execution loop
    script.run(&mut context).await?;

    // Placeholder for the main execution logic
    // This function will orchestrate the execution of commands, handling files, etc.
    Ok(())
}

fn build_context(
    script: &Script,
    cli_params: &[String],
) -> Result<HashMap<String, String>, String> {
    // Build initial context from params + secrets
    let context: HashMap<String, String> = {
        // params
        let params = if let Some(names) = &script.params {
            if names.len() != cli_params.len() {
                return Err(format!(
                    "Expected {} parameters, got {}",
                    names.len(),
                    cli_params.len()
                ));
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

    Ok(context)
}
