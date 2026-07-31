use std::io::Write;

use super::CtxResult;

#[derive(Debug, clap::Args)]
pub struct HandoffArgs {
    #[arg(num_args = 0.., allow_hyphen_values = true)]
    pub rest: Vec<String>,
}

pub fn run<W: Write>(_args: &HandoffArgs, _w: &mut W) -> CtxResult<i32> {
    Err("zirv ctx handoff is not implemented yet".into())
}
