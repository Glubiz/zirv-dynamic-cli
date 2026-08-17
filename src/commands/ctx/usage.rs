use std::io::{Read, Write};
use std::process::{Command, Stdio};

use serde_json::Value;

use super::adapters::{self, AgentAdapter};
use super::config::{CtxConfig, EnvLookup, PaceConfig, env_from_process};
use super::pace::{self, PaceDecision};
use super::poll;
use super::state::{StateDir, now_secs};
use super::window::{UsageWindows, Window, age_secs};
use super::{CtxResult, window};

#[derive(Debug, clap::Args)]
pub struct UsageArgs {
    #[command(subcommand)]
    pub action: Option<UsageAction>,
}

#[derive(Debug, clap::Subcommand)]
pub enum UsageAction {
    /// Statusline wrapper: record usage windows, then run the original command.
    Tee {
        /// The original statusline command, after `--`.
        //
        // `allow_hyphen_values` + `last` without `trailing_var_arg`, matching
        // `ExecArgs::command`: adding `trailing_var_arg` trips a clap debug
        // assertion that aborts the process instead of erroring.
        #[arg(allow_hyphen_values = true, last = true)]
        command: Vec<String>,
    },
}

/// Last-resort statusline: enough context to keep the line useful when the
/// chained command is missing or broken.
pub fn fallback_line(json: &str) -> String {
    let value: Value = serde_json::from_str(json).unwrap_or(Value::Null);
    let model = value
        .get("model")
        .and_then(|m| m.get("display_name"))
        .and_then(Value::as_str)
        .unwrap_or("claude");
    let context = value
        .get("context_window")
        .and_then(|c| c.get("used_percentage"))
        .and_then(Value::as_f64);

    match context {
        Some(percent) => format!("{model} | context {}%", percent.round() as i64),
        None => model.to_string(),
    }
}

/// Never returns non-zero and never returns without emitting a statusline:
/// Claude Code shows whatever this prints, so a silent failure would look like
/// a broken terminal to the user.
pub fn run_tee<W: Write>(
    w: &mut W,
    stdin_text: &str,
    command: &[String],
    state: Option<&StateDir>,
    now: u64,
) -> i32 {
    // Persisting is best-effort and happens first, so a broken statusline
    // script cannot cost us the reading.
    if let (Some(state), Some(fresh)) = (state, window::parse_statusline(stdin_text, now)) {
        // The legacy global file, still written exactly as before: `zirv ctx
        // usage`, `pace` and `wrap`'s status bar all read it, and an operator
        // downgrading must not lose their readout.
        let merged = window::merge(window::load(state), fresh.clone());
        let _ = window::store(state, &merged);

        // The same reading, also filed under the account it belongs to. The
        // provider is taken from the claude adapter rather than spelled out
        // here: this tee IS Claude Code's statusline hook (`parse_statusline`
        // reads Claude's documented `rate_limits` block and nothing else), so
        // its account is whatever that adapter says its account is. It is not
        // resolved through `adapters::select`, which would put a config load
        // -- and its failure modes -- on a statusline hot path that has never
        // had one.
        let provider = adapters::claude::ClaudeAdapter::new(None).provider();
        let per_provider =
            window::merge(window::load_for(state, provider).unwrap_or_default(), fresh);
        let _ = window::store_for(state, provider, &per_provider);
    }

    let chained = run_chained(stdin_text, command);
    match chained {
        Some(output) if !output.trim().is_empty() => {
            let _ = write!(w, "{output}");
        }
        _ => {
            let _ = writeln!(w, "{}", fallback_line(stdin_text));
        }
    }
    0
}

/// `None` when there is no command, it could not start, or it failed.
fn run_chained(stdin_text: &str, command: &[String]) -> Option<String> {
    let (program, rest) = command.split_first()?;
    let mut child = Command::new(program)
        .args(rest)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .ok()?;

    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(stdin_text.as_bytes());
    }
    drop(child.stdin.take());

    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

fn read_stdin() -> String {
    let mut buffer = String::new();
    let _ = std::io::stdin().read_to_string(&mut buffer);
    buffer
}

fn line_for(
    w: &mut impl Write,
    name: &str,
    window: Option<&Window>,
    now: u64,
    cfg: &PaceConfig,
    label: &str,
) -> CtxResult<()> {
    match window {
        Some(found) => {
            let age = age_secs(found, now);
            let freshness = if age > cfg.collector_max_age_secs {
                format!("{age}s ago, stale")
            } else {
                format!("{age}s ago")
            };
            let reset = if found.resets_at == 0 {
                "reset time unknown".to_string()
            } else {
                format!("resets at unix {}", found.resets_at)
            };
            writeln!(
                w,
                "  {name}: {:.1}% used ({label}, observed {freshness}, {reset})",
                found.used_percentage
            )?;
        }
        None => writeln!(w, "  {name}: not reported")?,
    }
    Ok(())
}

pub fn report<W: Write>(
    w: &mut W,
    collector: &UsageWindows,
    estimator: Option<&UsageWindows>,
    now: u64,
    cfg: &PaceConfig,
) -> CtxResult<()> {
    writeln!(
        w,
        "collector (server-authoritative, from the statusline tee):"
    )?;
    line_for(
        w,
        "five_hour",
        collector.five_hour.as_ref(),
        now,
        cfg,
        "collector",
    )?;
    line_for(
        w,
        "seven_day",
        collector.seven_day.as_ref(),
        now,
        cfg,
        "collector",
    )?;

    if collector.five_hour.is_none() && collector.seven_day.is_none() {
        writeln!(
            w,
            "  no readings yet. Wire your statusline through `zirv ctx usage tee -- <your statusline command>`; Claude reports these fields only for Pro and Max sessions, after the first response."
        )?;
    }

    match estimator {
        Some(windows) => {
            writeln!(w, "\nestimator (approximation from local transcripts):")?;
            line_for(
                w,
                "five_hour",
                windows.five_hour.as_ref(),
                now,
                cfg,
                "approximation",
            )?;
            line_for(
                w,
                "seven_day",
                windows.seven_day.as_ref(),
                now,
                cfg,
                "approximation",
            )?;
            writeln!(
                w,
                "  token class weighting is undocumented, so treat these as an approximation, never ground truth."
            )?;
        }
        None => {
            writeln!(
                w,
                "\nestimator: off (set pace.five_hour_budget_tokens or pace.seven_day_budget_tokens to enable it)"
            )?;
        }
    }

    writeln!(w, "\npacing:")?;
    if !cfg.enabled {
        writeln!(w, "  pacing is disabled (pace.enabled = false)")?;
        return Ok(());
    }
    writeln!(w, "  ceiling {:.1}%", cfg.max_percent)?;
    writeln!(
        w,
        "  wait bound: five_hour up to {}s, seven_day up to {}s{}",
        pace::wait_cap("five_hour", cfg),
        pace::wait_cap("seven_day", cfg),
        if cfg.max_wait_secs.is_some() {
            " (absolute override in effect)"
        } else {
            " (each window's own length plus slack)"
        }
    )?;
    let decision = pace::decide(collector, estimator, now, cfg);
    match &decision {
        PaceDecision::Slow {
            delay_secs,
            window,
            percent,
            source,
        } => {
            writeln!(
                w,
                "  throttle: would delay ~{delay_secs}s ({percent:.0}% of {window}, {})",
                source.as_str()
            )?;
        }
        PaceDecision::WaitUntil { .. } => {
            writeln!(w, "  would wait: {}", pace::describe(&decision))?;
        }
        _ => {
            writeln!(w, "  verdict: {}", pace::describe(&decision))?;
        }
    }
    Ok(())
}

pub fn run_with<W: Write>(
    args: &UsageArgs,
    w: &mut W,
    repo: &std::path::Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    match &args.action {
        Some(UsageAction::Tee { command }) => {
            let state = StateDir::resolve(env).ok();
            Ok(run_tee(
                w,
                &read_stdin(),
                command,
                state.as_ref(),
                now_secs(),
            ))
        }
        None => {
            let cfg = CtxConfig::load(repo, env)?;
            let state = StateDir::resolve(env)?;
            // O: `select` can refuse for reasons that have nothing to do with
            // reading a usage report -- a repo `.settings.toml` disabling the
            // configured agent, or an unlaunchable claude -- and before this
            // command was provider-scoped it never called `select` at all, so
            // none of those ever stopped it from working. Only a provider
            // *name* is actually needed here, not a working adapter.
            //
            // Low 5: derived from the *configured* agent name (`adapters::
            // provider_for_agent_name`), not guessed as `LEGACY_USAGE_
            // PROVIDER` on every refusal -- `agent = "codex"` with codex
            // disabled must still show "openai: no usage source", not
            // Anthropic percentages left over from a claude session that
            // happened to write the legacy file.
            //
            // Final wave item 4: `provider_for_usage_readout` tries `resolve_
            // default` first, so an *unset* `agent` with an operator-
            // disabled claude still lands on codex's own provider (what
            // `resolve_default`'s own fallback loop would actually select)
            // instead of guessing the legacy default. Falling back to
            // `provider_for_agent_name`, and from there to the legacy
            // provider, is reserved for when `resolve_default` itself
            // refuses (an explicitly configured, repo-disabled agent, or
            // nothing enabled and ready at all).
            let provider = adapters::provider_for_usage_readout(&cfg);
            let now = now_secs();
            // Best-effort source refresh, ahead of the no-usage-source check
            // below -- the same two calls the pacing gate itself makes
            // (`pace::wait_for_window`'s own `refresh_sources`), since this
            // command is the manual end-to-end check: it must actually try
            // to acquire data, not just report whatever happened to already
            // be on disk.
            let http_poller = poll::HttpPoller;
            if provider == window::CODEX_USAGE_PROVIDER {
                // See `pace::refresh_sources`'s own doc comment: resolved via
                // `crate::utils::home_dir()`, not left to `refresh_codex_
                // usage`'s internal `dirs::home_dir()` fallback, which
                // ignores `HOME`/`USERPROFILE` on Windows and so cannot be
                // pointed at a test fixture there.
                let sessions_dir = crate::utils::home_dir()
                    .ok()
                    .map(|h| h.join(".codex").join("sessions"));
                window::refresh_codex_usage(
                    &state,
                    sessions_dir.as_deref(),
                    now,
                    cfg.pace.collector_max_age_secs,
                );
            }
            // Gated on `pace.enabled` (review finding): pacing disabled means
            // zirv makes no proactive vendor request on this operator's
            // behalf -- `ZIRV_CTX_PACE=false` must not still send an OAuth
            // token to a usage endpoint. Passive sources above still refresh;
            // only the active poll is withheld. The gate paths need no such
            // check here because `wait_for_window` already returns before its
            // own `refresh_sources` when pacing is off.
            let poll_reading = if cfg.pace.enabled {
                poll::maybe_poll(&state, &cfg.pace, now, provider, &http_poller)
            } else {
                None
            };

            // Check whether anything has been recorded for this provider,
            // now that the refresh above has had its chance to acquire some.
            if window::has_no_usage_source(&state, provider) {
                writeln!(w, "{provider}: no usage source")?;
                return Ok(0);
            }
            let (collector, estimator) = pace::current_windows(&state, &cfg.pace, now, provider);
            report(w, &collector, estimator.as_ref(), now, &cfg.pace)?;

            if cfg.pace.use_credits.for_provider(provider) {
                writeln!(
                    w,
                    "\nuse_credits: enabled for this harness -- pacing gate skipped"
                )?;
            }
            // Only when a poll just ran and returned an opinion: never invent
            // vendor state from a stale reading, and no line at all when no
            // poll ran this time (disabled, floored, or nothing needed it).
            if let Some(reading) = &poll_reading
                && let Some(vendor_credits_enabled) = reading.vendor_credits_enabled
            {
                writeln!(
                    w,
                    "vendor reports credits {} on this plan",
                    if vendor_credits_enabled {
                        "enabled"
                    } else {
                        "disabled"
                    }
                )?;
            }
            Ok(0)
        }
    }
}

pub fn run<W: Write>(args: &UsageArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &repo, &env)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::Parser;

    use super::*;
    use crate::commands::ctx::state::StateDir;
    use crate::commands::ctx::window;

    fn fixture(name: &str) -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    fn statusline_script() -> Vec<String> {
        vec![
            "sh".to_string(),
            fixture("fake-statusline.sh").display().to_string(),
        ]
    }

    #[test]
    fn tee_persists_the_windows_and_chains_the_original_command() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let json =
            std::fs::read_to_string(fixture("statusline-with-limits.json")).expect("fixture");

        let mut out = Vec::new();
        let code = run_tee(
            &mut out,
            &json,
            &statusline_script(),
            Some(&state),
            1_784_999_000,
        );
        assert_eq!(code, 0);

        let printed = String::from_utf8(out).expect("utf8");
        assert!(
            printed.contains("CHAINED-OK"),
            "chained output must reach the terminal: {printed}"
        );

        let stored = window::load(&state);
        assert_eq!(stored.five_hour.expect("five_hour").used_percentage, 87.5);
        assert_eq!(
            stored.seven_day.expect("seven_day").resets_at,
            1_785_400_000
        );
    }

    /// The tee is Claude Code's statusline, so the same reading is also filed
    /// under the account it belongs to. Both files are written: the legacy
    /// one because `zirv ctx usage`, `pace` and `wrap`'s status bar read it,
    /// the provider one so a per-account header can tell Anthropic's windows
    /// apart from a vendor it has no source for.
    #[test]
    fn tee_files_the_reading_under_the_account_it_belongs_to() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let json =
            std::fs::read_to_string(fixture("statusline-with-limits.json")).expect("fixture");

        let mut out = Vec::new();
        run_tee(&mut out, &json, &[], Some(&state), 1_784_999_000);

        let stored = window::load_for(&state, "anthropic").expect("a provider file was written");
        assert_eq!(stored.five_hour.expect("five_hour").used_percentage, 87.5);
        assert_eq!(
            stored,
            window::load(&state),
            "the legacy file keeps the same reading"
        );
        assert_eq!(
            window::load_for(&state, "openai"),
            None,
            "a claude statusline says nothing about anyone else's account"
        );
    }

    #[test]
    fn the_chained_command_receives_the_original_json_on_stdin() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let log = tmp.path().join("seen.json");
        let json =
            std::fs::read_to_string(fixture("statusline-with-limits.json")).expect("fixture");

        // SAFETY: CI runs tests single-threaded.
        unsafe {
            std::env::set_var("FAKE_STATUSLINE_LOG", &log);
        }
        let mut out = Vec::new();
        run_tee(&mut out, &json, &statusline_script(), None, 1);
        unsafe {
            std::env::remove_var("FAKE_STATUSLINE_LOG");
        }

        let seen = std::fs::read_to_string(&log).expect("chained command ran");
        assert_eq!(seen, json, "the payload must pass through byte for byte");
    }

    #[test]
    fn a_payload_without_rate_limits_still_chains_and_writes_no_state() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let json = std::fs::read_to_string(fixture("statusline-no-limits.json")).expect("fixture");

        let mut out = Vec::new();
        let code = run_tee(&mut out, &json, &statusline_script(), Some(&state), 1);
        assert_eq!(code, 0);
        assert!(String::from_utf8_lossy(&out).contains("CHAINED-OK"));
        assert!(
            !state.usage().exists(),
            "nothing to record means no file: {:?}",
            state.usage()
        );
    }

    #[test]
    fn a_failing_chained_command_still_produces_a_statusline() {
        let json =
            std::fs::read_to_string(fixture("statusline-with-limits.json")).expect("fixture");
        unsafe {
            std::env::set_var("FAKE_STATUSLINE_MODE", "fail");
        }
        let mut out = Vec::new();
        let code = run_tee(&mut out, &json, &statusline_script(), None, 1);
        unsafe {
            std::env::remove_var("FAKE_STATUSLINE_MODE");
        }

        assert_eq!(
            code, 0,
            "a broken statusline script must not break the statusline"
        );
        let printed = String::from_utf8(out).expect("utf8");
        assert!(!printed.trim().is_empty(), "fallback line expected");
        assert!(
            printed.contains("Fable 5"),
            "fallback names the model: {printed}"
        );
    }

    #[test]
    fn a_missing_chained_binary_falls_back_instead_of_erroring() {
        let json =
            std::fs::read_to_string(fixture("statusline-with-limits.json")).expect("fixture");
        let mut out = Vec::new();
        let code = run_tee(
            &mut out,
            &json,
            &["/nonexistent/statusline".to_string()],
            None,
            1,
        );
        assert_eq!(code, 0);
        assert!(String::from_utf8_lossy(&out).contains("Fable 5"));
    }

    #[test]
    fn no_chained_command_means_the_fallback_is_the_statusline() {
        let json =
            std::fs::read_to_string(fixture("statusline-with-limits.json")).expect("fixture");
        let mut out = Vec::new();
        let code = run_tee(&mut out, &json, &[], None, 1);
        assert_eq!(code, 0);
        let printed = String::from_utf8(out).expect("utf8");
        assert!(printed.contains("Fable 5"));
        assert!(
            printed.contains("42"),
            "context percentage carries through: {printed}"
        );
    }

    #[test]
    fn an_unwritable_state_dir_never_breaks_the_statusline() {
        let json =
            std::fs::read_to_string(fixture("statusline-with-limits.json")).expect("fixture");
        let state = StateDir::from_root(PathBuf::from("/proc/nonexistent/zirv-ctx"));
        let mut out = Vec::new();
        let code = run_tee(&mut out, &json, &statusline_script(), Some(&state), 1);
        assert_eq!(code, 0);
        assert!(String::from_utf8_lossy(&out).contains("CHAINED-OK"));
    }

    #[test]
    fn garbage_on_stdin_is_passed_through_untouched() {
        let mut out = Vec::new();
        let code = run_tee(&mut out, "this is not json", &statusline_script(), None, 1);
        assert_eq!(code, 0);
        assert!(String::from_utf8_lossy(&out).contains("CHAINED-OK"));
    }

    #[test]
    fn the_fallback_line_is_plain_and_has_no_em_dash() {
        let json =
            std::fs::read_to_string(fixture("statusline-with-limits.json")).expect("fixture");
        let line = fallback_line(&json);
        assert_eq!(line.lines().count(), 1);
        assert!(!line.contains('\u{2014}'));
        assert_eq!(
            fallback_line("garbage").lines().count(),
            1,
            "always exactly one line"
        );
    }

    #[test]
    fn tee_parses_as_a_subcommand_with_a_trailing_command() {
        let cli = crate::commands::ctx::CtxCli::try_parse_from([
            "zirv ctx",
            "usage",
            "tee",
            "--",
            "bash",
            "~/.claude/statusline-command.sh",
        ])
        .expect("usage tee should parse");
        match cli.verb {
            crate::commands::ctx::CtxVerb::Usage(args) => match args.action {
                Some(UsageAction::Tee { command }) => assert_eq!(
                    command,
                    vec![
                        "bash".to_string(),
                        "~/.claude/statusline-command.sh".to_string()
                    ]
                ),
                other => panic!("expected Tee, got {other:?}"),
            },
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    use crate::commands::ctx::config::PaceConfig;
    use crate::commands::ctx::window::{UsageWindows, Window};

    const NOW: u64 = 1_785_507_315;

    fn collector_at(percent: f64, age: u64) -> UsageWindows {
        UsageWindows {
            five_hour: Some(Window {
                used_percentage: percent,
                resets_at: NOW + 1800,
                observed_at: NOW - age,
            }),
            seven_day: None,
        }
    }

    #[test]
    fn the_report_names_each_window_and_its_freshness() {
        let mut out = Vec::new();
        report(
            &mut out,
            &collector_at(63.0, 42),
            None,
            NOW,
            &PaceConfig::default(),
        )
        .expect("report");
        let text = String::from_utf8(out).expect("utf8");

        assert!(text.contains("five_hour"), "got {text}");
        assert!(text.contains("63"), "got {text}");
        assert!(
            text.contains("42s ago"),
            "freshness must be visible: {text}"
        );
        assert!(
            text.contains("seven_day"),
            "absent windows are still listed: {text}"
        );
        assert!(!text.contains('\u{2014}'));
    }

    #[test]
    fn an_absent_window_says_so_rather_than_showing_zero() {
        let mut out = Vec::new();
        report(
            &mut out,
            &UsageWindows::default(),
            None,
            NOW,
            &PaceConfig::default(),
        )
        .expect("report");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("not reported"),
            "no data must never look like 0%: {text}"
        );
        assert!(
            text.contains("statusline") || text.contains("zirv ctx usage tee"),
            "tell the user how to start collecting: {text}"
        );
    }

    #[test]
    fn a_stale_collector_reading_is_labeled_stale() {
        let mut out = Vec::new();
        report(
            &mut out,
            &collector_at(50.0, 100_000),
            None,
            NOW,
            &PaceConfig::default(),
        )
        .expect("report");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("stale"), "got {text}");
    }

    #[test]
    fn estimator_output_is_labeled_an_approximation() {
        let estimated = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 12.5,
                resets_at: NOW + 600,
                observed_at: NOW,
            }),
            seven_day: None,
        };
        let mut out = Vec::new();
        report(
            &mut out,
            &UsageWindows::default(),
            Some(&estimated),
            NOW,
            &PaceConfig::default(),
        )
        .expect("report");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("approximation"), "got {text}");
        assert!(text.contains("12.5"), "got {text}");
    }

    #[test]
    fn the_report_ends_with_the_pacing_verdict() {
        let mut out = Vec::new();
        report(
            &mut out,
            &collector_at(99.5, 10),
            None,
            NOW,
            &PaceConfig::default(),
        )
        .expect("report");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("waiting") || text.contains("would wait"),
            "got {text}"
        );
        assert!(text.contains("99"), "got {text}");
    }

    #[test]
    fn the_report_explains_the_per_window_wait_bound() {
        let mut out = Vec::new();
        report(
            &mut out,
            &collector_at(50.0, 10),
            None,
            NOW,
            &PaceConfig::default(),
        )
        .expect("report");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("wait bound"), "got {text}");
        assert!(text.contains("21600"), "five hours plus slack: {text}");
        assert!(text.contains("608400"), "seven days plus slack: {text}");
        assert!(text.contains("own length plus slack"), "got {text}");
    }

    #[test]
    fn the_report_flags_an_absolute_wait_override() {
        let cfg = PaceConfig {
            max_wait_secs: Some(7200),
            ..PaceConfig::default()
        };
        let mut out = Vec::new();
        report(&mut out, &collector_at(50.0, 10), None, NOW, &cfg).expect("report");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("7200"), "got {text}");
        assert!(text.contains("override in effect"), "got {text}");
    }

    #[test]
    fn the_report_says_when_pacing_is_switched_off() {
        let cfg = PaceConfig {
            enabled: false,
            ..PaceConfig::default()
        };
        let mut out = Vec::new();
        report(&mut out, &collector_at(99.9, 10), None, NOW, &cfg).expect("report");
        assert!(String::from_utf8_lossy(&out).contains("pacing is disabled"));
    }

    #[test]
    fn the_verb_reports_without_a_subcommand() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Like every sibling test here: an empty redirected home keeps the
        // no-subcommand path's source refresh (rollout scan + HttpPoller
        // token lookup) away from the real machine's credentials -- without
        // this, `maybe_poll` would issue a live authenticated request on
        // every `cargo test` run.
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            tmp.path().join("state").display().to_string(),
        )]
        .into();

        let mut out = Vec::new();
        let code = run_with(&UsageArgs { action: None }, &mut out, tmp.path(), &|k| {
            env.get(k).cloned()
        })
        .expect("report runs with no state at all");
        assert_eq!(code, 0);
        assert!(String::from_utf8_lossy(&out).contains("not reported"));
    }

    /// E: codex/openai has no possible usage source, so `zirv ctx usage`
    /// (no subcommand) must print the plain "<provider>: no usage source"
    /// line README documents -- never `report`'s own "not reported ... wire
    /// your statusline" text, which only ever helps claude.
    #[test]
    fn the_verb_names_a_provider_with_no_usage_source() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path();
        // Task 6's own source refresh (`refresh_codex_usage`, ahead of the
        // no-usage-source check) resolves the codex sessions directory via
        // `crate::utils::home_dir()` precisely so a real machine's own
        // `~/.codex/sessions` cannot leak into this "nothing recorded" test
        // -- an empty `HomeGuard` home keeps that scan a genuine no-op.
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        // `agent` is `REPO_FORBIDDEN` (final wave item 1) -- configured via
        // `ZIRV_CTX_AGENT` (the operator layer) instead of the repo's own
        // `ctx.toml`.
        let env: std::collections::HashMap<String, String> = [
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                tmp.path().join("state").display().to_string(),
            ),
            ("ZIRV_CTX_AGENT".to_string(), "codex".to_string()),
        ]
        .into();

        let mut out = Vec::new();
        let code = run_with(&UsageArgs { action: None }, &mut out, repo, &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        assert_eq!(code, 0);
        let printed = String::from_utf8(out).expect("utf8");
        assert_eq!(printed, "openai: no usage source\n");
    }

    /// O: before this command was provider-scoped it never called `adapters::
    /// select` at all -- it read the one legacy global file directly -- so a
    /// repo whose `.settings.toml` disables its own configured agent (or any
    /// other `select` refusal) never stopped it from working. `select`'s
    /// `?` regressed that: with claude configured (as the operator, since
    /// `agent` is now `REPO_FORBIDDEN`) and this repo's `.settings.toml`
    /// disabling it, `select` now refuses outright, and this must still
    /// print the legacy reading rather than hard-error -- via `provider_
    /// for_agent_name("claude")`, which happens to answer `"anthropic"` too
    /// (claude *is* the legacy provider), not because this name is unknown
    /// (see the codex-disabled test below for that case).
    #[test]
    fn the_verb_falls_back_to_the_legacy_reading_when_select_refuses() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path();
        std::fs::create_dir_all(repo.join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let state_dir = tmp.path().join("state");
        let env: std::collections::HashMap<String, String> = [
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state_dir.display().to_string(),
            ),
            ("ZIRV_CTX_AGENT".to_string(), "claude".to_string()),
        ]
        .into();

        let state = StateDir::from_root(state_dir);
        window::store(
            &state,
            &UsageWindows {
                five_hour: Some(Window {
                    used_percentage: 42.0,
                    resets_at: 1_785_509_000,
                    observed_at: now_secs(),
                }),
                seven_day: None,
            },
        )
        .expect("store");

        let mut out = Vec::new();
        let code = run_with(&UsageArgs { action: None }, &mut out, repo, &|k| {
            env.get(k).cloned()
        })
        .expect("falls back rather than hard-erroring on the refusal");
        assert_eq!(code, 0);
        let printed = String::from_utf8(out).expect("utf8");
        assert!(
            printed.contains("42"),
            "the legacy reading still gets through: {printed}"
        );
        assert!(
            !printed.contains("no usage source"),
            "anthropic (the legacy provider) is exempt from that check: {printed}"
        );
    }

    /// Low 5 (fix): unlike the claude case above, `agent = "codex"` with
    /// codex disabled must show "openai: no usage source", never Anthropic
    /// percentages left over from a claude session's legacy file. Guessing
    /// `LEGACY_USAGE_PROVIDER` on every `select` refusal (the pre-fix
    /// behavior) got this specific case wrong; deriving the provider from
    /// the configured name directly gets it right regardless of whether
    /// `select` itself would have refused.
    #[test]
    fn a_disabled_codex_shows_no_usage_source_not_anthropic_numbers() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path();
        // `agent` is `REPO_FORBIDDEN` (final wave item 1) -- configured via
        // `ZIRV_CTX_AGENT` (the operator layer) instead of the repo's own
        // `ctx.toml`.
        std::fs::create_dir_all(repo.join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.join(".zirv/.settings.toml"),
            "[agents.codex]\nenabled = false\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let state_dir = tmp.path().join("state");
        let env: std::collections::HashMap<String, String> = [
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state_dir.display().to_string(),
            ),
            ("ZIRV_CTX_AGENT".to_string(), "codex".to_string()),
        ]
        .into();

        // The legacy global file a claude session left behind: still on
        // disk, but must not be misattributed to this codex-configured repo.
        let state = StateDir::from_root(state_dir);
        window::store(
            &state,
            &UsageWindows {
                five_hour: Some(Window {
                    used_percentage: 77.0,
                    resets_at: 1_785_509_000,
                    observed_at: now_secs(),
                }),
                seven_day: None,
            },
        )
        .expect("store");

        let mut out = Vec::new();
        let code = run_with(&UsageArgs { action: None }, &mut out, repo, &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        assert_eq!(code, 0);
        let printed = String::from_utf8(out).expect("utf8");
        assert_eq!(printed, "openai: no usage source\n");
    }

    /// Final wave item 4: no `agent` configured anywhere, and claude
    /// disabled by the *operator* (home `.settings.toml`, not the repo) --
    /// `resolve_default`'s own fallback loop correctly skips it and lands
    /// on codex, so this must show "openai: no usage source", not the
    /// legacy Anthropic default `provider_for_agent_name(None)` alone would
    /// have guessed for an unset `agent`.
    #[test]
    fn an_unset_agent_with_an_operator_disabled_claude_reports_codexs_own_provider() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path();
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            tmp.path().join("state").display().to_string(),
        )]
        .into();

        let mut out = Vec::new();
        let code = run_with(&UsageArgs { action: None }, &mut out, repo, &|k| {
            env.get(k).cloned()
        })
        .expect("runs");
        assert_eq!(code, 0);
        let printed = String::from_utf8(out).expect("utf8");
        assert_eq!(printed, "openai: no usage source\n");
    }
}
