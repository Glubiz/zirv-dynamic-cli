use std::io::Write;
use std::path::Path;

use super::adapters::{self, DefaultOrigin};
use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::handoff::latest_for_repo;
use super::mail;
use super::state::{StateDir, repo_slug};
use super::{CtxResult, log};

/// The `chat:` status line: the adapter `zirv ctx chat` would launch and the
/// rule that picked it (`adapters::resolve_default`'s own `DefaultOrigin`),
/// or -- degrading rather than failing the whole command -- a summary of why
/// nothing qualifies. `resolve_default`'s own error already names each
/// candidate adapter and its reason, one per line; those are joined with
/// "; " here (dropping the "no agent is both enabled and ready:" summary
/// line) so the status line stays on one row like every other line `status`
/// prints, instead of splitting a single logical fact across several.
fn describe_chat(cfg: &CtxConfig) -> String {
    match adapters::resolve_default(cfg) {
        Ok((adapter, origin)) => {
            let rule = match origin {
                DefaultOrigin::Configured => "configured",
                DefaultOrigin::FirstEnabledReady => "first enabled and ready",
            };
            format!("chat: {} ({rule})", adapter.name())
        }
        Err(e) => {
            let full = e.to_string();
            let reasons: Vec<&str> = full.lines().skip(1).collect();
            let detail = if reasons.is_empty() {
                full.clone()
            } else {
                reasons.join("; ")
            };
            format!("chat: unavailable ({detail})")
        }
    }
}

#[derive(Debug, clap::Args)]
pub struct StatusArgs {
    /// How many recent supervisor decisions to show.
    #[arg(long, default_value_t = 10)]
    pub decisions: usize,
}

pub fn run_with<W: Write>(
    args: &StatusArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let state = StateDir::resolve(env)?;
    writeln!(w, "state dir: {}", state.root().display())?;

    match crate::settings::AgentGate::load(repo, env) {
        Ok(gate) => {
            writeln!(w, "\nagents:")?;
            for adapter in crate::commands::ctx::adapters::all(None) {
                let name = adapter.name();
                let (enabled, location) = gate
                    .states()
                    .find(|(n, _)| *n == name)
                    .map(|(_, s)| (s.enabled, s.location()))
                    .unwrap_or((true, "default".to_string()));
                writeln!(
                    w,
                    "  {name:<8} {:<8} ({location})",
                    if enabled { "enabled" } else { "disabled" }
                )?;
            }
        }
        Err(e) => writeln!(w, "\nagents: (settings unreadable: {e})")?,
    }

    match CtxConfig::load(repo, env) {
        Ok(cfg) => writeln!(w, "\n{}", describe_chat(&cfg))?,
        Err(e) => writeln!(w, "\nchat: unavailable (configuration error: {e})")?,
    }

    let mail_slug = repo_slug(repo);
    match mail::list(&state, &mail_slug, None) {
        Ok(messages) => writeln!(w, "mail: {} unread", messages.len())?,
        Err(_) => writeln!(w, "mail: (unreadable)")?,
    }

    let mut sessions: Vec<String> = std::fs::read_dir(state.sockets())
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("sock"))
                .filter_map(|e| {
                    e.path()
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                })
                .collect()
        })
        .unwrap_or_default();
    sessions.sort();

    writeln!(w, "\nsupervised sessions:")?;
    if sessions.is_empty() {
        writeln!(w, "  no supervised sessions")?;
    } else {
        for session in &sessions {
            writeln!(w, "  {session}")?;
        }
    }

    let windows = crate::commands::ctx::window::load(&state);
    let describe = |name: &str, window: Option<&crate::commands::ctx::window::Window>| match window
    {
        Some(found) => format!("{name} {:.0}%", found.used_percentage),
        None => format!("{name} unknown"),
    };
    writeln!(
        w,
        "\nusage windows: {}, {} (see `zirv ctx usage` for detail)",
        describe("five_hour", windows.five_hour.as_ref()),
        describe("seven_day", windows.seven_day.as_ref())
    )?;

    writeln!(w, "\nlatest handoff for {}:", repo.display())?;
    match latest_for_repo(&state, repo)? {
        Some((path, handoff)) => {
            writeln!(w, "  {}", path.display())?;
            writeln!(w, "  task: {}", handoff.task)?;
            writeln!(w, "  next: {}", handoff.next_step)?;
        }
        None => writeln!(w, "  no handoff stored")?,
    }

    writeln!(w, "\nrecent decisions:")?;
    let lines = log::tail(&state, args.decisions)?;
    if lines.is_empty() {
        writeln!(w, "  none recorded")?;
    } else {
        for line in lines.iter().rev() {
            writeln!(w, "  {line}")?;
        }
    }

    Ok(0)
}

pub fn run<W: Write>(args: &StatusArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &repo, &env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::handoff::Handoff;
    use crate::commands::ctx::log;
    #[cfg(unix)]
    use crate::commands::ctx::signal;
    use crate::commands::ctx::state::{STATE_ENV, StateDir};

    fn env_for(state: &std::path::Path) -> std::collections::HashMap<String, String> {
        [(STATE_ENV.to_string(), state.display().to_string())].into()
    }

    #[test]
    fn an_empty_state_dir_reports_nothing_supervised() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let env = env_for(&state);

        let mut out = Vec::new();
        let code = run_with(&StatusArgs { decisions: 10 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        assert_eq!(code, 0);

        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains(&state.display().to_string()),
            "name the state dir: {text}"
        );
        assert!(text.contains("no supervised sessions"), "got {text}");
        assert!(text.contains("no handoff"), "got {text}");
    }

    #[test]
    fn it_lists_sockets_decisions_and_the_latest_handoff() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());

        log::append(
            &state,
            &log::Decision {
                ts: 1_700_000_000,
                session: "11111111-2222",
                verb: "wrap",
                verdict: "compact",
                score: 64,
                action: "inject",
                detail: "cooldown armed",
            },
        )
        .expect("append");

        crate::commands::ctx::handoff::store(
            &state,
            tmp.path(),
            "11111111-2222",
            &Handoff {
                task: "Wire the webhook".to_string(),
                next_step: "Write the test".to_string(),
                ..Handoff::default()
            },
        )
        .expect("store");

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 10 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");

        assert!(
            text.contains("compact"),
            "verdict in the decision list: {text}"
        );
        assert!(text.contains("inject"));
        assert!(
            text.contains("Wire the webhook"),
            "latest handoff task: {text}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_live_socket_shows_up_as_a_supervised_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());

        let _server = signal::SignalServer::bind(&state.socket_for("abcdef12-3456")).expect("bind");

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("abcdef12"), "session prefix listed: {text}");
        assert!(!text.contains("no supervised sessions"));
    }

    #[test]
    fn the_decision_limit_is_honored() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());

        for i in 0..5 {
            log::append(
                &state,
                &log::Decision {
                    ts: 1_700_000_000 + i,
                    session: "s",
                    verb: "exec",
                    verdict: "healthy",
                    score: 0,
                    action: &format!("tick{i}"),
                    detail: "",
                },
            )
            .expect("append");
        }

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 2 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("tick4"));
        assert!(text.contains("tick3"));
        assert!(!text.contains("tick0"));
    }

    /// `zirv ctx status` surfaces the `.settings.toml` gate: every known
    /// adapter, whether it is enabled, and (when disabled) why.
    #[test]
    fn status_lists_each_adapter_with_whether_it_is_enabled_and_why() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        std::fs::create_dir_all(tmp.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            tmp.path().join(".zirv/.settings.toml"),
            "[agents.codex]\nenabled = false\n",
        )
        .expect("write");

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");

        assert!(text.contains("agents:"), "got {text}");
        let claude_line = text.lines().find(|l| l.contains("claude")).unwrap_or("");
        assert!(claude_line.contains("enabled"), "got {claude_line}");
        let codex_line = text.lines().find(|l| l.contains("codex")).unwrap_or("");
        assert!(codex_line.contains("disabled"), "got {codex_line}");
        assert!(
            codex_line.contains(".settings.toml"),
            "names the file that disabled it: {codex_line}"
        );
    }

    /// The `chat:` line names both the adapter `zirv ctx chat` would launch
    /// and the rule that picked it: no explicit `agent` configured falls
    /// back to the first enabled-and-ready adapter, while an explicit
    /// `agent = "claude"` in `ctx.toml` is reported as configured instead.
    #[test]
    fn status_names_the_agent_chat_would_launch_and_the_rule_that_chose_it() {
        let default_cfg = CtxConfig::default();
        assert_eq!(
            describe_chat(&default_cfg),
            "chat: claude (first enabled and ready)"
        );

        let configured_cfg = CtxConfig {
            agent: Some("claude".to_string()),
            ..CtxConfig::default()
        };
        assert_eq!(describe_chat(&configured_cfg), "chat: claude (configured)");
    }

    /// When nothing is both enabled and ready, `describe_chat` degrades to
    /// `resolve_default`'s own aggregated reasons rather than failing --
    /// `status` must keep printing everything else even when chat has
    /// nothing to launch.
    #[test]
    fn status_explains_why_chat_cannot_launch_when_nothing_is_enabled_and_ready() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        // Only claude is disabled: codex is still refused, but on its own
        // `ready()` (never implemented), not the settings gate -- matching
        // the design's own example wording.
        std::fs::create_dir_all(tmp.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            tmp.path().join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n",
        )
        .expect("write");

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");

        let chat_line = text.lines().find(|l| l.starts_with("chat:")).unwrap_or("");
        assert!(chat_line.contains("unavailable"), "got {chat_line}");
        assert!(chat_line.contains("claude"), "got {chat_line}");
        assert!(chat_line.contains("disabled"), "got {chat_line}");
        assert!(chat_line.contains("codex"), "got {chat_line}");
        assert!(chat_line.contains("not implemented yet"), "got {chat_line}");
        assert!(
            !chat_line.contains('\u{2014}'),
            "no em dashes in user-facing copy: {chat_line}"
        );
    }

    /// `mail: N unread` counts messages stored for this repo's slug via
    /// `mail::list`, the same store/list path `zirv ctx send`/`zirv ctx
    /// inbox` use.
    #[test]
    fn status_reports_the_unread_mail_count_for_this_repo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let slug = repo_slug(tmp.path());
        let cfg = CtxConfig::default();
        for from_session in ["sender-one", "sender-two"] {
            mail::store(
                &state,
                &slug,
                &mail::Message {
                    from_session: from_session.to_string(),
                    from_agent: "claude".to_string(),
                    to: "any".to_string(),
                    sent: 1_700_000_000,
                    body: "heads up".to_string(),
                },
                &cfg,
            )
            .expect("store");
        }

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");

        assert!(text.contains("mail: 2 unread"), "got {text}");
    }

    #[test]
    fn status_mentions_the_usage_windows() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        state.ensure().expect("ensure");
        let env = env_for(state.root());

        crate::commands::ctx::window::store(
            &state,
            &crate::commands::ctx::window::UsageWindows {
                five_hour: Some(crate::commands::ctx::window::Window {
                    used_percentage: 77.0,
                    resets_at: 1_785_509_000,
                    observed_at: crate::commands::ctx::state::now_secs(),
                }),
                seven_day: None,
            },
        )
        .expect("store");

        let mut out = Vec::new();
        run_with(&StatusArgs { decisions: 5 }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("usage"), "got {text}");
        assert!(text.contains("77"), "got {text}");
    }
}
