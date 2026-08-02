use std::path::{Path, PathBuf};
use std::process::Command;

use super::super::CtxResult;
use super::super::event::{
    Capabilities, NormalizedEvent, SessionId, SessionRef, StructuralContext,
};
use super::{AgentAdapter, TurnSignalSetup};

/// Verified facts backing this adapter live in
/// `docs/superpowers/notes/2026-07-31-codex-cli-facts.md`. `parse_events` and
/// `structural_context` stay empty and `ready()` stays `Err` because that
/// file records the assistant/tool-call/token-usage event shapes and the
/// notify contract as unverified: codex requires authentication this branch
/// was not permitted to set up, and the notify mechanism this codex version
/// exposes (a `hooks` feature flag) does not match the plan's assumed
/// `notify = [...]` config array at all.
#[derive(Debug, Clone)]
pub struct CodexAdapter {
    program: String,
    bin_args: Vec<String>,
    home: Option<PathBuf>,
}

impl CodexAdapter {
    /// `bin` may carry arguments, so `"sh /tmp/stub.sh"` and
    /// `"/usr/bin/env codex"` both work, mirroring `ClaudeAdapter::new`.
    pub fn new(bin: Option<&str>) -> Self {
        let raw = bin.unwrap_or("codex").trim();
        let mut parts = raw.split_whitespace().map(str::to_string);
        let program = parts.next().unwrap_or_else(|| "codex".to_string());
        Self {
            program,
            bin_args: parts.collect(),
            home: None,
        }
    }

    /// Test seam: pins the home directory the transcript path is built from.
    #[cfg(test)]
    pub fn with_home(mut self, home: PathBuf) -> Self {
        self.home = Some(home);
        self
    }

    /// Every command starts here so the program and its leading arguments are
    /// applied uniformly to headless, interactive and distiller invocations.
    fn base(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.bin_args);
        cmd
    }

    fn home_dir(&self) -> PathBuf {
        self.home
            .clone()
            .or_else(|| crate::utils::home_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

/// Codex nests rollout files under `<sessions>/<YYYY>/<MM>/<DD>/`, a depth
/// `SessionRef` cannot predict (it carries only the session id, not the
/// session's start time), so the transcript is found by filename suffix
/// rather than a computed path.
fn find_rollout(dir: &Path, filename_suffix: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_rollout(&path, filename_suffix) {
                return Some(found);
            }
        } else if path.to_string_lossy().ends_with(filename_suffix) {
            return Some(path);
        }
    }
    None
}

impl AgentAdapter for CodexAdapter {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn ready(&self) -> CtxResult<()> {
        // Item 7: this is the surface a user actually sees (`--agent codex`,
        // or a config that names it), so it has to be honest in its own
        // words rather than pointing at an internal plan task number.
        Err(
            "codex support is not implemented yet; ctx currently supports Claude Code only. \
             Pass --agent claude, or track progress at \
             https://github.com/Glubiz/zirv-dynamic-cli/issues/11."
                .into(),
        )
    }

    fn detect(&self, command: &[String]) -> bool {
        command
            .first()
            .and_then(|p| Path::new(p).file_name())
            .map(|f| f.to_string_lossy() == "codex")
            .unwrap_or(false)
    }

    /// `codex exec` has no `--session-id` flag (verified): codex always mints
    /// its own session id, so `session` cannot appear in the built command.
    fn headless_cmd(&self, prompt: &str, _session: &SessionId, extra: &[String]) -> Command {
        let mut cmd = self.base();
        cmd.arg("exec").arg(prompt).args(extra);
        cmd
    }

    /// With no subcommand, `codex [PROMPT]` forwards straight to the
    /// interactive CLI (verified via `codex --help`), exactly like claude.
    fn interactive_cmd(&self, initial_prompt: Option<&str>, extra: &[String]) -> Command {
        let mut cmd = self.base();
        if let Some(prompt) = initial_prompt {
            cmd.arg(prompt);
        }
        cmd.args(extra);
        cmd
    }

    fn system_prompt_args(&self, _prompt: &str) -> Vec<String> {
        // No verified per-run mechanism (see
        // docs/superpowers/notes/2026-08-01-system-prompt-injection-facts.md).
        Vec::new()
    }

    /// Verified via `codex exec --help` (quoted verbatim in the notes file):
    /// `-m, --model <MODEL>` is a real flag, and the prompt is read from
    /// stdin when none is given as an argument, so the distillation prompt
    /// never hits an argv length limit.
    fn distiller_cmd(&self, model: &str) -> Command {
        let mut cmd = self.base();
        cmd.arg("exec").arg("--model").arg(model);
        cmd
    }

    fn launch_prefix_len(&self) -> usize {
        1 + self.bin_args.len()
    }

    fn transcript_path(&self, session: &SessionRef) -> PathBuf {
        let sessions_root = self.home_dir().join(".codex").join("sessions");
        let suffix = format!("-{}.jsonl", session.id);
        find_rollout(&sessions_root, &suffix)
            .unwrap_or_else(|| sessions_root.join(format!("rollout{suffix}")))
    }

    fn parse_events(&self, _jsonl: &str) -> Vec<NormalizedEvent> {
        Vec::new()
    }

    fn structural_context(&self, _jsonl: &str, _last_n: usize) -> StructuralContext {
        StructuralContext::default()
    }

    fn compact_command(&self) -> Option<&'static str> {
        None
    }

    fn quit_sequence(&self) -> &'static str {
        "/quit\r"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            marker_signal: false,
            token_usage: false,
            turn_signal: false,
            system_prompt: false,
        }
    }

    fn register_turn_signal(&self, _session: &SessionRef, _socket: &Path) -> TurnSignalSetup {
        TurnSignalSetup {
            env: Vec::new(),
            instructions: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::adapters::{AgentAdapter, select};

    #[test]
    fn codex_detects_its_own_binary() {
        let adapter = CodexAdapter::new(None);
        assert!(adapter.detect(&["/usr/local/bin/codex".to_string()]));
        assert!(!adapter.detect(&["/usr/local/bin/claude".to_string()]));
    }

    #[test]
    fn codex_has_no_marker_signal() {
        let caps = CodexAdapter::new(None).capabilities();
        assert!(!caps.marker_signal, "the spec gives codex no marker signal");
    }

    #[test]
    fn codex_ships_without_injection_until_a_mechanism_is_verified() {
        let adapter = CodexAdapter::new(None);
        assert!(
            adapter.system_prompt_args("be consistent").is_empty(),
            "no verified mechanism means no arguments, not a guessed flag"
        );
        assert!(!adapter.capabilities().system_prompt);
        assert_eq!(
            adapter.user_system_prompt_flag(),
            None,
            "nothing to merge when there is no flag at all"
        );
    }

    #[test]
    fn selecting_codex_before_it_is_verified_fails_loudly() {
        // Replaced by a success assertion in Task A10 once the parser exists.
        let err = select(Some("codex"), &[], None).expect_err("unverified adapter");
        assert!(err.to_string().contains("codex"), "got {err}");
    }

    /// Item 7: the error a user actually sees from `--agent codex` must be
    /// plain about codex not working yet, name Claude Code as what does work
    /// today, and point at the issue tracking it, rather than an internal
    /// plan task number nobody outside the repo can look up.
    #[test]
    fn ready_error_is_honest_about_codex_support_and_points_at_the_tracking_issue() {
        let err = CodexAdapter::new(None).ready().expect_err("not ready yet");
        let msg = err.to_string();
        assert!(
            msg.contains("not implemented yet"),
            "must plainly say codex is not implemented yet: {msg}"
        );
        assert!(
            msg.contains("Claude Code only"),
            "must say ctx currently supports Claude Code only: {msg}"
        );
        assert!(
            msg.contains("issues/11"),
            "must reference the tracking issue: {msg}"
        );
    }

    #[test]
    fn detecting_codex_argv_does_not_silently_fall_back_to_claude() {
        let cmd = vec!["codex".to_string(), "exec".to_string(), "do it".to_string()];
        let err = select(None, &cmd, None).expect_err("must not misroute to claude");
        assert!(err.to_string().contains("codex"), "got {err}");
    }

    /// Verified via `codex exec --help`: there is no `--session-id` flag, so
    /// the session parameter cannot appear in the built command at all.
    #[test]
    fn headless_cmd_uses_exec_and_has_no_session_flag() {
        let adapter = CodexAdapter::new(Some("/tmp/fake-codex"));
        let cmd = adapter.headless_cmd(
            "do the work",
            &SessionId::parse("abc"),
            &["--model".to_string(), "gpt-5.6-luna".to_string()],
        );
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(cmd.get_program().to_string_lossy(), "/tmp/fake-codex");
        assert_eq!(
            args,
            vec![
                "exec".to_string(),
                "do the work".to_string(),
                "--model".to_string(),
                "gpt-5.6-luna".to_string(),
            ],
            "codex exec takes no session flag; codex mints its own session id"
        );
    }

    /// Verified via `codex --help`: with no subcommand, the prompt goes
    /// straight to the interactive CLI, exactly like claude's positional form.
    #[test]
    fn interactive_cmd_passes_the_initial_prompt_positionally_with_no_subcommand() {
        let adapter = CodexAdapter::new(None);
        let with = adapter.interactive_cmd(Some("resume this"), &[]);
        assert_eq!(with.get_program().to_string_lossy(), "codex");
        let args: Vec<String> = with
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args, vec!["resume this".to_string()]);

        let without = adapter.interactive_cmd(None, &["--last".to_string()]);
        let args: Vec<String> = without
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args, vec!["--last".to_string()]);
    }

    /// Verified via `codex exec --help` (quoted verbatim in the notes file):
    /// `-m, --model <MODEL>` is a real flag, and the prompt is read from
    /// stdin when omitted, so the distiller never needs an argv prompt.
    #[test]
    fn distiller_cmd_uses_exec_with_a_cheap_model_and_reads_stdin() {
        let adapter = CodexAdapter::new(None);
        let cmd = adapter.distiller_cmd("gpt-5.6-luna");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec![
                "exec".to_string(),
                "--model".to_string(),
                "gpt-5.6-luna".to_string(),
            ]
        );
    }

    /// A multi-word agent bin (`ZIRV_CTX_AGENT_BIN="sh /tmp/stub.sh"`) must work
    /// the same way it does for claude, so `ZIRV_CTX_AGENT_BIN` behaves
    /// identically across adapters.
    #[test]
    fn a_multi_word_agent_bin_is_split_across_every_command_kind() {
        let adapter = CodexAdapter::new(Some("sh /tmp/stub.sh"));

        let headless = adapter.headless_cmd("go", &SessionId::parse("abc"), &[]);
        assert_eq!(headless.get_program().to_string_lossy(), "sh");
        let args: Vec<String> = headless
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec![
                "/tmp/stub.sh".to_string(),
                "exec".to_string(),
                "go".to_string()
            ],
            "the bin arguments come before the agent flags"
        );

        let interactive = adapter.interactive_cmd(Some("resume"), &[]);
        assert_eq!(interactive.get_program().to_string_lossy(), "sh");
        let args: Vec<String> = interactive
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args, vec!["/tmp/stub.sh".to_string(), "resume".to_string()]);
    }

    /// Codex nests rollouts under a date directory that `SessionRef` cannot
    /// predict (it carries only the id, not the session's start time), so
    /// `transcript_path` must scan for the id rather than compute the path.
    #[test]
    fn transcript_path_scans_the_dated_sessions_tree_for_the_session_id() {
        let home = tempfile::tempdir().expect("tempdir");
        let day_dir = home.path().join(".codex/sessions/2026/07/31");
        std::fs::create_dir_all(&day_dir).expect("mkdir");
        let expected =
            day_dir.join("rollout-2026-07-31T20-16-08-11111111-2222-4333-8444-555555555555.jsonl");
        std::fs::write(&expected, "").expect("write");

        let adapter = CodexAdapter::new(None).with_home(home.path().to_path_buf());
        let session = SessionRef {
            id: SessionId::parse("11111111-2222-4333-8444-555555555555"),
            cwd: std::path::PathBuf::from("/work/repo"),
        };
        assert_eq!(adapter.transcript_path(&session), expected);
    }
}
