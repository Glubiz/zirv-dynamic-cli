//! `.zirv/.settings.toml` -- the zirv-wide on/off switchboard.
//!
//! Distinct from `ctx.toml`, which tunes ctx supervisor *behavior*: if the
//! question is "yes/no, may zirv use this thing", it belongs here; anything
//! else belongs in `ctx.toml`. The first (and so far only) section is
//! `[agents]`, one per-adapter `enabled` flag.
//!
//! Layering mirrors `ctx.toml`'s (`~/.zirv/.settings.toml`, then
//! `<repo>/.zirv/.settings.toml`, then `ZIRV_AGENT_<NAME>_ENABLED`), but the
//! fold is deliberately *not* a deep merge: each file is parsed on its own,
//! and the three answers are combined per agent as
//!
//! ```text
//! final(name) = env(name) if set
//!             else home(name).unwrap_or(true) && repo(name).unwrap_or(true)
//! ```
//!
//! so a repository can only narrow (`enabled = true` in a repo file is a
//! silent no-op: there is nothing to refuse), while the operator -- the home
//! file or the environment -- may disable *or* re-enable in either direction.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::commands::ctx::CtxResult;

pub const SETTINGS_FILE: &str = ".settings.toml";

pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// One `.settings.toml` file, parsed on its own (never merged with another
/// layer). `#[serde(default)]` with no `deny_unknown_fields`: an unrecognized
/// top-level section is forward-compat, not an error -- `AgentGate::load`
/// checks for and warns about it separately, by comparing the raw table's
/// keys before this type ever sees them.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct SettingsFile {
    pub agents: HashMap<String, AgentSettings>,
}

/// `enabled = None` means this layer is silent on the agent -- distinct from
/// `Some(true)`, which a repo layer is still not allowed to make count for
/// anything (see the module doc's fold). `deny_unknown_fields` here (unlike
/// `SettingsFile`) because a typo'd key *inside* a known `[agents.<name>]`
/// table is a mistake worth failing loudly on, not forward-compat.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentSettings {
    pub enabled: Option<bool>,
}

/// Where a `false` came from, for the refusal message and `zirv ctx status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    OperatorFile(PathBuf),
    RepoFile(PathBuf),
    Env(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentState {
    pub enabled: bool,
    /// `None` whenever `enabled` is true: there is nothing to attribute a
    /// permissive default to.
    pub disabled_by: Option<Source>,
}

impl AgentState {
    /// Where `zirv ctx status` says this state came from: the file path or
    /// environment variable that disabled it, or "default" when nothing did.
    pub fn location(&self) -> String {
        match &self.disabled_by {
            None => "default".to_string(),
            Some(Source::OperatorFile(path)) | Some(Source::RepoFile(path)) => {
                path.display().to_string()
            }
            Some(Source::Env(var)) => format!("environment: {var}"),
        }
    }
}

/// Per-agent enable/disable state, folded from every layer. `Default` is
/// permissive (an empty gate enables every name, known or not), which is what
/// a caller that never loaded settings -- or hit a codepath that has to
/// degrade past a load failure -- gets.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AgentGate {
    states: HashMap<String, AgentState>,
}

/// Reads one layer, warning (never failing) about a top-level section other
/// than `agents` or an agent name this build's adapter registry doesn't know
/// about. `known` drives that second check.
fn read_layer(path: &Path, known: &[&str]) -> CtxResult<Option<SettingsFile>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let raw: toml::Table = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;

    for key in raw.keys() {
        if key != "agents" {
            crate::output::warn(format!(
                "{}: unknown section `[{key}]`; ignoring",
                path.display()
            ));
        }
    }
    if let Some(agents) = raw.get("agents").and_then(|v| v.as_table()) {
        for name in agents.keys() {
            if !known.contains(&name.as_str()) {
                crate::output::warn(format!(
                    "{}: unknown agent '{name}' in [agents]; ignoring",
                    path.display()
                ));
            }
        }
    }

    let settings: SettingsFile = toml::Value::Table(raw)
        .try_into()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Some(settings))
}

fn env_var_for(name: &str) -> String {
    format!("ZIRV_AGENT_{}_ENABLED", name.to_uppercase())
}

impl AgentGate {
    /// Layers `~/.zirv/.settings.toml`, then `<repo>/.zirv/.settings.toml`,
    /// then `ZIRV_AGENT_<NAME>_ENABLED`, per agent name known to
    /// `adapters::all`. See the module doc for the exact fold.
    pub fn load(repo: &Path, env: EnvLookup<'_>) -> CtxResult<Self> {
        let known: Vec<&'static str> = crate::commands::ctx::adapters::all(None)
            .iter()
            .map(|a| a.name())
            .collect();

        let operator_path = crate::utils::home_dir()
            .ok()
            .map(|home| home.join(crate::utils::SCRIPT_DIR_NAME).join(SETTINGS_FILE));
        let operator = match &operator_path {
            Some(path) => read_layer(path, &known)?,
            None => None,
        };

        let repo_path = repo.join(crate::utils::SCRIPT_DIR_NAME).join(SETTINGS_FILE);
        let repo_layer = read_layer(&repo_path, &known)?;

        let mut states = HashMap::new();
        for name in known {
            let operator_enabled = operator
                .as_ref()
                .and_then(|s| s.agents.get(name))
                .and_then(|a| a.enabled);
            let repo_enabled = repo_layer
                .as_ref()
                .and_then(|s| s.agents.get(name))
                .and_then(|a| a.enabled);

            let env_var = env_var_for(name);
            let env_value = match env(&env_var) {
                Some(raw) => Some(
                    raw.parse::<bool>()
                        .map_err(|_| format!("{env_var}: expected true or false, got '{raw}'"))?,
                ),
                None => None,
            };

            let state = if let Some(enabled) = env_value {
                let disabled_by = (!enabled).then(|| Source::Env(env_var.clone()));
                AgentState {
                    enabled,
                    disabled_by,
                }
            } else {
                let operator_component = operator_enabled.unwrap_or(true);
                let repo_component = repo_enabled.unwrap_or(true);
                let enabled = operator_component && repo_component;
                let disabled_by = if enabled {
                    None
                } else if !operator_component {
                    operator_path.clone().map(Source::OperatorFile)
                } else {
                    Some(Source::RepoFile(repo_path.clone()))
                };
                AgentState {
                    enabled,
                    disabled_by,
                }
            };

            states.insert(name.to_string(), state);
        }

        Ok(Self { states })
    }

    /// Unknown names are enabled: this gate only ever narrows what its known
    /// adapters may do, never what a caller thinks a name means.
    pub fn is_enabled(&self, name: &str) -> bool {
        self.states.get(name).is_none_or(|s| s.enabled)
    }

    /// `None` when the agent is enabled (or unknown); otherwise a message
    /// naming the cause and the remedy, matching the plain tone of
    /// `CodexAdapter::ready`.
    pub fn refusal(&self, name: &str) -> Option<String> {
        let state = self.states.get(name)?;
        if state.enabled {
            return None;
        }
        let location = state.location();
        let env_var = env_var_for(name);
        let repo_note = matches!(state.disabled_by, Some(Source::RepoFile(_)))
            .then_some(" (a repository may only disable an agent, never enable one)")
            .unwrap_or_default();
        Some(format!(
            "agent '{name}' is disabled by {location}; zirv will not launch or parse it. \
             Remove [agents.{name}] enabled = false from that file, set {env_var}=true, or pass \
             a different --agent.{repo_note}"
        ))
    }

    pub fn states(&self) -> impl Iterator<Item = (&str, &AgentState)> {
        self.states.iter().map(|(k, v)| (k.as_str(), v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as StdHashMap;

    fn env_map(pairs: &[(&str, &str)]) -> StdHashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn write_settings(dir: &Path, contents: &str) {
        std::fs::create_dir_all(dir.join(".zirv")).expect("mkdir");
        std::fs::write(dir.join(".zirv").join(SETTINGS_FILE), contents).expect("write");
    }

    #[test]
    fn defaults_enable_every_known_adapter() {
        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let gate = AgentGate::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(gate.is_enabled("claude"));
        assert!(gate.is_enabled("codex"));
        assert!(gate.refusal("claude").is_none());
        assert!(gate.refusal("codex").is_none());
    }

    #[test]
    fn an_operator_file_can_disable_an_agent() {
        let home = tempfile::tempdir().expect("tempdir");
        write_settings(home.path(), "[agents.codex]\nenabled = false\n");
        let repo = tempfile::tempdir().expect("tempdir");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        let gate = AgentGate::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(!gate.is_enabled("codex"));
        assert!(gate.is_enabled("claude"), "only codex was disabled");
    }

    #[test]
    fn a_repository_may_disable_an_agent() {
        let repo = tempfile::tempdir().expect("tempdir");
        write_settings(repo.path(), "[agents.codex]\nenabled = false\n");
        let home = tempfile::tempdir().expect("tempdir");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        let gate = AgentGate::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(!gate.is_enabled("codex"));
    }

    #[test]
    fn a_repository_may_not_re_enable_an_agent_the_operator_disabled() {
        let home = tempfile::tempdir().expect("tempdir");
        write_settings(home.path(), "[agents.codex]\nenabled = false\n");
        let repo = tempfile::tempdir().expect("tempdir");
        write_settings(repo.path(), "[agents.codex]\nenabled = true\n");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        let gate = AgentGate::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(
            !gate.is_enabled("codex"),
            "a repo re-enabling must be a silent no-op"
        );
    }

    #[test]
    fn the_environment_is_the_operator_and_may_re_enable_a_repo_disabled_agent() {
        let repo = tempfile::tempdir().expect("tempdir");
        write_settings(repo.path(), "[agents.codex]\nenabled = false\n");
        let home = tempfile::tempdir().expect("tempdir");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let env = env_map(&[("ZIRV_AGENT_CODEX_ENABLED", "true")]);
        let gate = AgentGate::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(
            gate.is_enabled("codex"),
            "the environment is the operator, not the checkout"
        );
    }

    #[test]
    fn an_unknown_agent_name_is_warned_about_rather_than_failing_the_load() {
        let repo = tempfile::tempdir().expect("tempdir");
        write_settings(repo.path(), "[agents.gemini]\nenabled = false\n");
        let home = tempfile::tempdir().expect("tempdir");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        let gate = AgentGate::load(repo.path(), &|k| empty.get(k).cloned())
            .expect("an unknown agent name must not fail the load");
        assert!(gate.is_enabled("claude"));
        assert!(gate.is_enabled("codex"));
    }

    #[test]
    fn an_unknown_top_level_section_does_not_fail_the_load() {
        let repo = tempfile::tempdir().expect("tempdir");
        write_settings(repo.path(), "[tools]\nfoo = true\n");
        let home = tempfile::tempdir().expect("tempdir");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        AgentGate::load(repo.path(), &|k| empty.get(k).cloned())
            .expect("an unknown top-level section must not fail the load");
    }

    #[test]
    fn an_unknown_key_inside_an_agent_table_is_rejected_loudly() {
        let repo = tempfile::tempdir().expect("tempdir");
        write_settings(repo.path(), "[agents.codex]\nenbaled = false\n");
        let home = tempfile::tempdir().expect("tempdir");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        let err = AgentGate::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a typo'd key inside a known agent table must be rejected");
        assert!(err.to_string().contains("enbaled"), "got {err}");
    }

    #[test]
    fn a_malformed_settings_file_names_the_file() {
        let repo = tempfile::tempdir().expect("tempdir");
        write_settings(repo.path(), "not [ valid toml");
        let home = tempfile::tempdir().expect("tempdir");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        let err = AgentGate::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("malformed toml must error");
        assert!(err.to_string().contains(SETTINGS_FILE), "got {err}");
    }

    #[test]
    fn missing_settings_files_are_not_an_error() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        let gate = AgentGate::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(gate.is_enabled("claude"));
    }

    #[test]
    fn the_refusal_names_the_layer_that_disabled_the_agent() {
        // Operator file.
        let home = tempfile::tempdir().expect("tempdir");
        write_settings(home.path(), "[agents.codex]\nenabled = false\n");
        let repo = tempfile::tempdir().expect("tempdir");
        {
            let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            let empty = env_map(&[]);
            let gate = AgentGate::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
            let msg = gate.refusal("codex").expect("disabled");
            assert!(
                msg.contains(&home.path().display().to_string()),
                "got {msg}"
            );
            assert!(
                !msg.contains("may only disable"),
                "operator variant has no repo caveat: {msg}"
            );
        }

        // Repo file.
        let home2 = tempfile::tempdir().expect("tempdir");
        let repo2 = tempfile::tempdir().expect("tempdir");
        write_settings(repo2.path(), "[agents.codex]\nenabled = false\n");
        {
            let _guard = crate::commands::ctx::testenv::HomeGuard::set(home2.path());
            let empty = env_map(&[]);
            let gate = AgentGate::load(repo2.path(), &|k| empty.get(k).cloned()).expect("load");
            let msg = gate.refusal("codex").expect("disabled");
            assert!(
                msg.contains(&repo2.path().display().to_string()),
                "got {msg}"
            );
            assert!(
                msg.contains("a repository may only disable an agent, never enable one"),
                "got {msg}"
            );
        }

        // Environment.
        let home3 = tempfile::tempdir().expect("tempdir");
        let repo3 = tempfile::tempdir().expect("tempdir");
        {
            let _guard = crate::commands::ctx::testenv::HomeGuard::set(home3.path());
            let env = env_map(&[("ZIRV_AGENT_CODEX_ENABLED", "false")]);
            let gate = AgentGate::load(repo3.path(), &|k| env.get(k).cloned()).expect("load");
            let msg = gate.refusal("codex").expect("disabled");
            assert!(msg.contains("ZIRV_AGENT_CODEX_ENABLED"), "got {msg}");
        }
    }

    #[test]
    fn a_non_boolean_env_override_is_rejected_with_the_variable_named() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let env = env_map(&[("ZIRV_AGENT_CODEX_ENABLED", "sure")]);
        let err = AgentGate::load(repo.path(), &|k| env.get(k).cloned())
            .expect_err("a non-boolean override must be rejected");
        assert!(
            err.to_string().contains("ZIRV_AGENT_CODEX_ENABLED"),
            "got {err}"
        );
    }
}
