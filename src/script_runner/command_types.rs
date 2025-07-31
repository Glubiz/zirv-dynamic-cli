use super::command::Command;
use serde::{Deserialize, Serialize};

/// Represents either a single command or a group of commands
/// that should run sequentially in their own task.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(untagged)]
pub enum CommandTypes {
    /// A single command definition.
    Command(Command),
    /// A set of commands executed sequentially in a spawned task.
    Commands(Vec<Command>),
}
