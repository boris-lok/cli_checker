use crate::checker::config::Vars;
use crate::checker::executor::CommandExecutor;
use crate::checker::runner::StepRunner;
use crate::poll::entity::{Spec, Task};
use crate::poll::store::TaskStore;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::task::JoinSet;

#[derive(Debug, Clone)]
pub struct Config {
    /// How long a claimed task stays locked. Must be longer than
    /// the slowest poll run, otherwise another worker re-claims a
    /// task that is still being processed.
    pub lease: Duration,

    /// Max tasks claimed (and run concurrently) per tick.
    pub batch_size: u8,

    /// How long to sleep when a tick found nothing to do.
    pub idle_interval: Duration,
}

/// Claims due tasks from the store, runs their polls and records the result
///
/// Outcome of one run:
/// - the poll passed                       -> `Succeeded`
/// - the poll failed, time is left         -> back the `Pending`, next_run_at = now + interval
/// - the poll failed, time is up           -> `TimedOut`
/// - the deadline had already passed       -> `TimedOut` (poll not run)
/// - the poll panicked                     -> `Failed` (retrying won't fix it)
pub struct PollExecutor<S, R>
where
    S: TaskStore + Send + Sync + 'static,
    R: CommandExecutor + 'static,
{
    store: Arc<S>,
    runner: Arc<StepRunner<R>>,
    config: Config,
}

impl<S, R> PollExecutor<S, R>
where
    S: TaskStore + Send + Sync + 'static,
    R: CommandExecutor + 'static,
{
    pub fn new(store: Arc<S>, runner: StepRunner<R>, config: Config) -> Self {
        Self {
            store,
            runner: Arc::new(runner),
            config,
        }
    }

    /// Run until `shutdown` resolves. The batch in flight is allowed to finish.
    pub async fn run_until(&self, shutdown: impl Future<Output = ()>) -> anyhow::Result<()> {
        tokio::pin!(shutdown);

        loop {
            let ran = tokio::select! {
                _ = &mut shutdown => return Ok(()),
                ran = self.tick() => ran?,
            };

            if ran == 0 {
                tokio::select! {
                    _ = &mut shutdown => return Ok(()),
                    _ = tokio::time::sleep(self.config.idle_interval) => {}
                }
            }
        }
    }

    /// Claim one batch of due tasks, run them concurrently and wait for all of
    /// them. Returns how many tasks were run.
    pub async fn tick(&self) -> anyhow::Result<usize> {
        let tasks = self
            .store
            .claim_due(
                SystemTime::now(),
                self.config.lease,
                self.config.batch_size as u32,
            )
            .await?;

        let count = tasks.len();

        let mut set = JoinSet::new();
        for task in tasks {
            let store = Arc::clone(&self.store);
            let runner = Arc::clone(&self.runner);
            set.spawn(async move { process(store.as_ref(), runner, task).await });
        }

        while let Some(joined) = set.join_next().await {
            if let Err(err) = joined {
                eprintln!("poll worker crashed: {err}");
            }
        }

        Ok(count)
    }
}

async fn process<S, R>(store: &S, runner: Arc<StepRunner<R>>, task: Task)
where
    S: TaskStore + Send + Sync + 'static,
    R: CommandExecutor + 'static,
{
    let Task {
        id,
        spec,
        deadline_at,
        ..
    } = task;

    if SystemTime::now() >= deadline_at {
        record(
            &id,
            store
                .timed_out(
                    &id,
                    "deadline passed before the poll ran",
                    SystemTime::now(),
                )
                .await,
        );
        return;
    }

    let interval = spec.poll.interval;
    // `StepRunner` blocks (child processes, thread::sleep), so keep it off the
    // async worker threads.
    let result = tokio::task::spawn_blocking(move || execute(&runner, &spec)).await;

    let now = SystemTime::now();
    let recorded = match result {
        Ok(Ok(())) => store.succeed(&id, now).await,
        Ok(Err(e)) => {
            let error = format!("{e:#}");
            let next_run_at = now + interval;
            if next_run_at >= deadline_at {
                store.timed_out(&id, &error, now).await
            } else {
                store.retry(&id, &error, next_run_at, now).await
            }
        }
        Err(join_err) => {
            store
                .failed(&id, &format!("poll panicked: {join_err}"), now)
                .await
        }
    };

    record(&id, recorded);
}

/// Execute the run in the poll spec.
fn execute<R: CommandExecutor>(runner: &StepRunner<R>, spec: &Spec) -> anyhow::Result<()> {
    let mut ctx: Vars = spec.vars.clone();
    runner.run(&spec.poll.run, &mut ctx)?;
    Ok(())
}

/// A lost update (`Ok(false)`) means the lease expired and another worker took
/// the task over; nothing to do but note it.
fn record(id: &str, res: anyhow::Result<bool>) {
    match res {
        Ok(true) => {}
        Ok(false) => eprintln!("task {id}: no longer running, result discarded"),
        Err(e) => eprintln!("task {id}: failed to record result: {e}"),
    }
}
