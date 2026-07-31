use std::io::Write;

use super::CtxResult;

#[derive(Debug, clap::Args)]
pub struct WrapArgs {
    #[arg(num_args = 0.., allow_hyphen_values = true)]
    pub rest: Vec<String>,
}

pub fn run<W: Write>(_args: &WrapArgs, _w: &mut W) -> CtxResult<i32> {
    Err("zirv ctx wrap is not implemented yet".into())
}
