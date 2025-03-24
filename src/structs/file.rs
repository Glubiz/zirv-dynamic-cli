use clap::Parser;
use serde::Deserialize;

#[derive(Debug, Deserialize, Parser)]
pub struct File {
    /// A descriptive name for the script.
    pub name: String,
    /// Optional parameters (positional arguments) that will be mapped to the script's expected params.
    #[arg(num_args = 0..)]
    pub params: Vec<String>,
}
