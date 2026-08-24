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
//! silent no-op: there is nothing to refuse). The home file can disable an
//! agent too -- that half is symmetric with the repo file -- but the
//! `&&` means a repo's `false` cannot be undone by the home file alone
//! (`true && false` is still `false`); only the environment sits above the
//! fold entirely and can re-enable an agent a repo disabled, or disable one
//! nothing else touched. The environment is the operator, in both
//! directions; the home file is the operator only in the narrowing
//! direction the fold actually gives it.
//!
//! `[agents.<name>]` also carries an optional `capacity` key (the only
//! recognized value is `"small"`; absent means full capacity), mirroring the
//! `enabled` gate's exact trust shape: `ZIRV_AGENT_<NAME>_CAPACITY` sits above
//! the fold and wins outright (it may set `"small"` or clear back to `"full"`
//! in either direction, same as the environment always can for `enabled`),
//! and otherwise the answer is `home(name) || repo(name)` -- a repo checkout
//! may narrow a full-capacity agent to `"small"`, but a repo file that is
//! silent on capacity can never undo a `"small"` the home file already set
//! (there is no `"full"` value to write; the only way to widen back is the
//! environment).

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
    pub capacity: Option<Capacity>,
}

/// The only file-settable capacity marker. There is deliberately no `"full"`
/// variant: absence already means full capacity (see `AgentSettings::
/// capacity`'s doc), and the only value a file needs to spell is the
/// narrowing one -- widening back to full is the environment's job alone
/// (see the module doc's fold).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capacity {
    Small,
}

impl<'de> Deserialize<'de> for Capacity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        if raw.eq_ignore_ascii_case("small") {
            Ok(Capacity::Small)
        } else {
            Err(serde::de::Error::custom(format!(
                "expected \"small\", got '{raw}'"
            )))
        }
    }
}

/// Where a `false` came from, for the refusal message and `zirv ctx status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    OperatorFile(PathBuf),
    RepoFile(PathBuf),
    Env(String),
    /// The gate itself could not be determined at all -- `AgentGate::
    /// load_operator_only`'s own fallback, used when even the reduced
    /// operator-only path errors (a malformed home file, a non-boolean env
    /// override). Every known adapter is denied rather than left permissive,
    /// so a broken settings surface fails closed. Carries a short reason for
    /// the refusal message and `zirv ctx status`.
    Unavailable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentState {
    pub enabled: bool,
    /// `None` whenever `enabled` is true: there is nothing to attribute a
    /// permissive default to.
    pub disabled_by: Option<Source>,
    /// Whether this agent is capacity-limited ("small tasks only") right
    /// now, folded with the same env-wins/home-or-repo-narrows shape as
    /// `enabled` (see the module doc). `false` is full capacity, the
    /// permissive default for an agent no layer mentions.
    pub capacity_small: bool,
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
            Some(Source::Unavailable(reason)) => format!("settings unreadable: {reason}"),
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

fn capacity_env_var_for(name: &str) -> String {
    format!("ZIRV_AGENT_{}_CAPACITY", name.to_uppercase())
}

fn known_adapter_names() -> Vec<&'static str> {
    crate::commands::ctx::adapters::all(None)
        .iter()
        .map(|a| a.name())
        .collect()
}

fn operator_settings_path() -> Option<PathBuf> {
    crate::utils::home_dir()
        .ok()
        .map(|home| home.join(crate::utils::SCRIPT_DIR_NAME).join(SETTINGS_FILE))
}

/// Writes `[agents.<name>] enabled = <bool>` into `<home>/.zirv/.settings.toml`,
/// creating the file and parent directories as needed. Reads and
/// re-serializes the whole file first, so this only ever adds/updates the one
/// `[agents.<name>]` table -- every other section, and every other agent's own
/// table, is left exactly as it was (though, like the pre-existing
/// `set_home_ctx_toml_bool` does for `ctx.toml`, round-tripping through
/// `toml::Table` drops any hand-written comments in the file). Written for
/// the first-run setup wizard (`commands::setup::run_first_run`), which only
/// calls this while the file is known not to exist yet
/// (`commands::setup::first_run_needed`), but it stays defensive so a later
/// re-run (`zirv setup`'s guided menu) merges rather than silently clobbers
/// an existing file's keys.
pub fn set_operator_agent_enabled(home: &Path, name: &str, enabled: bool) -> CtxResult<()> {
    let path = home.join(crate::utils::SCRIPT_DIR_NAME).join(SETTINGS_FILE);
    let mut root: toml::Table = if path.is_file() {
        toml::from_str(&std::fs::read_to_string(&path)?)?
    } else {
        toml::Table::new()
    };
    let agents = root
        .entry("agents".to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| format!("{}: `[agents]` is not a table", path.display()))?;
    let agent = agents
        .entry(name.to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| format!("{}: `[agents.{name}]` is not a table", path.display()))?;
    agent.insert("enabled".to_string(), toml::Value::Boolean(enabled));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, toml::to_string_pretty(&root)?)?;
    Ok(())
}

/// `Ok(None)` when the environment is silent on this agent; `Err` names the
/// variable when it is set but not a boolean.
fn env_override(name: &str, env: EnvLookup<'_>) -> CtxResult<Option<bool>> {
    let env_var = env_var_for(name);
    match env(&env_var) {
        Some(raw) => raw
            .parse::<bool>()
            .map(Some)
            .map_err(|_| format!("{env_var}: expected true or false, got '{raw}'").into()),
        None => Ok(None),
    }
}

fn file_enabled(layer: Option<&SettingsFile>, name: &str) -> Option<bool> {
    layer
        .and_then(|s| s.agents.get(name))
        .and_then(|a| a.enabled)
}

/// Whether `layer` marks `name` capacity-small. Unlike `file_enabled` there
/// is no tri-state to return: the only value a file can write is `"small"`,
/// so "not mentioned" and "explicitly full" are indistinguishable on disk,
/// and both mean `false` here.
fn file_capacity_small(layer: Option<&SettingsFile>, name: &str) -> bool {
    layer
        .and_then(|s| s.agents.get(name))
        .is_some_and(|a| a.capacity == Some(Capacity::Small))
}

/// `Ok(None)` when the environment is silent on this agent's capacity;
/// `Err` names the variable when it is set but neither `"small"` nor
/// `"full"`. Unlike `env_override` (a plain bool), this env var has real
/// two-way authority: `"full"` is how an operator clears a home- or
/// repo-narrowed agent back to full capacity, since no file can write that
/// value itself.
fn capacity_env_override(name: &str, env: EnvLookup<'_>) -> CtxResult<Option<bool>> {
    let env_var = capacity_env_var_for(name);
    match env(&env_var) {
        Some(raw) if raw.eq_ignore_ascii_case("small") => Ok(Some(true)),
        Some(raw) if raw.eq_ignore_ascii_case("full") => Ok(Some(false)),
        Some(raw) => Err(format!("{env_var}: expected 'small' or 'full', got '{raw}'").into()),
        None => Ok(None),
    }
}

impl AgentGate {
    /// Layers `~/.zirv/.settings.toml`, then `<repo>/.zirv/.settings.toml`,
    /// then `ZIRV_AGENT_<NAME>_ENABLED`, per agent name known to
    /// `adapters::all`. See the module doc for the exact fold.
    pub fn load(repo: &Path, env: EnvLookup<'_>) -> CtxResult<Self> {
        let known = known_adapter_names();

        let operator_path = operator_settings_path();
        let operator = match &operator_path {
            Some(path) => read_layer(path, &known)?,
            None => None,
        };

        let repo_path = repo.join(crate::utils::SCRIPT_DIR_NAME).join(SETTINGS_FILE);
        let repo_layer = read_layer(&repo_path, &known)?;

        let mut states = HashMap::new();
        for name in known {
            let capacity_small = match capacity_env_override(name, env)? {
                Some(small) => small,
                None => {
                    file_capacity_small(operator.as_ref(), name)
                        || file_capacity_small(repo_layer.as_ref(), name)
                }
            };

            let state = match env_override(name, env)? {
                Some(enabled) => AgentState {
                    enabled,
                    disabled_by: (!enabled).then(|| Source::Env(env_var_for(name))),
                    capacity_small,
                },
                None => {
                    let operator_component = file_enabled(operator.as_ref(), name).unwrap_or(true);
                    let repo_component = file_enabled(repo_layer.as_ref(), name).unwrap_or(true);
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
                        capacity_small,
                    }
                }
            };

            states.insert(name.to_string(), state);
        }

        Ok(Self { states })
    }

    /// Fallback for callers that must never fail outright (`optimize`'s and
    /// `hook`'s config-load degradation arms): the operator layers only --
    /// home file, then environment -- with the repo layer skipped entirely.
    /// A malformed *repo* `.settings.toml` must never be able to void an
    /// *operator* disable by taking the whole `CtxConfig::load` down with it
    /// and landing a caller on a permissive default.
    ///
    /// If even this reduced path errors, the result denies every known
    /// adapter rather than falling open: a settings surface zirv cannot read
    /// at all fails closed, never permissive. What "even this reduced path
    /// errors" means is scoped narrowly, though (see `load_operator_layers`):
    /// only a home *file* that cannot be read is genuinely global, because
    /// nothing else was even attempted. A single agent's malformed env
    /// override is not global -- it is that one agent's problem, handled per
    /// name inside `load_operator_layers` -- so it never reaches here.
    pub fn load_operator_only(env: EnvLookup<'_>) -> Self {
        let known = known_adapter_names();
        match Self::load_operator_layers(&known, env) {
            Ok(gate) => gate,
            Err(e) => Self::deny_all(&known, &e.to_string()),
        }
    }

    /// The only failure this propagates (`?`, hence `CtxResult`) is the home
    /// file itself being unreadable/malformed -- genuinely global, since it
    /// means nothing about *any* agent could be determined from it. A single
    /// agent's `env_override` error is deliberately NOT propagated with `?`:
    /// `ZIRV_AGENT_CODEX_ENABLED=1` (invalid) must deny only codex, not also
    /// claude, which has no reason to be unresolvable just because a
    /// different agent's variable is malformed.
    fn load_operator_layers(known: &[&'static str], env: EnvLookup<'_>) -> CtxResult<Self> {
        let operator_path = operator_settings_path();
        let operator = match &operator_path {
            Some(path) => read_layer(path, known)?,
            None => None,
        };

        let mut states = HashMap::new();
        for name in known {
            // Capacity errors are scoped the same way `enabled`'s own env
            // override errors are below: one agent's malformed
            // `ZIRV_AGENT_<NAME>_CAPACITY` denies only that agent, never the
            // whole operator-only fallback.
            let capacity_small = match capacity_env_override(name, env) {
                Ok(Some(small)) => small,
                Ok(None) => file_capacity_small(operator.as_ref(), name),
                Err(_) => false,
            };

            let state = match env_override(name, env) {
                Ok(Some(enabled)) => AgentState {
                    enabled,
                    disabled_by: (!enabled).then(|| Source::Env(env_var_for(name))),
                    capacity_small,
                },
                Ok(None) => {
                    let enabled = file_enabled(operator.as_ref(), name).unwrap_or(true);
                    let disabled_by = (!enabled)
                        .then(|| operator_path.clone().map(Source::OperatorFile))
                        .flatten();
                    AgentState {
                        enabled,
                        disabled_by,
                        capacity_small,
                    }
                }
                Err(e) => AgentState {
                    enabled: false,
                    disabled_by: Some(Source::Unavailable(e.to_string())),
                    capacity_small,
                },
            };
            states.insert((*name).to_string(), state);
        }
        Ok(Self { states })
    }

    fn deny_all(known: &[&'static str], reason: &str) -> Self {
        let states = known
            .iter()
            .map(|name| {
                (
                    (*name).to_string(),
                    AgentState {
                        enabled: false,
                        disabled_by: Some(Source::Unavailable(reason.to_string())),
                        capacity_small: false,
                    },
                )
            })
            .collect();
        Self { states }
    }

    /// Unknown names are enabled: this gate only ever narrows what its known
    /// adapters may do, never what a caller thinks a name means.
    pub fn is_enabled(&self, name: &str) -> bool {
        self.states.get(name).is_none_or(|s| s.enabled)
    }

    /// Whether `name` is capacity-limited ("small tasks only") right now.
    /// Unknown names are full capacity, the same permissive default
    /// `is_enabled` gives an unknown name.
    pub fn is_capacity_small(&self, name: &str) -> bool {
        self.states.get(name).is_some_and(|s| s.capacity_small)
    }

    /// `None` when the agent is enabled (or unknown); otherwise a message
    /// naming the cause and the remedy, matching the plain tone of
    /// `CodexAdapter::ready`.
    pub fn refusal(&self, name: &str) -> Option<String> {
        let state = self.states.get(name)?;
        if state.enabled {
            return None;
        }
        let env_var = env_var_for(name);
        if let Some(Source::Unavailable(reason)) = &state.disabled_by {
            // No "set VAR=true" suggestion here, deliberately: `reason` may
            // name a file (unreadable/malformed home file, where the
            // environment is never even consulted -- see
            // `load_operator_layers`), not this agent's own env var, and a
            // remedy naming the wrong thing to fix is worse than none.
            return Some(format!(
                "agent '{name}' cannot be verified enabled: its settings could not be read \
                 ({reason}); zirv will not launch or parse it until that is fixed. Pass a \
                 different --agent in the meantime."
            ));
        }
        let location = state.location();
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

    /// G: whether `name` is disabled, and disabled *only* by the repo layer
    /// -- neither the operator's own home file nor the environment had
    /// anything to say about it. `load`'s own fold already attributes
    /// `disabled_by` to `Source::OperatorFile`/`Source::Env` whenever either
    /// of those layers is the one that actually decided the disable (see the
    /// `load` doc comment), so this is a cheap read of state already
    /// computed, not a second pass over any settings file.
    ///
    /// This is the provenance `resolve_default`'s fallback (`adapters/
    /// mod.rs`) uses to tell "the repo narrowed this option away" from "an
    /// operator (or the environment) chose to disable it": only the former
    /// is untrusted-input territory, where a repo checkout silently forcing
    /// the fallback onto a *different* adapter -- a different vendor
    /// account, different supervision capabilities -- would let a hostile
    /// checkout select something rather than merely narrow what's on offer.
    pub fn disabled_only_by_repo(&self, name: &str) -> bool {
        matches!(
            self.states.get(name),
            Some(AgentState {
                enabled: false,
                disabled_by: Some(Source::RepoFile(_)),
                ..
            })
        )
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

    // -- set_operator_agent_enabled (the first-run wizard's settings writer) --

    #[test]
    fn set_operator_agent_enabled_writes_a_fresh_file_the_loader_can_read() {
        let home = tempfile::tempdir().expect("tempdir");

        set_operator_agent_enabled(home.path(), "codex", false).expect("write");

        let path = home.path().join(".zirv").join(SETTINGS_FILE);
        let settings: SettingsFile =
            toml::from_str(&std::fs::read_to_string(&path).expect("read back")).expect("parse");
        assert_eq!(
            settings.agents.get("codex").and_then(|a| a.enabled),
            Some(false)
        );
    }

    #[test]
    fn set_operator_agent_enabled_merges_without_clobbering_other_agents_or_keys() {
        let home = tempfile::tempdir().expect("tempdir");
        write_settings(
            home.path(),
            "[agents.claude]\nenabled = true\ncapacity = \"small\"\n",
        );

        set_operator_agent_enabled(home.path(), "codex", true).expect("write");

        let path = home.path().join(".zirv").join(SETTINGS_FILE);
        let settings: SettingsFile =
            toml::from_str(&std::fs::read_to_string(&path).expect("read back")).expect("parse");
        assert_eq!(
            settings.agents.get("codex").and_then(|a| a.enabled),
            Some(true)
        );
        assert_eq!(
            settings.agents.get("claude").and_then(|a| a.enabled),
            Some(true),
            "an unrelated agent's existing table must survive the merge"
        );
        assert_eq!(
            settings.agents.get("claude").and_then(|a| a.capacity),
            Some(Capacity::Small),
            "an unrelated key on the same agent's table must survive the merge"
        );
    }

    #[test]
    fn set_operator_agent_enabled_round_trips_through_the_real_loader() {
        let home = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("tempdir");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        set_operator_agent_enabled(home.path(), "codex", false).expect("write");

        let empty = env_map(&[]);
        let gate = AgentGate::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(!gate.is_enabled("codex"));
        assert!(gate.is_enabled("claude"), "only codex was disabled");
    }

    /// Review finding 3: without an isolated `HOME`, this read the
    /// developer's real `~/.zirv/.settings.toml` -- harmless on a clean
    /// machine, but a false pass (or a false failure on a machine that has
    /// one) either way.
    #[test]
    fn defaults_enable_every_known_adapter() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

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

    /// G: `disabled_only_by_repo` is the provenance `resolve_default`'s
    /// fallback reads to refuse silently landing on a different adapter --
    /// true only when the repo layer is the one that actually decided the
    /// disable, never merely "the repo file also happens to mention it."
    #[test]
    fn disabled_only_by_repo_is_true_when_only_the_repo_layer_disabled_it() {
        let repo = tempfile::tempdir().expect("tempdir");
        write_settings(repo.path(), "[agents.codex]\nenabled = false\n");
        let home = tempfile::tempdir().expect("tempdir");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        let gate = AgentGate::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(gate.disabled_only_by_repo("codex"));
        assert!(
            !gate.disabled_only_by_repo("claude"),
            "an enabled agent is not disabled by anything"
        );
    }

    /// G: an operator disable takes attribution priority over a repo one
    /// (`load`'s own fold), so when both layers disable the same agent this
    /// must read as operator-caused, not repo-only -- an operator's own
    /// choice is not the untrusted-input case this predicate exists for.
    #[test]
    fn disabled_only_by_repo_is_false_when_the_operator_also_disabled_it() {
        let home = tempfile::tempdir().expect("tempdir");
        write_settings(home.path(), "[agents.codex]\nenabled = false\n");
        let repo = tempfile::tempdir().expect("tempdir");
        write_settings(repo.path(), "[agents.codex]\nenabled = false\n");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        let gate = AgentGate::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(!gate.disabled_only_by_repo("codex"));
    }

    /// G: the environment is the operator too, and it can disable an agent
    /// the repo never mentioned at all -- also not the repo-only case.
    #[test]
    fn disabled_only_by_repo_is_false_for_an_environment_disable() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let env = env_map(&[("ZIRV_AGENT_CODEX_ENABLED", "false")]);
        let gate = AgentGate::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(!gate.disabled_only_by_repo("codex"));
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

    /// Review finding 1's core primitive: the operator-only fallback must
    /// see the home file and ignore the repo file entirely -- not merely
    /// "ignore a broken repo file", but never read it at all, since the
    /// whole point is to be usable when the repo layer already blew up
    /// `AgentGate::load`.
    #[test]
    fn load_operator_only_ignores_the_repo_layer_even_when_it_is_well_formed() {
        let home = tempfile::tempdir().expect("tempdir");
        write_settings(home.path(), "[agents.codex]\nenabled = false\n");
        let repo = tempfile::tempdir().expect("tempdir");
        write_settings(repo.path(), "[agents.claude]\nenabled = false\n");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        let gate = AgentGate::load_operator_only(&|k| empty.get(k).cloned());
        assert!(
            !gate.is_enabled("codex"),
            "the operator disable still holds"
        );
        assert!(
            gate.is_enabled("claude"),
            "the repo layer must never be consulted, not even read"
        );
    }

    #[test]
    fn load_operator_only_still_honors_the_environment() {
        let home = tempfile::tempdir().expect("tempdir");
        write_settings(home.path(), "[agents.codex]\nenabled = false\n");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let env = env_map(&[("ZIRV_AGENT_CODEX_ENABLED", "true")]);
        let gate = AgentGate::load_operator_only(&|k| env.get(k).cloned());
        assert!(
            gate.is_enabled("codex"),
            "the environment is still the operator"
        );
    }

    /// The deny-all degradation: if even the operator-only path cannot be
    /// read, every known adapter is refused rather than left permissive.
    #[test]
    fn load_operator_only_denies_every_known_adapter_when_it_cannot_be_read_either() {
        let home = tempfile::tempdir().expect("tempdir");
        write_settings(home.path(), "not [ valid toml");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        let gate = AgentGate::load_operator_only(&|k| empty.get(k).cloned());
        assert!(!gate.is_enabled("claude"), "fail closed, not open");
        assert!(!gate.is_enabled("codex"), "fail closed, not open");
        let msg = gate.refusal("claude").expect("must be refused");
        assert!(
            msg.to_lowercase().contains("could not be read")
                || msg.to_lowercase().contains("unreadable"),
            "got {msg}"
        );
    }

    /// Review finding NEW-1: `env_override` erroring for one agent must not
    /// deny every agent -- `ZIRV_AGENT_CODEX_ENABLED=1` (invalid) is codex's
    /// problem alone, and claude's fold (no file, no env override) must
    /// still resolve to its ordinary permissive default.
    #[test]
    fn an_invalid_env_override_denies_only_the_agent_it_names() {
        let home = tempfile::tempdir().expect("tempdir");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let env = env_map(&[("ZIRV_AGENT_CODEX_ENABLED", "1")]);
        let gate = AgentGate::load_operator_only(&|k| env.get(k).cloned());

        assert!(
            gate.is_enabled("claude"),
            "an unrelated agent's bad env var must not deny claude"
        );
        assert!(!gate.is_enabled("codex"));
        let msg = gate.refusal("codex").expect("codex must be refused");
        assert!(
            msg.contains("ZIRV_AGENT_CODEX_ENABLED"),
            "the reason must name the actual bad variable: {msg}"
        );
        assert!(
            !msg.contains("ZIRV_AGENT_CLAUDE_ENABLED"),
            "the remedy must not suggest fixing an unrelated agent's variable: {msg}"
        );
    }

    /// The same scoping applies to `AgentGate::load` (not just the
    /// operator-only fallback): a bad env var still fails the whole
    /// `CtxConfig::load` (existing, deliberate -- `a_non_boolean_env_
    /// override_is_rejected_with_the_variable_named` above pins that), but
    /// `refusal`'s wording for the *resulting* `Unavailable` state (reached
    /// only through `load_operator_only`) must never suggest a remedy for
    /// the wrong agent. This is a direct regression guard on the message
    /// itself, independent of which path produced the `Unavailable` state.
    #[test]
    fn the_unavailable_refusal_never_suggests_setting_an_unrelated_variable() {
        let home = tempfile::tempdir().expect("tempdir");
        write_settings(home.path(), "not [ valid toml");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        let gate = AgentGate::load_operator_only(&|k| empty.get(k).cloned());
        let msg = gate.refusal("claude").expect("must be refused");
        assert!(
            !msg.contains("Set ZIRV_AGENT_CLAUDE_ENABLED=true"),
            "the whole-file-unreadable case never even consults env, so this remedy is a dead \
             end: {msg}"
        );
    }

    // -- capacity ("small tasks only") --------------------------------

    #[test]
    fn defaults_are_full_capacity() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        let gate = AgentGate::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(!gate.is_capacity_small("claude"));
        assert!(!gate.is_capacity_small("codex"));
    }

    #[test]
    fn a_home_file_can_mark_an_agent_capacity_small() {
        let home = tempfile::tempdir().expect("tempdir");
        write_settings(home.path(), "[agents.codex]\ncapacity = \"small\"\n");
        let repo = tempfile::tempdir().expect("tempdir");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        let gate = AgentGate::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(gate.is_capacity_small("codex"));
        assert!(!gate.is_capacity_small("claude"), "only codex was marked");
    }

    #[test]
    fn a_repository_can_mark_an_agent_capacity_small() {
        let repo = tempfile::tempdir().expect("tempdir");
        write_settings(repo.path(), "[agents.codex]\ncapacity = \"small\"\n");
        let home = tempfile::tempdir().expect("tempdir");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        let gate = AgentGate::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(gate.is_capacity_small("codex"));
    }

    /// The repo-cannot-widen half of the fold: a repo file that says nothing
    /// about capacity must not undo a home-set `"small"` -- there is no
    /// `"full"` value a file can write at all, so the only way this could go
    /// wrong is the fold silently treating "repo is silent" as "repo says
    /// full". It must not.
    #[test]
    fn a_repository_cannot_widen_a_home_narrowed_agent_back_to_full() {
        let home = tempfile::tempdir().expect("tempdir");
        write_settings(home.path(), "[agents.codex]\ncapacity = \"small\"\n");
        let repo = tempfile::tempdir().expect("tempdir");
        // The repo file exists and mentions this agent, but says nothing
        // about capacity -- still must not clear the home-set narrowing.
        write_settings(repo.path(), "[agents.codex]\nenabled = true\n");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        let gate = AgentGate::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(
            gate.is_capacity_small("codex"),
            "a repo silent on capacity must not widen a home-set narrowing"
        );
    }

    /// The environment is the operator in both directions for capacity too:
    /// it can narrow an agent nothing else marked, and it can widen one a
    /// file narrowed back to full -- the one case no file can express on its
    /// own.
    #[test]
    fn the_environment_can_narrow_or_widen_capacity_in_either_direction() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let narrow = env_map(&[("ZIRV_AGENT_CODEX_CAPACITY", "small")]);
        let gate = AgentGate::load(repo.path(), &|k| narrow.get(k).cloned()).expect("load");
        assert!(gate.is_capacity_small("codex"), "env can narrow");

        write_settings(home.path(), "[agents.codex]\ncapacity = \"small\"\n");
        let widen = env_map(&[("ZIRV_AGENT_CODEX_CAPACITY", "full")]);
        let gate = AgentGate::load(repo.path(), &|k| widen.get(k).cloned()).expect("load");
        assert!(
            !gate.is_capacity_small("codex"),
            "env can widen back to full even over a home-set 'small'"
        );
    }

    #[test]
    fn an_invalid_capacity_env_override_is_rejected_with_the_variable_named() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let env = env_map(&[("ZIRV_AGENT_CODEX_CAPACITY", "medium")]);
        let err = AgentGate::load(repo.path(), &|k| env.get(k).cloned())
            .expect_err("an invalid capacity override must be rejected");
        assert!(
            err.to_string().contains("ZIRV_AGENT_CODEX_CAPACITY"),
            "got {err}"
        );
    }

    #[test]
    fn an_invalid_capacity_file_value_is_rejected_loudly() {
        let repo = tempfile::tempdir().expect("tempdir");
        write_settings(repo.path(), "[agents.codex]\ncapacity = \"medium\"\n");
        let home = tempfile::tempdir().expect("tempdir");
        let _guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        let err = AgentGate::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("only 'small' is a valid capacity value");
        assert!(err.to_string().contains("small"), "got {err}");
    }
}
