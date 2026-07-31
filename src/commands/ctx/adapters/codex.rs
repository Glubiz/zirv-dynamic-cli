use std::path::{Path, PathBuf};
use std::process::Command;

use super::super::CtxResult;
use super::super::event::{
    Capabilities, NormalizedEvent, SessionId, SessionRef, StructuralContext,
};
use super::{AgentAdapter, TurnSignalSetup};

// Scaffold only: Task A9/A10 replace this once the codex CLI is verified.
#[derive(Debug, Clone)]
pub struct CodexAdapter {
    bin: String,
}

impl CodexAdapter {
    pub fn new(bin: Option<&str>) -> Self {
        Self {
            bin: bin.unwrap_or("codex").to_string(),
        }
    }
}

impl AgentAdapter for CodexAdapter {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn ready(&self) -> CtxResult<()> {
        Err(
            "the codex adapter is not verified yet (see plan task A9/A10); \
             pass --agent claude or wait for the codex parser"
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

    fn headless_cmd(&self, _prompt: &str, _session: &SessionId, _extra: &[String]) -> Command {
        Command::new(&self.bin)
    }

    fn interactive_cmd(&self, _initial_prompt: Option<&str>, _extra: &[String]) -> Command {
        Command::new(&self.bin)
    }

    fn distiller_cmd(&self, _model: &str) -> Command {
        Command::new(&self.bin)
    }

    fn transcript_path(&self, _session: &SessionRef) -> PathBuf {
        PathBuf::new()
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
        }
    }

    fn register_turn_signal(&self, _session: &SessionRef, _socket: &Path) -> TurnSignalSetup {
        TurnSignalSetup {
            env: Vec::new(),
            instructions: String::new(),
        }
    }
}
