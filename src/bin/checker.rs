use clap::Parser;
use cli::checker::config::Config;
use cli::checker::executor::ShellCommandExecutor;
use cli::checker::runner::StepRunner;
use cli::checker::template::{UndefinedVariableError, render};
use std::collections::HashMap;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Read the configuration file, and parse it into a Config struct
    let f = std::fs::File::open(cli.config_path)?;
    let config: Config = serde_norway::from_reader(std::io::BufReader::new(f))?;

    if cli.verify {
        println!("{}", serde_norway::to_string(&config)?);
        return Ok(());
    }

    let m = HashMap::new();
    let mut ctx = config
        .vars
        .into_iter()
        .map(|(k, v)| Ok((k, render(&v, &m)?)))
        .collect::<Result<HashMap<_, _>, UndefinedVariableError>>()?;

    let executor = ShellCommandExecutor;
    let step_runner = StepRunner::new(executor, cli.verbose);
    for step in config.setups.iter() {
        step_runner.run(step, &mut ctx)?;
    }

    // we won't use `?` here, because we want to continue running teardown steps even if the run step fails
    let _ = step_runner.run(&config.run, &mut ctx);

    for step in config.teardown.iter() {
        // ignore the result of teardown steps
        let _ = step_runner.run(step, &mut ctx);
    }

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
