use hashbrown::HashMap;
use serde::Deserialize;
use std::process::{Child, Stdio};
use tokio::process::Command as TokioCommand;
use tokio::time::{Duration, sleep};

use crate::script_runner::Shell;

use super::options::Options;

/// Represents a single command in the YAML script.
#[derive(Debug, Deserialize, Clone)]
pub struct Command {
    /// The shell command to execute.
    pub command: String,
    /// Optional argument defines varable names to capture from the command output.
    pub capture: Option<String>,
    /// An optional description of what the command does.
    pub description: Option<String>,
    /// Optional options that control the behavior of the command.
    pub options: Option<Options>,
}
