use clap::Parser;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    Ok(())
}

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about,
    long_about = "A CLI tool for verifying/running configuration files

    This tool provides a command-line interface for verifying and running configuration files.

    Usage:

    checker [OPTIONS]

    Options:
        -v, --verbose       Enable verbose output
        -c, --config <PATH> Path to the configuration(YAML) file
        --verify            Verify the configuration(YAML) file

    Example
        - checker --config config.yaml --verify
        - checker --config config.yaml

    "
)]
struct Cli {
    #[arg(
        short = 'v',
        long,
        help = "Enable verbose output",
        default_value_t = false
    )]
    verbose: bool,

    #[arg(
        short = 'c',
        long = "config",
        help = "Path to the configuration(YAML) file"
    )]
    config_path: PathBuf,

    #[arg(
        long,
        help = "Verify the configuration(YAML) file",
        default_value_t = false
    )]
    verify: bool,
}
