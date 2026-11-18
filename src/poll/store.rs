use crate::poll::entity::{RawTask, Task, TaskStatus};
use crate::utils::to_ms;
use sqlx::SqlitePool;
use std::time::{Duration, SystemTime};

#[async_trait::async_trait]
pub trait TaskStore {
    /// Save a task to the store
    async fn save(&self, task: &Task) -> anyhow::Result<()>;

    /// Claim tasks due for execution
    ///
    /// Automatically claims up to `limit` tasks and marks them to `Running`
    ///
    /// A task can be claimed when it's `Pending` and due, or it is `Running`
    /// but its `lock_until` has expired (the worker that held it crashed). Each
    /// claim locks the task until `now + lease` and counts as one attempt.
    async fn claim_due(
        &self,
        now: SystemTime,
        lease: Duration,
        limit: u32,
    ) -> anyhow::Result<Vec<Task>>;

    /// The poll did not pass yet, put the task back to `Pending`
    /// due at `next_run_at`, and keep the error for inspection.
    async fn retry(
        &self,
        id: &str,
        error: &str,
        next_run_at: SystemTime,
        now: SystemTime,
    ) -> anyhow::Result<bool>;

    /// The deadline was reached without the poll passing. Mark `Timeout`
    async fn timed_out(&self, id: &str, error: &str, now: SystemTime) -> anyhow::Result<bool>;

    /// The poll passed: mark the task `Succeeded`.
    async fn succeed(&self, id: &str, now: SystemTime) -> anyhow::Result<bool>;

    /// An error that retrying won't fix: mark the task `Failed`.
    async fn failed(&self, id: &str, error: &str, now: SystemTime) -> anyhow::Result<bool>;
}

#[derive(Debug)]
pub struct SQLiteTaskStore {
    pool: SqlitePool,
}

impl SQLiteTaskStore {
    pub async fn new(pool: SqlitePool) -> anyhow::Result<Self> {
        schema(&pool).await?;
        Ok(Self { pool })
    }

    /// Move a `Running` task to a final status and release its lock.
    async fn finish(
        &self,
        id: &str,
        status: TaskStatus,
        error: Option<&str>,
        now: SystemTime,
    ) -> sqlx::Result<bool> {
        let res = sqlx::query(
            r#"
            UPDATE tasks
            SET status = ?, last_error = ?, lock_until = NULL, updated_at = ?
            WHERE id = ? AND status = 'running'
            "#,
        )
        .bind(status.as_str())
        .bind(error)
        .bind(to_ms(now))
        .bind(id)
        .execute(&self.pool)
        .await?;

        Ok(res.rows_affected() > 0)
    }
}

#[async_trait::async_trait]
impl TaskStore for SQLiteTaskStore {
    async fn save(&self, task: &Task) -> anyhow::Result<()> {
        let raw = RawTask::try_from(task)?;

        sqlx::query(
            r#"
            INSERT INTO tasks (id, name, spec, status, attempts, last_error,
                               next_run_at, deadline_at, lock_until, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                name        = excluded.name,
                spec        = excluded.spec,
                status      = excluded.status,
                attempts    = excluded.attempts,
                last_error  = excluded.last_error,
                next_run_at = excluded.next_run_at,
                deadline_at = excluded.deadline_at,
                lock_until  = excluded.lock_until,
                updated_at  = excluded.updated_at
            "#,
        )
        .bind(raw.id)
        .bind(raw.name)
        .bind(raw.spec)
        .bind(raw.status)
        .bind(raw.attempts)
        .bind(raw.last_error)
        .bind(raw.next_run_at)
        .bind(raw.deadline_at)
        .bind(raw.lock_until)
        .bind(raw.created_at)
        .bind(raw.updated_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn claim_due(
        &self,
        now: SystemTime,
        lease: Duration,
        limit: u32,
    ) -> anyhow::Result<Vec<Task>> {
        let now_ms = to_ms(now);

        let res = sqlx::query_as::<_, RawTask>(
            r#"
            UPDATE tasks
            SET status     = 'running',
                lock_until = ?,
                attempts   = attempts + 1,
                updated_at = ?
            WHERE id IN (
                SELECT id FROM tasks
                WHERE (status = 'pending' AND next_run_at <= ?)
                   OR (status = 'running' AND lock_until < ?)
                ORDER BY next_run_at
                LIMIT ?
            )
            RETURNING *
            "#,
        )
        .bind(to_ms(now + lease))
        .bind(now_ms)
        .bind(now_ms)
        .bind(now_ms)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Task::try_from)
        .collect::<Result<Vec<_>, sqlx::Error>>()?;

        Ok(res)
    }

    async fn retry(
        &self,
        id: &str,
        error: &str,
        next_run_at: SystemTime,
        now: SystemTime,
    ) -> anyhow::Result<bool> {
        let res = sqlx::query(
            r#"
            UPDATE tasks
            SET status = 'pending', last_error = ?, next_run_at = ?,
                lock_until = NULL, updated_at = ?
            WHERE id = ? AND status = 'running'
            "#,
        )
        .bind(error)
        .bind(to_ms(next_run_at))
        .bind(to_ms(now))
        .bind(id)
        .execute(&self.pool)
        .await?;

        Ok(res.rows_affected() > 0)
    }

    async fn timed_out(&self, id: &str, error: &str, now: SystemTime) -> anyhow::Result<bool> {
        Ok(self
            .finish(id, TaskStatus::TimedOut, Some(error), now)
            .await?)
    }

    async fn succeed(&self, id: &str, now: SystemTime) -> anyhow::Result<bool> {
        Ok(self.finish(id, TaskStatus::Succeeded, None, now).await?)
    }

    async fn failed(&self, id: &str, error: &str, now: SystemTime) -> anyhow::Result<bool> {
        Ok(self
            .finish(id, TaskStatus::Failed, Some(error), now)
            .await?)
    }
}

async fn schema(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS tasks (
            id          TEXT    PRIMARY KEY NOT NULL,
            name        TEXT    NOT NULL,
            spec        TEXT    NOT NULL,
            status      TEXT    NOT NULL CHECK (status IN
                            ('pending', 'running', 'succeeded', 'failed', 'timed_out')),
            attempts    INTEGER NOT NULL DEFAULT 0,
            last_error  TEXT,
            next_run_at INTEGER NOT NULL,
            deadline_at INTEGER NOT NULL,
            lock_until  INTEGER,
            created_at  INTEGER NOT NULL,
            updated_at  INTEGER NOT NULL
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_tasks_status_next_run ON tasks (status, next_run_at)",
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_tasks_name ON tasks (name)")
        .execute(pool)
        .await?;

    Ok(())
}
