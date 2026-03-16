use hashbrown::HashMap;
use script::Script;

mod command;
mod command_types;
mod fallback_command;
mod operating_system;
mod options;
pub mod script;
mod secret;

pub async fn execute(script: &Script, params: &[String], dry_run: bool) -> Result<(), String> {
    let mut context = build_context(script, params)?;
    script.run(&mut context, dry_run).await?;
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
            let required_count = names.iter().filter(|n| !n.ends_with('?')).count();
            let total_count = names.len();

            // Validate ordering: optional params must come after required
            let mut seen_optional = false;
            for name in names {
                let is_optional = name.ends_with('?');
                if seen_optional && !is_optional {
                    return Err(
                        "Optional parameters must come after all required parameters".to_string(),
                    );
                }
                seen_optional = is_optional;
            }

            // Validate no duplicate param names
            let mut seen_names = std::collections::HashSet::new();
            for name in names {
                let clean = name.strip_suffix('?').unwrap_or(name);
                if !seen_names.insert(clean) {
                    return Err(format!("Duplicate parameter name: '{clean}'"));
                }
            }

            if cli_params.len() < required_count || cli_params.len() > total_count {
                let expected = if required_count == total_count {
                    format!("{required_count}")
                } else {
                    format!("{required_count} to {total_count}")
                };
                return Err(format!(
                    "Expected {expected} parameters, got {}",
                    cli_params.len()
                ));
            }

            names
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    let clean_name = name.strip_suffix('?').unwrap_or(name).to_string();
                    let value = cli_params.get(i).cloned().unwrap_or_default();
                    (clean_name, value)
                })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script_runner::{command::Command, command_types::CommandTypes};

    #[tokio::test]
    async fn test_build_context() {
        let script = Script {
            name: "Test Script".to_string(),
            description: Some("A script for testing".to_string()),
            params: Some(vec!["param1".to_string(), "param2".to_string()]),
            secrets: Some(vec![secret::Secret {
                name: "commit_password".to_string(),
                env_var: "COMMIT_PASSWORD".to_string(),
            }]),
            commands: vec![CommandTypes::Command(Command {
                command: "echo 'Hello World'".to_string(),
                capture: None,
                description: Some("Prints Hello World".to_string()),
                options: None,
            })],
        };

        unsafe {
            std::env::set_var("COMMIT_PASSWORD", "secret123");
        }

        let context = build_context(&script, &["value1".to_string(), "value2".to_string()])
            .expect("Failed to build context");

        assert_eq!(context.get("param1"), Some(&"value1".to_string()));
        assert_eq!(context.get("param2"), Some(&"value2".to_string()));
        assert_eq!(
            context.get("commit_password"),
            Some(&"secret123".to_string())
        );
    }

    fn make_script(params: Vec<String>) -> Script {
        Script {
            name: "Test".to_string(),
            description: None,
            params: Some(params),
            secrets: None,
            commands: vec![CommandTypes::Command(Command {
                command: "echo test".to_string(),
                capture: None,
                description: None,
                options: None,
            })],
        }
    }

    #[tokio::test]
    async fn test_optional_param_provided() {
        let script = make_script(vec!["required".to_string(), "optional?".to_string()]);
        let context = build_context(&script, &["a".to_string(), "b".to_string()]).unwrap();
        assert_eq!(context.get("required"), Some(&"a".to_string()));
        assert_eq!(context.get("optional"), Some(&"b".to_string()));
    }

    #[tokio::test]
    async fn test_optional_param_omitted() {
        let script = make_script(vec!["required".to_string(), "optional?".to_string()]);
        let context = build_context(&script, &["a".to_string()]).unwrap();
        assert_eq!(context.get("required"), Some(&"a".to_string()));
        assert_eq!(context.get("optional"), Some(&String::new()));
    }

    #[tokio::test]
    async fn test_optional_param_too_few() {
        let script = make_script(vec!["required".to_string(), "optional?".to_string()]);
        let result = build_context(&script, &[]);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_optional_param_too_many() {
        let script = make_script(vec!["required".to_string(), "optional?".to_string()]);
        let result = build_context(
            &script,
            &["a".to_string(), "b".to_string(), "c".to_string()],
        );
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_all_optional() {
        let script = make_script(vec!["a?".to_string(), "b?".to_string()]);
        let context = build_context(&script, &[]).unwrap();
        assert_eq!(context.get("a"), Some(&String::new()));
        assert_eq!(context.get("b"), Some(&String::new()));
    }

    #[tokio::test]
    async fn test_optional_before_required_rejected() {
        let script = make_script(vec!["optional?".to_string(), "required".to_string()]);
        let result = build_context(&script, &["a".to_string(), "b".to_string()]);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("Optional parameters must come after all required parameters")
        );
    }

    #[tokio::test]
    async fn test_duplicate_param_names_rejected() {
        let script = make_script(vec!["name".to_string(), "name".to_string()]);
        let result = build_context(&script, &["a".to_string(), "b".to_string()]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Duplicate parameter name"));
    }

    #[tokio::test]
    async fn test_duplicate_optional_param_names_rejected() {
        let script = make_script(vec!["name".to_string(), "name?".to_string()]);
        let result = build_context(&script, &["a".to_string(), "b".to_string()]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Duplicate parameter name"));
    }
}
