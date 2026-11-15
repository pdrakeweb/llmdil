//! Task subsystem state: the task records, their per-task control
//! blocks (cancellation tokens and abort routing, see
//! [`crate::tasks`]), and the id counter.
//!
//! Lock order (see the [`crate::state`] module docs): the records
//! lock is held while the task's start event is published on the
//! event bus (tasks → event bus). `controls` uses its own internal
//! std lock and is never held across an await.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::Mutex;

use crate::{tasks, TaskRecord};

/// The tracked-task registry.
pub(crate) struct TaskRegistry {
    /// Task records by id.
    pub(crate) records: Mutex<HashMap<String, TaskRecord>>,
    /// Per-task cancellation tokens and abort routing (see
    /// [`crate::tasks`]).
    pub(crate) controls: tasks::TaskControls,
    counter: AtomicU64,
}

impl Default for TaskRegistry {
    fn default() -> Self {
        Self {
            records: Mutex::new(HashMap::new()),
            controls: tasks::TaskControls::default(),
            counter: AtomicU64::new(1),
        }
    }
}

impl TaskRegistry {
    /// The next task id (`task-000001`, ...).
    pub(crate) fn next_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("task-{n:06}")
    }
}
