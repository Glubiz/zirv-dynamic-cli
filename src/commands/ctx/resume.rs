use std::io::Write;

use super::CtxResult;

#[derive(Debug, clap::Args)]
pub struct ResumeArgs {
    #[arg(num_args = 0.., allow_hyphen_values = true)]
    pub rest: Vec<String>,
}

pub fn run<W: Write>(_args: &ResumeArgs, _w: &mut W) -> CtxResult<i32> {
    Err("zirv ctx resume is not implemented yet".into())
}
