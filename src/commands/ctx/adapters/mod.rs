// Consumed by verb entry points added in later tasks of this plan; nothing
// calls this yet outside tests, so dead_code is silenced module-wide (this
// also covers the claude/codex submodules) until then.
#![allow(dead_code)]

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

    fn transcript_path(&self, session: &SessionRef) -> PathBuf;
    fn parse_events(&self, jsonl: &str) -> Vec<NormalizedEvent>;
    fn structural_context(&self, jsonl: &str, last_n: usize) -> StructuralContext;

    fn compact_command(&self) -> Option<&'static str>;
    fn quit_sequence(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    fn register_turn_signal(&self, session: &SessionRef, socket: &Path) -> TurnSignalSetup;
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
