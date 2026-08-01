use std::io::{Read, Write};
use std::process::{Command, Stdio};

use serde_json::Value;

use super::config::{EnvLookup, env_from_process};
use super::state::{StateDir, now_secs};
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
        let merged = window::merge(window::load(state), fresh);
        let _ = window::store(state, &merged);
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
        // The human-readable report arrives in Task E5.
        None => {
            let _ = repo;
            Err("zirv ctx usage reporting is not implemented yet".into())
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
}
