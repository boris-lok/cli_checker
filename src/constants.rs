use std::path::PathBuf;
use std::sync::LazyLock;

pub static DATABASE_PATH: LazyLock<PathBuf> = LazyLock::new(|| PathBuf::from("/tmp/task.db"));
