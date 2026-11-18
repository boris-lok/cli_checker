use clap::Parser;
use cli::checker::executor::ShellCommandExecutor;
use cli::checker::runner::StepRunner;
use cli::constants::DATABASE_PATH;
use cli::poll::executor::{Config, PollExecutor};
use cli::poll::store::SQLiteTaskStore;
use cli::utils::create_database;
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let pool = create_database(&DATABASE_PATH).await?;
    let store = Arc::new(SQLiteTaskStore::new(pool).await?);
    let executor = ShellCommandExecutor;
    let step_runner = StepRunner::new(executor, true);
    let config = Config {
        lease: cli.lease,
        batch_size: cli.batch_size,
        idle_interval: cli.interval,
    };

    let executor = PollExecutor::new(store.clone(), step_runner, config);
    executor
        .run_until(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;

    Ok(())
}

#[derive(Debug, Parser)]
struct Cli {
    #[arg(long, value_parser = humantime::parse_duration, default_value = "10m")]
    lease: Duration,

    #[arg(long, value_parser = humantime::parse_duration, default_value = "10s")]
    interval: Duration,

    #[arg(long, default_value_t = 4)]
    batch_size: u8,
}
