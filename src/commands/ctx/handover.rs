//! `zirv ctx handover`: swap the orchestrator seat's model or harness in
//! place, carrying a handoff packet across the swap while the session keeps
//! its registry short id (issue #84).
//!
//! This CLI process never touches the wrapped agent's pty itself -- only the
//! live `wrap` supervisor owns that (or, inside the dashboard, the pane the
//! operator picked). It writes a small request file next to the session's
//! registry record (`<state>/sessions/<short>.handover-req`), the same
//! request/claim/ack shape `dash/spawnreq.rs` already uses for cross-process
//! delegation, and polls for the ack `wrap`'s pump loop writes back once it
//! has decided (swap performed, or refused). `wrap`'s own tick already knows
//! whether the session is at a verified-idle turn boundary (`wrap::
//! may_inject`) -- reusing that seam is what "quiesce" means here, rather
//! than this process trying to infer liveness state it has no access to.
//!
//! **Tier resolution.** Each generic tier resolves from a small built-in
//! ladder per adapter, overridable per operator via `ctx.toml`'s
//! `[handover.<agent>]` table (`config.rs`'s `HandoverConfig`) or, with the
//! final word, `ZIRV_CTX_HANDOVER_<AGENT>_<TIER>` -- the same "operator env
//! always wins over config, which always wins over the built-in default"
//! shape every other model choice in this codebase already follows. Both the
//! table and the env vars are `REPO_FORBIDDEN` (`config.rs`'s `REPO_FORBIDDEN`
//! entry for `["handover"]`): swapping the orchestrator seat's harness or
//! model is picking which vendor account gets spent, the same trust asymmetry
//! as `agent`/`review.*`/`worker.*`, so only the operator's own
//! `~/.zirv/ctx.toml`, `ZIRV_CTX_*` or flags may set it -- never a repo
//! checkout. A literal model id (anything not one of `cheap`/`standard`/
//! `deep`) always passes through unresolved.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::state::StateDir;
use super::{CtxResult, adapters, handoff, sessions};

/// The three generic tiers `--model`/the dashboard picker accept, resolved
/// per harness. Anything else is treated as a literal model id.
pub const TIERS: [&str; 3] = ["cheap", "standard", "deep"];

fn tier_default(agent: &str, tier: &str) -> Option<&'static str> {
    match (agent, tier) {
        ("claude", "cheap") => Some("haiku"),
        ("claude", "standard") => Some("sonnet"),
        ("claude", "deep") => Some("opus"),
        ("codex", "cheap") => Some("gpt-5.4-mini"),
        ("codex", "standard") => Some("gpt-5.6-terra"),
        ("codex", "deep") => Some("gpt-5.6-sol"),
        _ => None,
    }
}

/// The operator's own `[handover.<agent>]` override for `tier`, if any --
/// already folded from `ctx.toml` and (with the final word) the matching
/// `ZIRV_CTX_HANDOVER_<AGENT>_<TIER>` by `CtxConfig::load`'s ordinary
/// env-over-merged-layers precedence (see `ENV_MAP` in `config.rs`), so this
/// function needs no env lookup of its own.
fn handover_config_tier<'a>(cfg: &'a CtxConfig, agent: &str, tier: &str) -> Option<&'a str> {
    let agent_cfg = match agent {
        "claude" => &cfg.handover.claude,
        "codex" => &cfg.handover.codex,
        _ => return None,
    };
    match tier {
        "cheap" => agent_cfg.cheap.as_deref(),
        "standard" => agent_cfg.standard.as_deref(),
        "deep" => agent_cfg.deep.as_deref(),
        _ => None,
    }
}

/// Resolves `requested` (a generic tier, or a literal model id) for `agent`.
/// The operator's own configured override (`cfg.handover`, itself already
/// env-overridden -- see `handover_config_tier`) wins over the built-in
/// ladder; a value that is not one of `TIERS` at all (a literal model id)
/// always passes through verbatim, unresolved.
///
/// Finding #6: a *tier word* (`cheap`/`standard`/`deep`) for an agent with
/// no configured override and no built-in ladder entry used to fall through
/// to `requested` verbatim too -- silently handing the literal string
/// `"deep"` to a launch argv as if it were a real model id, for any adapter
/// this ladder does not (yet) know. That is now an error naming the agent,
/// so the caller finds out at resolution time rather than watching a launch
/// fail on a model id nobody actually meant.
pub fn resolve_model(agent: &str, requested: &str, cfg: &CtxConfig) -> CtxResult<String> {
    let tier = requested.trim().to_ascii_lowercase();
    if TIERS.contains(&tier.as_str()) {
        if let Some(v) = handover_config_tier(cfg, agent, &tier).filter(|s| !s.trim().is_empty()) {
            return Ok(v.trim().to_string());
        }
        if let Some(v) = tier_default(agent, &tier) {
            return Ok(v.to_string());
        }
        return Err(format!(
            "zirv ctx handover: no tier ladder for adapter '{agent}'; pass a literal model id \
             instead of '{tier}', or configure one under [handover.{agent}] in ~/.zirv/ctx.toml"
        )
        .into());
    }
    Ok(requested.trim().to_string())
}

#[derive(Debug, clap::Args)]
pub struct HandoverArgs {
    /// Target model: a literal model id, or a generic tier (cheap/standard/deep)
    /// resolved for the target harness. Omit to keep the target harness's own
    /// default.
    #[arg(long)]
    pub model: Option<String>,
    /// Target harness/adapter name. Defaults to this session's current agent
    /// (a same-harness model swap).
    #[arg(long)]
    pub agent: Option<String>,
    /// Print the handoff packet and the resolved swap; changes nothing.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    /// Swap even mid-turn, interrupting whatever the session is doing.
    #[arg(long, default_value_t = false)]
    pub force: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoverRequest {
    pub target_agent: String,
    pub target_model: Option<String>,
    pub force: bool,
    pub requested_at: u64,
    /// Whether the REQUESTER can vouch that a human is present to answer an
    /// `Ask` prompt on the fresh successor session this swap launches
    /// (2026-08-24, cross-harness permissions hardening) -- true only for a
    /// swap a human directly triggered from the dashboard's own live pane.
    /// `#[serde(default)]` makes `false` (`Headless`, fail-closed) what an
    /// older request, or one this module cannot otherwise vouch for,
    /// deserialises to. `resolve_swap_launch` is what actually reads it.
    #[serde(default)]
    pub interactive: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HandoverAck {
    pub ok: bool,
    pub reason: Option<String>,
    pub from_agent: Option<String>,
    pub from_model: Option<String>,
    pub to_agent: Option<String>,
    pub to_model: Option<String>,
    pub stored: Option<String>,
}

fn request_path(state: &StateDir, short: &str) -> PathBuf {
    state.sessions().join(format!("{short}.handover-req"))
}

fn ack_path(state: &StateDir, short: &str) -> PathBuf {
    state.sessions().join(format!("{short}.handover-ack"))
}

/// Writes the request, best-effort directory creation matching every other
/// piece of state-dir housekeeping in this codebase.
pub fn write_request(state: &StateDir, short: &str, req: &HandoverRequest) -> CtxResult<()> {
    let _ = super::state::create_private_dir_all(&state.sessions());
    let json = serde_json::to_string(req)?;
    super::state::write_private(&request_path(state, short), &json)?;
    Ok(())
}

/// Atomically claims (removes) the pending request for `short`, returning it
/// when one was present. `std::fs::remove_file` is the claim -- the same
/// idiom `sessions::claim_nudge_marker` already uses -- so a request is acted
/// on by at most one observer. Both a real swap and a refusal claim the
/// request: leaving it behind on refusal would let a later, unrelated tick
/// silently retry a decision the operator already saw fail.
pub fn take_request(state: &StateDir, short: &str) -> Option<HandoverRequest> {
    let path = request_path(state, short);
    let contents = std::fs::read_to_string(&path).ok()?;
    std::fs::remove_file(&path).ok()?;
    serde_json::from_str(&contents).ok()
}

/// Best-effort: an ack that never lands just means the requester's own poll
/// times out and reports failure honestly, never that the swap itself (or
/// the refusal) did not happen.
pub fn write_ack(state: &StateDir, short: &str, ack: &HandoverAck) {
    let _ = super::state::create_private_dir_all(&state.sessions());
    if let Ok(json) = serde_json::to_string(ack) {
        let _ = super::state::write_private(&ack_path(state, short), &json);
    }
}

fn take_ack(state: &StateDir, short: &str) -> Option<HandoverAck> {
    let path = ack_path(state, short);
    let contents = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    serde_json::from_str(&contents).ok()
}

/// The resolved successor adapter plus the extra argv `AgentAdapter::
/// interactive_cmd`'s own `extra` parameter should carry: the shipped
/// sandbox/policy posture plus (when `req.target_model` names one) the model
/// flag. Shared by every live-swap seam (`wrap::perform_handover_swap`,
/// `dash::pane::Pane::handover`) so the two can never drift on what a swap's
/// fresh launch actually carries.
pub fn resolve_swap_launch(
    cfg: &CtxConfig,
    req: &HandoverRequest,
) -> CtxResult<(Box<dyn adapters::AgentAdapter>, Vec<String>)> {
    let new_adapter = adapters::select(Some(&req.target_agent), &[], cfg)?;
    let mut extra = adapters::policy_launch_args(
        cfg,
        new_adapter.as_ref(),
        &[],
        if req.interactive {
            adapters::LaunchMode::Interactive
        } else {
            adapters::LaunchMode::Headless
        },
    );
    if let Some(model) = &req.target_model {
        extra.extend(new_adapter.model_args(model));
    }
    Ok((new_adapter, extra))
}

/// The environment a swap's fresh child needs to carry its identity
/// correctly: the new adapter's own turn-signal env (against the
/// *unchanged* socket, since the session id -- and so the socket path --
/// never moves across a handover), `AGENT_ENV` naming the new harness, and
/// (for an `Orchestrator` launch) `SEAT_MODEL_ENV` naming the resolved
/// target model. Shared by every live-swap seam for the same reason
/// `resolve_swap_launch` is.
pub fn build_turn_env(
    new_adapter: &dyn adapters::AgentAdapter,
    server: Option<&super::signal::SignalServer>,
    session_id: &str,
    repo: &Path,
    role: super::prompt::PromptRole,
    target_model: Option<&str>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = server
        .map(|server| {
            new_adapter
                .register_turn_signal(
                    &super::event::SessionRef {
                        id: super::event::SessionId::parse(session_id),
                        cwd: repo.to_path_buf(),
                    },
                    server.path(),
                )
                .env
        })
        .unwrap_or_default();
    env.push((
        adapters::AGENT_ENV.to_string(),
        new_adapter.name().to_string(),
    ));
    // Finding #3 (mirrors `dash::build_turn_env`'s identical patch): a
    // successor adapter with no turn-signal mechanism at all (codex today)
    // returns an empty `env` from `register_turn_signal` above, which used
    // to leave `SESSION_ENV` entirely unset for a claude->codex swap. A
    // turn-signal-capable adapter (claude) already sets this as part of its
    // own `setup.env`, so it is added here only when not already present.
    if !env.iter().any(|(k, _)| k == adapters::SESSION_ENV) {
        env.push((adapters::SESSION_ENV.to_string(), session_id.to_string()));
    }
    env.extend(adapters::seat_model_env(role, &[], target_model));
    env
}

/// How long the CLI waits for a live supervisor to answer. Generous enough
/// to cover a real distiller model call (`cfg.handoff.timeout_secs`) plus the
/// quit/relaunch overhead around it, bounded so a session with nothing
/// listening still reports a clear failure instead of hanging.
fn ack_timeout(cfg: &CtxConfig) -> Duration {
    Duration::from_secs(cfg.handoff.timeout_secs.max(20) + 30)
}

/// The markdown packet plus a short header, for `--dry-run` and for the
/// dashboard picker's own preview. Never mutates anything: it distills
/// read-only off whatever the session has already written to its transcript.
fn preview_packet<W: Write>(
    w: &mut W,
    cfg: &CtxConfig,
    current_agent: &str,
    target_agent: &str,
    target_model: Option<&str>,
    transcript: &Path,
) -> CtxResult<()> {
    let current_adapter = adapters::select(Some(current_agent), &[], cfg)?;
    let jsonl = std::fs::read_to_string(transcript).unwrap_or_default();
    let ctx = current_adapter.structural_context(&jsonl, cfg.handoff.tail_items);
    let (packet, source) = handoff::distill_or_structural(
        current_adapter.as_ref(),
        &handoff::resolve_distiller_model(cfg.handoff.model.as_deref(), current_adapter.as_ref()),
        &ctx,
        Duration::from_secs(cfg.handoff.timeout_secs),
        cfg.chrome.events,
    );
    writeln!(w, "# zirv ctx handover --dry-run")?;
    writeln!(
        w,
        "from: {current_agent} -> to: {target_agent} ({})",
        target_model.unwrap_or("harness default")
    )?;
    writeln!(w, "packet source: {source}\n")?;
    write!(w, "{}", packet.to_markdown())?;
    Ok(())
}

pub fn run_with<W: Write>(
    args: &HandoverArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load_for_launch(repo, env)?;
    let state = StateDir::resolve(env)?;

    let session_id = env(super::adapters::SESSION_ENV)
        .filter(|s| !s.trim().is_empty())
        .ok_or(
            "zirv ctx handover: no session identity in the environment; run this from inside a \
             session zirv is already supervising (`zirv ctx wrap`/`zirv ctx chat`)",
        )?;
    let short = sessions::short_id(&session_id);

    let record =
        sessions::resolve_prefix(&state, &short).map_err(|e| format!("zirv ctx handover: {e}"))?;
    if !matches!(record.verb, sessions::Verb::Wrap | sessions::Verb::Chat) {
        return Err(format!(
            "zirv ctx handover: session {short} is a {} session, not an interactive orchestrator \
             seat; handover only swaps an interactive session (wrap/chat)",
            record.verb
        )
        .into());
    }

    let current_agent = record.agent.clone();
    let target_agent = args.agent.clone().unwrap_or_else(|| current_agent.clone());
    // Validate the target adapter resolves at all before anything else: a
    // typo in --agent should fail loudly here, not inside wrap's own tick,
    // where the only visible effect would be an unexplained refusal.
    adapters::select(Some(&target_agent), &[], &cfg)?;
    let target_model = args
        .model
        .as_deref()
        .map(|m| resolve_model(&target_agent, m, &cfg))
        .transpose()?;

    if args.dry_run {
        let transcript_path = env(super::wrap::TRANSCRIPT_ENV).map(PathBuf::from).ok_or(
            "zirv ctx handover --dry-run: no transcript path in the environment yet (the \
                 session has not reported a turn boundary); nothing to preview",
        )?;
        preview_packet(
            w,
            &cfg,
            &current_agent,
            &target_agent,
            target_model.as_deref(),
            &transcript_path,
        )?;
        return Ok(0);
    }

    let req = HandoverRequest {
        target_agent: target_agent.clone(),
        target_model: target_model.clone(),
        force: args.force,
        requested_at: super::state::now_secs(),
        // Real signal, not an assumption: this CLI command can be typed by
        // a human directly in the wrapped session's own terminal, or run
        // headlessly/scripted -- `is_terminal()` on this process's own
        // stdio is the same check `wrap.rs`'s launch-time gate already uses
        // to answer the identical question (2026-08-24 hardening).
        interactive: std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
    };
    write_request(&state, &short, &req)?;

    // wrap's pump loop ticks on its ordinary ~100ms cadence and checks for a
    // pending request every tick, so a short poll here is not a busy loop --
    // it is just waiting out the same cadence from the outside.
    let deadline = Instant::now() + ack_timeout(&cfg);
    loop {
        if let Some(ack) = take_ack(&state, &short) {
            if ack.ok {
                writeln!(
                    w,
                    "zirv ctx handover: {} ({}) -> {} ({}); handoff stored at {}",
                    ack.from_agent.unwrap_or_default(),
                    ack.from_model.unwrap_or_else(|| "default".to_string()),
                    ack.to_agent.unwrap_or_default(),
                    ack.to_model.unwrap_or_else(|| "default".to_string()),
                    ack.stored.unwrap_or_default(),
                )?;
                return Ok(0);
            }
            let reason = ack.reason.unwrap_or_else(|| "refused".to_string());
            return Err(format!("zirv ctx handover: {reason}").into());
        }
        if Instant::now() >= deadline {
            // Withdraw: best-effort, since a supervisor may have already
            // claimed it in the tiny window before this runs -- in which
            // case the swap (or refusal) still happens, this call simply
            // cannot report it.
            let _ = take_request(&state, &short);
            return Err(format!(
                "zirv ctx handover: no running supervisor for session {short} answered within \
                 {}s; is this session actually running under `zirv ctx wrap`/`zirv ctx chat`?",
                ack_timeout(&cfg).as_secs()
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn run<W: Write>(args: &HandoverArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &repo, &env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::sessions::{Record, SessionGuard, Verb};
    use crate::commands::ctx::state::STATE_ENV;
    use crate::commands::ctx::testenv;

    fn state_in(root: &Path) -> StateDir {
        StateDir::from_root(root.to_path_buf())
    }

    /// Finding #3: a claude->codex swap must still carry `SESSION_ENV` even
    /// though codex's `register_turn_signal` returns an empty `env` (it has
    /// no turn-signal mechanism at all) -- `scrub_supervision_env` strips the
    /// old session's env from the fresh child's inherited environment, so
    /// without this the successor has no session identity of its own at all.
    #[test]
    fn build_turn_env_sets_session_env_for_an_adapter_with_no_turn_signal_mechanism() {
        let codex = crate::commands::ctx::adapters::codex::CodexAdapter::new(None);
        let repo = tempfile::tempdir().expect("repo");
        let state_root = tempfile::tempdir().expect("state");
        let state = state_in(state_root.path());
        let session_id = "abcdef12-3456-4789-8abc-def012345678";
        // A real, bound server -- so `register_turn_signal` is actually
        // called (not short-circuited by `server: None`) and this test
        // genuinely exercises codex's own empty-`env` return, not merely the
        // no-server branch.
        let server =
            crate::commands::ctx::signal::SignalServer::bind(&state.socket_for(session_id))
                .expect("bind signal server");

        let env = build_turn_env(
            &codex,
            Some(&server),
            session_id,
            repo.path(),
            crate::commands::ctx::prompt::PromptRole::Worker,
            None,
        );

        assert!(
            env.iter()
                .any(|(k, v)| k == adapters::SESSION_ENV && v == session_id),
            "SESSION_ENV must be set even for an adapter with no turn-signal mechanism: {env:?}"
        );
    }

    #[test]
    fn resolve_model_prefers_the_operators_env_override_over_config() {
        // Env wins even over an explicit `[handover]` config value -- the
        // same "environment is the final word" precedence every other model
        // key in `config.rs` follows.
        let home = tempfile::tempdir().expect("home");
        let _home = testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[handover.claude]\ndeep = \"opus-from-config\"\n",
        )
        .expect("write home ctx.toml");
        let env: std::collections::HashMap<String, String> = [(
            "ZIRV_CTX_HANDOVER_CLAUDE_DEEP".to_string(),
            "opus-4-1".to_string(),
        )]
        .into();
        let repo = tempfile::tempdir().expect("repo");
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(resolve_model("claude", "deep", &cfg).unwrap(), "opus-4-1");
    }

    #[test]
    fn resolve_model_prefers_the_operators_home_config_over_the_built_in_ladder() {
        let home = tempfile::tempdir().expect("home");
        let _home = testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[handover.codex]\nstandard = \"gpt-6-custom\"\n",
        )
        .expect("write home ctx.toml");
        let empty: std::collections::HashMap<String, String> = Default::default();
        let repo = tempfile::tempdir().expect("repo");
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(
            resolve_model("codex", "standard", &cfg).unwrap(),
            "gpt-6-custom"
        );
        // An unconfigured tier on the same agent still falls through to the
        // built-in ladder.
        assert_eq!(
            resolve_model("codex", "cheap", &cfg).unwrap(),
            "gpt-5.4-mini"
        );
    }

    /// A repo `ctx.toml` setting `[handover]` at all is a `REPO_FORBIDDEN`
    /// rejection -- swapping the orchestrator seat's harness/model is picking
    /// which vendor account gets spent, the same asymmetry as
    /// `agent`/`review.*`/`worker.*`.
    #[test]
    fn a_repo_ctx_toml_may_not_set_handover() {
        let home = tempfile::tempdir().expect("home");
        let _home = testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[handover.claude]\ndeep = \"opus-from-repo\"\n",
        )
        .expect("write repo ctx.toml");
        let empty: std::collections::HashMap<String, String> = Default::default();
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("repo may not set handover");
        assert!(
            crate::commands::ctx::config::is_repo_forbidden(err.as_ref()),
            "must be a REPO_FORBIDDEN rejection: {err}"
        );
        assert!(err.to_string().contains("handover"), "got {err}");
    }

    #[test]
    fn resolve_model_falls_back_to_the_built_in_ladder() {
        let cfg = CtxConfig::default();
        assert_eq!(resolve_model("claude", "cheap", &cfg).unwrap(), "haiku");
        assert_eq!(resolve_model("claude", "standard", &cfg).unwrap(), "sonnet");
        assert_eq!(resolve_model("claude", "deep", &cfg).unwrap(), "opus");
        assert_eq!(
            resolve_model("codex", "cheap", &cfg).unwrap(),
            "gpt-5.4-mini"
        );
        assert_eq!(
            resolve_model("codex", "standard", &cfg).unwrap(),
            "gpt-5.6-terra"
        );
        assert_eq!(resolve_model("codex", "deep", &cfg).unwrap(), "gpt-5.6-sol");
    }

    #[test]
    fn resolve_model_passes_a_literal_model_id_through_unresolved() {
        let cfg = CtxConfig::default();
        assert_eq!(
            resolve_model("claude", "claude-opus-4-5", &cfg).unwrap(),
            "claude-opus-4-5"
        );
        assert_eq!(
            resolve_model("codex", "gpt-5.6-terra", &cfg).unwrap(),
            "gpt-5.6-terra"
        );
    }

    /// Finding #6: a tier word for an adapter with no built-in ladder entry
    /// and no configured override must be a loud error, not a literal tier
    /// string silently handed to a launch argv as if it were a real model
    /// id.
    #[test]
    fn resolve_model_errors_for_a_tier_word_with_no_ladder_for_the_adapter() {
        let cfg = CtxConfig::default();
        let err = resolve_model("some-third-adapter", "deep", &cfg)
            .expect_err("no tier ladder exists for this adapter");
        let msg = err.to_string();
        assert!(msg.contains("some-third-adapter"), "names the agent: {msg}");
        assert!(msg.contains("deep"), "names the tier: {msg}");
        assert!(
            msg.contains("literal model id"),
            "says what to pass instead: {msg}"
        );
    }

    /// A literal model id for the same unknown adapter must still pass
    /// through unresolved -- only a *tier word* triggers the new refusal.
    #[test]
    fn resolve_model_still_passes_a_literal_model_id_through_for_an_unknown_adapter() {
        let cfg = CtxConfig::default();
        assert_eq!(
            resolve_model("some-third-adapter", "some-literal-model", &cfg).unwrap(),
            "some-literal-model"
        );
    }

    #[test]
    fn take_request_is_a_one_shot_atomic_claim() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let req = HandoverRequest {
            target_agent: "codex".to_string(),
            target_model: Some("gpt-5.6-terra".to_string()),
            force: false,
            requested_at: 1,
            interactive: false,
        };
        write_request(&state, "abcd1234", &req).expect("write");
        let claimed = take_request(&state, "abcd1234").expect("present");
        assert_eq!(claimed.target_agent, "codex");
        assert!(
            take_request(&state, "abcd1234").is_none(),
            "claimed once only"
        );
    }

    #[test]
    fn write_ack_round_trips() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let ack = HandoverAck {
            ok: true,
            reason: None,
            from_agent: Some("claude".to_string()),
            from_model: Some("sonnet".to_string()),
            to_agent: Some("codex".to_string()),
            to_model: Some("gpt-5.6-terra".to_string()),
            stored: Some("/tmp/h.md".to_string()),
        };
        write_ack(&state, "abcd1234", &ack);
        let back = take_ack(&state, "abcd1234").expect("present");
        assert!(back.ok);
        assert_eq!(back.to_agent.as_deref(), Some("codex"));
        assert!(take_ack(&state, "abcd1234").is_none(), "consumed once only");
    }

    /// Finding 10 (2026-08-24 review): `resolve_swap_launch` used to
    /// hardcode `LaunchMode::Interactive` regardless of the requesting
    /// `HandoverRequest`'s own `interactive` field, so a swap this module
    /// cannot vouch a human is watching (`interactive: false`, what
    /// `#[serde(default)]` gives an older request too) got the permissive
    /// interactive posture instead of failing closed. Claude's own
    /// `default_sandbox_args` is independently verified to use
    /// `--permission-mode dontAsk` under `Headless`, so that flag's value
    /// is the observable signal here.
    #[test]
    fn resolve_swap_launch_fails_closed_to_headless_for_a_non_interactive_request() {
        let cfg = CtxConfig::default();
        let req = HandoverRequest {
            target_agent: "claude".to_string(),
            target_model: None,
            force: false,
            requested_at: 0,
            interactive: false,
        };
        let (_, extra) = resolve_swap_launch(&cfg, &req).expect("resolves");
        assert!(
            extra.contains(&"--permission-mode".to_string())
                && extra.contains(&"dontAsk".to_string()),
            "a non-interactive handover request must not get the permissive interactive posture: got {extra:?}"
        );
    }

    /// A `--dry-run` invocation must provably mutate nothing: no request or
    /// ack file, and no change anywhere else under the state dir -- the same
    /// style `zirv ctx optimize`'s own report-only test uses.
    #[test]
    fn dry_run_previews_the_packet_and_mutates_nothing() {
        let tmp = testenv::repo();
        // T8 hermeticity: this test's `--agent codex` dry-run reaches
        // `AgentGate::load` (real-`$HOME`-backed), same as the identical
        // fix on handoff.rs's `no_model_on_codex_now_reports_structural_
        // since_codex_has_real_structural_context` -- without it, codex
        // being disabled in a developer's own `~/.zirv/.settings.toml`
        // fails this test on an unrelated refusal.
        let home = tmp.path().join("home");
        let _home = testenv::HomeGuard::set(&home);
        let state = state_in(&tmp.path().join("state"));
        let transcript = tmp.path().join("t.jsonl");
        std::fs::write(
            &transcript,
            "{\"type\":\"user\",\"message\":{\"content\":\"ship the thing\"}}\n",
        )
        .expect("write transcript");

        let repo = tmp.path();
        let record = Record::new(
            "11111111-2222-4333-8444-555555555555",
            "claude",
            repo,
            Verb::Wrap,
        );
        let guard = SessionGuard::register(&state, record);

        let env: std::collections::HashMap<String, String> = [
            (STATE_ENV.to_string(), state.root().display().to_string()),
            (
                crate::commands::ctx::adapters::SESSION_ENV.to_string(),
                "11111111-2222-4333-8444-555555555555".to_string(),
            ),
            (
                crate::commands::ctx::wrap::TRANSCRIPT_ENV.to_string(),
                transcript.display().to_string(),
            ),
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                "/nonexistent/agent-must-never-launch".to_string(),
            ),
        ]
        .into();

        let before = snapshot(state.root());

        let args = HandoverArgs {
            model: Some("deep".to_string()),
            agent: Some("codex".to_string()),
            dry_run: true,
            force: false,
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, repo, &|k| env.get(k).cloned()).expect("dry-run runs");
        assert_eq!(code, 0);
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("--dry-run"), "got {text}");
        assert!(text.contains("claude -> to: codex"), "got {text}");
        assert!(text.contains("## Task"), "packet included: {text}");

        let after = snapshot(state.root());
        assert_eq!(
            before, after,
            "a dry run must provably change nothing on disk"
        );
        drop(guard);
    }

    /// A byte-for-byte snapshot of every file under `root`, keyed by relative
    /// path -- the same style `zirv ctx optimize`'s own before/after test
    /// uses to prove report-only really means report-only.
    fn snapshot(root: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
        let mut out = std::collections::BTreeMap::new();
        fn walk(dir: &Path, root: &Path, out: &mut std::collections::BTreeMap<String, Vec<u8>>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else if let Ok(bytes) = std::fs::read(&path) {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .to_string();
                    out.insert(rel, bytes);
                }
            }
        }
        walk(root, root, &mut out);
        out
    }

    #[test]
    fn dry_run_refuses_without_a_transcript_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path();
        let record = Record::new(
            "22222222-2222-4333-8444-555555555555",
            "claude",
            repo,
            Verb::Wrap,
        );
        let guard = SessionGuard::register(&state, record);
        let env: std::collections::HashMap<String, String> = [
            (STATE_ENV.to_string(), state.root().display().to_string()),
            (
                crate::commands::ctx::adapters::SESSION_ENV.to_string(),
                "22222222-2222-4333-8444-555555555555".to_string(),
            ),
        ]
        .into();
        let args = HandoverArgs {
            model: None,
            agent: None,
            dry_run: true,
            force: false,
        };
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, repo, &|k| env.get(k).cloned())
            .expect_err("no transcript yet");
        assert!(err.to_string().contains("no transcript"), "got {err}");
        drop(guard);
    }

    #[test]
    fn refuses_with_no_session_identity_in_the_environment() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let env: std::collections::HashMap<String, String> = [(
            STATE_ENV.to_string(),
            tmp.path().join("state").display().to_string(),
        )]
        .into();
        let args = HandoverArgs {
            model: None,
            agent: None,
            dry_run: false,
            force: false,
        };
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("no session identity");
        assert!(err.to_string().contains("no session identity"), "got {err}");
    }

    /// Issue #84 acceptance: "the successor keeps the same session id: mail
    /// sent to the seat before the swap is delivered after it, and `zirv ctx
    /// nudge <id>` still reaches it." `wrap::perform_handover_swap` never
    /// calls `SessionGuard::refresh_session` -- only `adopt_child_pid`, twice
    /// (parked on zirv's own pid for the duration of the swap, then the
    /// fresh child's) -- exactly the sequence driven directly against the
    /// registry here, which is what proves the address never moves without
    /// needing a real pty child (`perform_handover_swap`'s own pty/adapter
    /// mechanics reuse `wrap::quit_child`/`wrap::relaunch`, already covered
    /// by their own tests).
    #[test]
    fn a_handover_preserves_the_same_short_id_for_mail_and_nudge() {
        use crate::commands::ctx::mail;
        use crate::commands::ctx::sessions::resolve_prefix;

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let cfg = CtxConfig::default();

        let record = Record::new(
            "11111111-2222-4333-8444-555555555555",
            "claude",
            &repo,
            Verb::Wrap,
        );
        let mut guard = SessionGuard::register(&state, record);
        let short_before = guard.short().to_string();

        // Mail sent to the seat's stable address *before* the swap.
        let msg = mail::Message {
            from_session: "sender-session".to_string(),
            from_agent: "claude".to_string(),
            to: "any".to_string(),
            to_session: Some(short_before.clone()),
            sent: 1,
            body: "hold on, look at this once you're free".to_string(),
        };
        let repo_slug = crate::commands::ctx::state::repo_slug(&repo);
        mail::store_to(&state, &repo_slug, &repo_slug, &msg, &cfg).expect("store mail");

        // The exact bookkeeping `perform_handover_swap` performs: park on
        // zirv's own pid for the duration of the swap, then adopt the fresh
        // child's pid -- never `refresh_session`, so the short id never moves.
        // Both `adopt_child_pid` calls use this test process's own (alive)
        // pid, standing in for "zirv's own pid" and then "the successor's
        // pid" in turn -- `sessions::list` sweeps any record naming a dead
        // pid, so a genuinely fake pid here would make the session
        // disappear from the registry before `resolve_prefix` ever ran,
        // which is not the scenario under test.
        guard.adopt_child_pid(std::process::id());
        guard.adopt_child_pid(std::process::id());

        assert_eq!(
            guard.short(),
            short_before,
            "the delivery address never moved"
        );

        let after = mail::list(&state, &repo_slug, None, Some(&short_before)).expect("list");
        assert_eq!(
            after.len(),
            1,
            "mail sent before the swap is still there after it"
        );
        assert_eq!(after[0].1.body, "hold on, look at this once you're free");

        let resolved = resolve_prefix(&state, &short_before).expect("nudge still resolves");
        assert_eq!(resolved.short, short_before);

        drop(guard);
    }

    #[test]
    fn refuses_an_unknown_target_agent_before_writing_any_request() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path();
        let record = Record::new(
            "33333333-2222-4333-8444-555555555555",
            "claude",
            repo,
            Verb::Wrap,
        );
        let guard = SessionGuard::register(&state, record);
        let env: std::collections::HashMap<String, String> = [
            (STATE_ENV.to_string(), state.root().display().to_string()),
            (
                crate::commands::ctx::adapters::SESSION_ENV.to_string(),
                "33333333-2222-4333-8444-555555555555".to_string(),
            ),
        ]
        .into();
        let args = HandoverArgs {
            model: None,
            agent: Some("not-a-real-agent".to_string()),
            dry_run: false,
            force: false,
        };
        let mut out = Vec::new();
        let err =
            run_with(&args, &mut out, repo, &|k| env.get(k).cloned()).expect_err("unknown agent");
        assert!(err.to_string().contains("unknown agent"), "got {err}");
        assert!(
            !request_path(&state, "33333333").exists(),
            "must fail before writing a request nobody will ever claim"
        );
        drop(guard);
    }
}
