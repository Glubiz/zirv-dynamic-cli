use std::io::Write;

use super::CtxResult;

#[derive(Debug, clap::Args)]
pub struct LoopArgs {
    /// Prompt to run each cycle.
    #[arg(long)]
    pub prompt: Option<String>,
}

pub fn run<W: Write>(_args: &LoopArgs, _w: &mut W) -> CtxResult<i32> {
    Err("zirv ctx loop is not implemented yet".into())
}
