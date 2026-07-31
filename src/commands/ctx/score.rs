use std::io::Write;
use std::path::PathBuf;

use super::CtxResult;

#[derive(Debug, clap::Args)]
pub struct ScoreArgs {
    /// Path to the agent transcript (JSONL).
    #[arg(long)]
    pub transcript: PathBuf,
    /// Adapter name: claude or codex. Defaults to config, then claude.
    #[arg(long)]
    pub agent: Option<String>,
}

pub fn run<W: Write>(_args: &ScoreArgs, _w: &mut W) -> CtxResult<i32> {
    Err("zirv ctx score is not implemented yet".into())
}
