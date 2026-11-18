use crate::checker::config::Poll;
use crate::utils::{from_ms, to_ms};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;
use std::time::SystemTime;

/// The poll spec
#[derive(Debug, Deserialize, Serialize)]
pub struct Spec {
    /// The variables to be used in the poll
    ///
    /// Usually, after we execute the Config.yaml, we use the variables to execute the poll
    #[serde(default)]
    pub vars: HashMap<String, String>,

    /// The poll to be executed
    pub poll: Poll,
}

pub struct Task {
    /// The Task ID
    pub id: String,

    /// The name from Confi.yaml
    pub name: String,

    /// The poll spec
    pub spec: Spec,

    /// The status of the task
    pub status: TaskStatus,

    /// The number of attempts made to execute the task
    pub attempts: u32,

    /// The last error that occurred while executing the task
    pub last_error: Option<String>,

    /// The next run at which the poll should be executed
    pub next_run_at: SystemTime,

    /// The deadline at which the poll should be executed
    ///
    /// created_at + spec.max_duration
    pub deadline_at: SystemTime,

    /// The time at which the task was locked
    pub lock_until: Option<SystemTime>,

    /// The time at which the task was created
    pub created_at: SystemTime,

    /// The time at which the task was last updated
    pub updated_at: SystemTime,
}

/// The status of the task
pub enum TaskStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    TimedOut,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::Running => "running",
            TaskStatus::Succeeded => "succeeded",
            TaskStatus::Failed => "failed",
            TaskStatus::TimedOut => "timed_out",
        }
    }
}

impl FromStr for TaskStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(TaskStatus::Pending),
            "running" => Ok(TaskStatus::Running),
            "succeeded" => Ok(TaskStatus::Succeeded),
            "failed" => Ok(TaskStatus::Failed),
            "timed_out" => Ok(TaskStatus::TimedOut),
            _ => Err(format!("Invalid task status: {}", s)),
        }
    }
}

/// The row as stored in SQLite
///
/// - `spec` ist the `Poll` serialized as YAML (reuses `serde_noway`)
/// - `status` is the task status as a string
/// - every duration is milliseconds
#[derive(Debug, sqlx::FromRow)]
pub struct RawTask {
    /// The Task ID
    pub id: String,

    /// The name from Confi.yaml
    pub name: String,

    /// The poll spec
    pub spec: String,

    /// The status of the task
    pub status: String,

    /// The number of attempts made to execute the task
    pub attempts: i64,

    /// The last error that occurred while executing the task
    pub last_error: Option<String>,

    /// The next run at which the poll should be executed
    pub next_run_at: i64,

    /// The deadline at which the poll should be executed
    ///
    /// created_at + spec.max_duration
    pub deadline_at: i64,

    /// The time at which the task was locked
    pub lock_until: Option<i64>,

    /// The time at which the task was created
    pub created_at: i64,

    /// The time at which the task was last updated
    pub updated_at: i64,
}

/// Implement TryFrom for RawTask to Task
impl TryFrom<RawTask> for Task {
    type Error = sqlx::Error;

    fn try_from(raw: RawTask) -> Result<Self, Self::Error> {
        Ok(Self {
            id: raw.id,
            name: raw.name,
            spec: serde_norway::from_str(&raw.spec)
                .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
            status: raw
                .status
                .parse()
                .map_err(|e: String| sqlx::Error::Decode(e.into()))?,
            attempts: u32::try_from(raw.attempts).map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
            last_error: raw.last_error,
            next_run_at: from_ms(raw.next_run_at),
            deadline_at: from_ms(raw.deadline_at),
            lock_until: raw.lock_until.map(from_ms),
            created_at: from_ms(raw.created_at),
            updated_at: from_ms(raw.updated_at),
        })
    }
}

/// Implement from reference `Task` to `RawTask`
impl TryFrom<&Task> for RawTask {
    type Error = sqlx::Error;

    fn try_from(task: &Task) -> Result<Self, Self::Error> {
        Ok(Self {
            id: task.id.clone(),
            name: task.name.clone(),
            spec: serde_norway::to_string(&task.spec)
                .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
            status: task.status.as_str().to_string(),
            attempts: i64::from(task.attempts),
            last_error: task.last_error.clone(),
            next_run_at: to_ms(task.next_run_at),
            deadline_at: to_ms(task.deadline_at),
            lock_until: task.lock_until.map(to_ms),
            created_at: to_ms(task.created_at),
            updated_at: to_ms(task.updated_at),
        })
    }
}
