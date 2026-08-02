use std::path::{Path, PathBuf};
use std::process::Command;

pub mod claude;
pub mod codex;

use super::CtxResult;
use super::event::{Capabilities, NormalizedEvent, SessionId, SessionRef, StructuralContext};

/// How an adapter arranges for turn-boundary events to reach a supervisor's
/// socket. `env` is injected into the launched agent so the hook that runs
/// inside it can find the socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSignalSetup {
    pub env: Vec<(String, String)>,
    pub instructions: String,
}

pub const SOCKET_ENV: &str = "ZIRV_CTX_SOCKET";
pub const SESSION_ENV: &str = "ZIRV_CTX_SESSION";

/// `Debug` is a supertrait so `Box<dyn AgentAdapter>` can appear in
/// `Result::expect_err` (the registry tests assert on the unknown-adapter
/// error path); every adapter already derives it.
pub trait AgentAdapter: std::fmt::Debug {
    fn name(&self) -> &'static str;

    /// `Err` when the adapter exists but is not safe to use yet, so callers
    /// fail loudly instead of scoring garbage.
    fn ready(&self) -> CtxResult<()>;

    fn detect(&self, command: &[String]) -> bool;

    fn headless_cmd(&self, prompt: &str, session: &SessionId, extra: &[String]) -> Command;
    fn interactive_cmd(&self, initial_prompt: Option<&str>, extra: &[String]) -> Command;
    fn distiller_cmd(&self, model: &str) -> Command;

    /// Arguments that add `prompt` to this agent's system prompt for one run.
    /// Empty when the agent has no verified mechanism, which is how an
    /// unsupported agent ships without injection rather than with a guess.
    fn system_prompt_args(&self, prompt: &str) -> Vec<String>;

    /// The user-facing flag name `system_prompt_args` emits, when the agent has
    /// one. Lets a caller find and merge a user's own use of the flag instead
    /// of silently overriding it with a second occurrence. `None` when the
    /// agent has no such flag, which is also the default: nothing to merge.
    fn user_system_prompt_flag(&self) -> Option<&'static str> {
        None
    }

    /// The user-facing flag name that delivers the composed prompt via a
    /// file path instead of argv text, when this agent has a verified one.
    /// `None` (the default) means: use `system_prompt_args`, which puts the
    /// prompt on argv instead.
    fn system_prompt_file_flag(&self) -> Option<&'static str> {
        None
    }

    /// Whether the binary about to be spawned advertises
    /// `system_prompt_file_flag` in its own `--help`. Probed rather than
    /// assumed: an adapter can know a flag's name and still find it missing
    /// from an older install.
    ///
    /// `launch` is the argv the caller is about to spawn, and the probe must
    /// hit exactly that program: `wrap` spawns the user's own argv, which can
    /// be an entirely different install from the one `agent_bin` names, and
    /// handing the file flag to a binary that does not have it fails the
    /// launch outright. An empty `launch` means the adapter's own program.
    ///
    /// `false` -- the default, and the fallback for any probe failure -- means
    /// argv delivery via `system_prompt_args`, never a blocked launch.
    fn supports_system_prompt_file(&self, launch: &[String]) -> bool {
        let _ = launch;
        false
    }

    /// How many leading argv tokens are the program invocation itself rather
    /// than flags the operator passed. One for a bare binary; more when
    /// `agent_bin` carries arguments, since `"/usr/bin/env claude"` spends two
    /// tokens before the first real flag. A relaunch rebuilds the invocation
    /// from `headless_cmd`, so anything inside this prefix must never be
    /// carried over as if the operator had asked for it.
    fn launch_prefix_len(&self) -> usize {
        1
    }

    fn transcript_path(&self, session: &SessionRef) -> PathBuf;

    /// Must be line-local: every line's events depend on that line alone, so
    /// parsing a transcript in pieces cut at newlines and concatenating the
    /// results is the same as parsing the whole of it. The incremental scoring
    /// path in `score.rs` feeds each adapter only the bytes appended since the
    /// last pass, and that is what makes it equal to a full parse.
    fn parse_events(&self, jsonl: &str) -> Vec<NormalizedEvent>;
    fn structural_context(&self, jsonl: &str, last_n: usize) -> StructuralContext;

    fn compact_command(&self) -> Option<&'static str>;
    fn quit_sequence(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    fn register_turn_signal(&self, session: &SessionRef, socket: &Path) -> TurnSignalSetup;
}

/// The program invocation at the head of an argv: the binary plus the leading
/// arguments before the first flag, which is what `sh wrapper.sh --foo` and
/// `/usr/bin/env claude -p x` both need. Anything past that is the operator's
/// own flags and has no business being passed to a `--help` probe.
pub fn program_invocation(launch: &[String]) -> Option<(String, Vec<String>)> {
    let (program, rest) = launch.split_first()?;
    let args = rest
        .iter()
        .take_while(|arg| !arg.starts_with('-'))
        .cloned()
        .collect();
    Some((program.clone(), args))
}

pub fn all(bin: Option<&str>) -> Vec<Box<dyn AgentAdapter>> {
    vec![
        Box::new(claude::ClaudeAdapter::new(bin)),
        Box::new(codex::CodexAdapter::new(bin)),
    ]
}

/// Explicit `--agent` name, else detection from the wrapped argv, else claude.
pub fn select(
    name: Option<&str>,
    command: &[String],
    bin: Option<&str>,
) -> CtxResult<Box<dyn AgentAdapter>> {
    let adapters = all(bin);

    if let Some(name) = name {
        let found = adapters.into_iter().find(|a| a.name() == name);
        let adapter = found.ok_or_else(|| {
            format!(
                "unknown agent '{name}'; known adapters: {}",
                all(None)
                    .iter()
                    .map(|a| a.name())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        adapter.ready()?;
        return Ok(adapter);
    }

    if let Some(adapter) = adapters.into_iter().find(|a| a.detect(command)) {
        adapter.ready()?;
        return Ok(adapter);
    }

    let adapter: Box<dyn AgentAdapter> = Box::new(claude::ClaudeAdapter::new(bin));
    adapter.ready()?;
    Ok(adapter)
}

/// True when the wrapped command can be trusted to actually be this adapter's
/// agent: either the operator named it explicitly (`--agent`, or the config's
/// `agent` key), or detection matched the command's own argv. Neither true
/// means `select`'s last arm defaulted here with nothing to back it up (an
/// arbitrary wrapped command that matches no adapter), and injecting this
/// adapter's own flags (e.g. `--append-system-prompt`) into whatever program
/// that turns out to be would leak them into its output instead of an agent
/// that would ever read them.
pub fn command_matches_adapter(
    adapter: &dyn AgentAdapter,
    agent_explicit: bool,
    command: &[String],
) -> bool {
    agent_explicit || adapter.detect(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M7 probed the adapter's own program while `wrap` spawned the user's
    /// argv, so the file flag could be handed to a binary that never
    /// advertised it -- failing the launch outright, which is the one thing
    /// the probe promises never to do. The probe target now comes from the
    /// argv about to be spawned, which means finding the invocation in it.
    #[test]
    fn the_program_invocation_stops_at_the_first_flag() {
        let argv =
            |parts: &[&str]| -> Vec<String> { parts.iter().map(|s| s.to_string()).collect() };

        assert_eq!(
            program_invocation(&argv(&["claude", "-p", "task"])),
            Some(("claude".to_string(), vec![]))
        );
        assert_eq!(
            program_invocation(&argv(&["/usr/bin/env", "claude", "-p", "task"])),
            Some(("/usr/bin/env".to_string(), vec!["claude".to_string()]))
        );
        assert_eq!(
            program_invocation(&argv(&["sh", "/opt/wrap.sh", "--model", "opus"])),
            Some(("sh".to_string(), vec!["/opt/wrap.sh".to_string()]))
        );
        assert_eq!(program_invocation(&[]), None, "nothing to probe");
    }

    #[test]
    fn explicit_name_wins() {
        let adapter = select(Some("claude"), &[], None).expect("claude selects");
        assert_eq!(adapter.name(), "claude");
    }

    #[test]
    fn detection_reads_the_wrapped_argv() {
        let cmd = vec![
            "/opt/homebrew/bin/claude".to_string(),
            "--resume".to_string(),
        ];
        let adapter = select(None, &cmd, None).expect("detect claude");
        assert_eq!(adapter.name(), "claude");
    }

    #[test]
    fn empty_command_defaults_to_claude() {
        let adapter = select(None, &[], None).expect("default");
        assert_eq!(adapter.name(), "claude");
    }

    #[test]
    fn unknown_name_is_an_error_that_lists_the_options() {
        let err = select(Some("gemini"), &[], None).expect_err("unknown agent");
        let msg = err.to_string();
        assert!(msg.contains("gemini"), "got {msg}");
        assert!(
            msg.contains("claude"),
            "error should list known adapters: {msg}"
        );
    }

    #[test]
    fn registry_exposes_both_v1_adapters() {
        let names: Vec<&str> = all(None).iter().map(|a| a.name()).collect();
        assert_eq!(names, vec!["claude", "codex"]);
    }

    /// The gate wrap and exec use before injecting: a command that matches no
    /// adapter, with no explicit `--agent` to back it, must not be treated as
    /// a match just because `select` had to default to one.
    #[test]
    fn an_undetected_command_with_no_explicit_agent_does_not_match() {
        let adapter = claude::ClaudeAdapter::new(None);
        let command = vec!["echo".to_string(), "hello".to_string()];
        assert!(!command_matches_adapter(&adapter, false, &command));
    }

    #[test]
    fn an_explicit_agent_matches_regardless_of_the_command() {
        let adapter = claude::ClaudeAdapter::new(None);
        let command = vec!["echo".to_string(), "hello".to_string()];
        assert!(command_matches_adapter(&adapter, true, &command));
    }

    #[test]
    fn a_detected_command_matches_even_without_an_explicit_agent() {
        let adapter = claude::ClaudeAdapter::new(None);
        let command = vec!["/opt/homebrew/bin/claude".to_string()];
        assert!(command_matches_adapter(&adapter, false, &command));
    }
}
