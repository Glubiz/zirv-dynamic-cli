//! Comment-preserving edits to the operator's ctx config.

use std::io::Write;

use toml_edit::{DocumentMut, Item, Value};

use super::{CtxResult, config, state};

#[derive(Debug, clap::Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum ConfigCommand {
    /// Print operator ~/.zirv/ctx.toml, or one dotted key, as TOML.
    Show { key: Option<String> },
    /// Set a dotted key; parse VALUE as TOML, otherwise as a string. Asks for approval.
    Set {
        key: String,
        #[arg(allow_hyphen_values = true)]
        value: String,
    },
    /// Append one array element, creating the array if absent; duplicates are unchanged. Asks for approval.
    Add {
        key: String,
        #[arg(allow_hyphen_values = true)]
        value: String,
    },
}

fn key_parts(key: &str) -> CtxResult<Vec<toml_edit::Key>> {
    toml_edit::Key::parse(key).map_err(|e| format!("invalid config key {key:?}: {e}").into())
}

fn slot<'a>(item: &'a mut Item, parts: &[toml_edit::Key]) -> CtxResult<&'a mut Item> {
    let Some((key, rest)) = parts.split_first() else {
        return Ok(item);
    };
    if item.is_none() {
        *item = Item::Table(toml_edit::Table::new());
    }
    let table = item
        .as_table_like_mut()
        .ok_or_else(|| format!("config key {key}: parent is not a table"))?;
    slot(table.entry(key.get()).or_insert(Item::None), rest)
}

fn semantic_value(value: &Value) -> CtxResult<toml::Value> {
    let mut value = value.clone();
    value.decor_mut().clear();
    let table: toml::Table = toml::from_str(&format!("value = {value}"))?;
    Ok(table["value"].clone())
}

fn edit(doc: &mut DocumentMut, key: &str, raw: &str, append: bool) -> CtxResult<bool> {
    let mut value = raw.parse::<Value>().unwrap_or_else(|_| Value::from(raw));
    let target = slot(doc.as_item_mut(), &key_parts(key)?)?;
    if append {
        if target.is_none() {
            *target = Item::Value(Value::Array(toml_edit::Array::new()));
        }
        let array = target
            .as_array_mut()
            .ok_or_else(|| format!("config key {key} is not an array"))?;
        let incoming = semantic_value(&value)?;
        for existing in array.iter() {
            if semantic_value(existing)? == incoming {
                return Ok(false);
            }
        }
        // Comments after the final comma belong to the array's trailing
        // decoration. Keep them before the new element, beside the old one.
        if (array.trailing_comma() || array.is_empty())
            && let Some((before_close, closing_indent)) = array
                .trailing()
                .as_str()
                .unwrap_or_default()
                .rsplit_once('\n')
        {
            let indent = array
                .iter()
                .last()
                .and_then(|v| v.decor().prefix())
                .and_then(|prefix| prefix.as_str())
                .and_then(|prefix| prefix.rsplit_once('\n'))
                .map(|(_, indent)| indent)
                .unwrap_or("  ");
            value
                .decor_mut()
                .set_prefix(format!("{before_close}\n{indent}"));
            let trailing = format!("\n{closing_indent}");
            array.set_trailing(trailing);
        }
        array.push_formatted(value);
    } else {
        if let Some(old) = target.as_value() {
            *value.decor_mut() = old.decor().clone();
        }
        *target = Item::Value(value);
    }
    Ok(true)
}

pub fn run(args: &ConfigArgs, w: &mut dyn Write) -> CtxResult<i32> {
    let path = config::operator_path()?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let (key, value, append) = match &args.command {
        ConfigCommand::Show { key: None } => {
            write!(w, "{text}")?;
            return Ok(0);
        }
        ConfigCommand::Show { key: Some(key) } => {
            let doc: DocumentMut = text.parse()?;
            let mut item = doc.as_item();
            for part in key_parts(key)? {
                item = item
                    .get(part.get())
                    .ok_or_else(|| format!("config key {key} is not set"))?;
            }
            writeln!(w, "{item}")?;
            return Ok(0);
        }
        ConfigCommand::Set { key, value } => (key, value, false),
        ConfigCommand::Add { key, value } => (key, value, true),
    };
    let mut doc: DocumentMut = text.parse()?;
    let changed = edit(&mut doc, key, value, append)?;
    let updated = doc.to_string();
    config::validate_operator_document(&updated)
        .map_err(|e| format!("refusing to update {}: {e}", path.display()))?;
    if !changed {
        writeln!(w, "{key}: element already present (unchanged)")?;
        return Ok(0);
    }
    if let Some(parent) = path.parent() {
        state::create_private_dir_all(parent)?;
    }
    state::write_private(&path, &updated)?;
    writeln!(w, "{key}: {}", if append { "added" } else { "set" })?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::super::testenv::HomeGuard;
    use super::*;

    fn invoke(command: ConfigCommand) -> CtxResult<String> {
        let mut output = Vec::new();
        assert_eq!(run(&ConfigArgs { command }, &mut output)?, 0);
        Ok(String::from_utf8(output)?)
    }

    fn set(key: &str, value: &str) -> ConfigCommand {
        ConfigCommand::Set {
            key: key.into(),
            value: value.into(),
        }
    }

    fn add(key: &str, value: &str) -> ConfigCommand {
        ConfigCommand::Add {
            key: key.into(),
            value: value.into(),
        }
    }

    #[test]
    fn cli_accepts_negative_toml_values() {
        use clap::Parser;
        for verb in ["set", "add"] {
            let cli = super::super::CtxCli::try_parse_from([
                "zirv ctx",
                "config",
                verb,
                "score.window",
                "-3",
            ])
            .unwrap();
            assert!(matches!(cli.verb, super::super::CtxVerb::Config(_)));
        }
    }

    #[test]
    fn creates_operator_file_and_parses_typed_values_and_strings() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        invoke(set("worker.codex", "gpt-5")).unwrap();
        invoke(set("memory.enabled", "true")).unwrap();
        invoke(set("score.window", "3")).unwrap();
        invoke(set("dash.workdir_roots", r#"["/tmp/a", "/tmp/b"]"#)).unwrap();
        let text = invoke(ConfigCommand::Show { key: None }).unwrap();
        let table: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(table["worker"]["codex"].as_str(), Some("gpt-5"));
        assert_eq!(table["memory"]["enabled"].as_bool(), Some(true));
        assert_eq!(table["score"]["window"].as_integer(), Some(3));
        assert_eq!(table["dash"]["workdir_roots"].as_array().unwrap().len(), 2);
        assert_eq!(
            invoke(ConfigCommand::Show {
                key: Some("score.window".into())
            })
            .unwrap()
            .trim(),
            "3"
        );
    }

    #[test]
    fn preserves_comments_and_formatting_and_add_is_idempotent() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let original = "# my config\n[worker] # models\ncodex  = 'old' # keep\n\n[dash]\nworkdir_roots = [\n  '/tmp/a', # first\n]\n";
        let path = config::operator_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, original).unwrap();
        invoke(set("worker.codex", "new")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, original.replace("'old'", "\"new\""));
        assert!(
            invoke(add("dash.workdir_roots", "\"/tmp/a\""))
                .unwrap()
                .contains("already present")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        invoke(add("dash.workdir_roots", "/tmp/b")).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("'/tmp/a', # first")
        );
        invoke(add("safety.allow", "cargo test")).unwrap();
        assert!(
            invoke(add("safety.allow", "cargo test"))
                .unwrap()
                .contains("already present")
        );
    }

    #[test]
    fn invalid_edits_leave_file_untouched_including_separate_policy_sections() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        invoke(set("memory.enabled", "true")).unwrap();
        let path = config::operator_path().unwrap();
        let original = std::fs::read_to_string(&path).unwrap();
        for command in [
            set("unknown", "true"),
            set("dash.unknown", "3"),
            set("memory.enabled", "nope"),
            set("safety.unknown", "true"),
            set("policy.unknown", "true"),
            set("safety.allow", "3"),
            set("policy.network", "3"),
            add("memory.enabled", "true"),
            set("memory.enabled.child", "true"),
            set("", "3"),
        ] {
            assert!(invoke(command).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        }
    }

    #[test]
    fn missing_show_and_invalid_new_config_create_nothing() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        assert_eq!(invoke(ConfigCommand::Show { key: None }).unwrap(), "");
        assert!(
            invoke(ConfigCommand::Show {
                key: Some("worker.codex".into())
            })
            .is_err()
        );
        assert!(invoke(set("worker.codex", "3")).is_err());
        assert!(!home.path().join(".zirv").exists());
    }

    #[test]
    fn edits_inline_tables_and_refuses_malformed_existing_document() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let path = config::operator_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "worker = { codex = 'old' } # keep\n").unwrap();
        invoke(set("worker.codex", "new")).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "worker = { codex = \"new\" } # keep\n"
        );
        std::fs::write(&path, "[broken").unwrap();
        assert!(invoke(set("worker.codex", "new")).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[broken");
    }
}
