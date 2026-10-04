//! bftools, command line tools for building Fowl Engine campaign missions.
//!
//! Currently there is one tool, `miz`, which builds the final mission file
//! from a base mission and a set of template missions, see
//! [`mission_edit::run`]. For example:
//!
//! ```text
//! bftools miz --output final.miz --base base.miz --weapon weapons.miz \
//!     --options options.miz --warehouse warehouse.miz
//! ```

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use serde_derive::Serialize;
use std::path::PathBuf;

mod mission_edit;

/// Arguments to the `miz` tool. All the inputs are .miz files.
#[derive(Args, Clone, Debug, Serialize)]
struct MizCmd {
    /// the final miz file to output
    #[clap(long)]
    output: PathBuf,
    /// the base mission file
    #[clap(long)]
    base: PathBuf,
    /// the weapon template
    #[clap(long)]
    weapon: PathBuf,
    /// the options template
    #[clap(long)]
    options: PathBuf,
    /// the warehouse template
    #[clap(long)]
    warehouse: Option<PathBuf>,
    /// the name of the invisible FARP whose warehouse holds blue's production
    #[clap(long, default_value = "BINVENTORY")]
    blue_production_template: String,
    /// the name of the invisible FARP whose warehouse holds red's production
    #[clap(long, default_value = "RINVENTORY")]
    red_production_template: String
}

/// The available tools, one subcommand each
#[derive(Subcommand, Clone, Debug, Serialize)]
enum Tools {
    Miz(MizCmd),
}

#[derive(Parser)]
struct BftoolsArgs {
    #[clap(subcommand)]
    tool: Tools,
}

fn main() -> Result<()> {
    let bftools_args = BftoolsArgs::parse();
    env_logger::init();

    match bftools_args.tool {
        Tools::Miz(cfg) => mission_edit::run(&cfg)?,
    };
    Ok(())
}
