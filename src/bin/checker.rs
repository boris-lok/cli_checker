use clap::Parser;
use cli::checker::config::{Config, PollWhen, Vars};
use cli::checker::executor::{CommandOutput, ShellCommandExecutor};
use cli::checker::runner::StepRunner;
use cli::checker::template::{UndefinedVariableError, render};
use cli::constants::DATABASE_PATH;
use cli::poll::entity::{Spec, Task, TaskStatus};
use cli::poll::store::{SQLiteTaskStore, TaskStore};
use cli::utils::create_database;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::SystemTime;

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
        .clone()
        .into_iter()
        .map(|(k, v)| Ok((k, render(&v, &m)?)))
        .collect::<Result<HashMap<_, _>, UndefinedVariableError>>()?;

    let executor = ShellCommandExecutor;
    let step_runner = StepRunner::new(executor, cli.verbose);
    for step in config.setups.iter() {
        step_runner.run(step, &mut ctx)?;
    }

    // we won't use `?` here, because we want to continue running teardown steps even if the run step fails
    let res = step_runner.run(&config.run, &mut ctx);

    for step in config.teardown.iter() {
        // ignore the result of teardown steps
        let _ = step_runner.run(step, &mut ctx);
    }

    let pool = create_database(&DATABASE_PATH).await?;
    let store = SQLiteTaskStore::new(pool).await?;
    maybe_schedule_poll(&store, &config, ctx, &res).await;

    Ok(())
}

/// Schedule a poll task if necessary.
/// Depends on `poll_when`
///
/// - If it is always, it will schedule a poll task.
/// - If it is run_passed, it will schedule a task when res is okay
/// - If it is run_failed, it will schedule a task when res is an error
async fn maybe_schedule_poll(
    store: &dyn TaskStore,
    config: &Config,
    ctx: Vars,
    res: &anyhow::Result<Option<CommandOutput>>,
) {
    let Some(poll) = config.poll.clone() else {
        return;
    };

    let should_poll = matches!(
        (&poll.when, res),
        (PollWhen::Always, _) | (PollWhen::RunPassed, Ok(_)) | (PollWhen::RunFailed, Err(_))
    );
    if !should_poll {
        return;
    }

    let now = SystemTime::now();
    let task = Task {
        id: uuid::Uuid::new_v4().to_string(),
        name: config.name.clone(),
        next_run_at: now + poll.interval,
        deadline_at: now + poll.max_duration,
        spec: Spec { vars: ctx, poll },
        status: TaskStatus::Pending,
        attempts: 0,
        last_error: None,
        lock_until: None,
        created_at: now,
        updated_at: now,
    };

    if let Err(e) = store.save(&task).await {
        eprintln!("failed to save poll task {e:#?}");
    }
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
