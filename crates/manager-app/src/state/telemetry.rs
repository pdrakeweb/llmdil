//! Telemetry subsystem state: the shared GPU telemetry store, the
//! handle that keeps its poll loop alive, and the persistent GPU
//! history cell. The lazy initializers live on
//! [`crate::AppState::telemetry`] and [`crate::AppState::gpu_history`]
//! because they read the configuration and the data dir.
//!
//! Lock order (see the [`crate::state`] module docs): the first
//! `AppState::telemetry()` call reads the config, so it must never
//! run under the config write lock. The store's std lock is never
//! held across an await.

use std::sync::Arc;

use manager_telemetry::TelemetryStore;
use tokio::sync::OnceCell;

use crate::gpu_history;

/// GPU telemetry and history.
#[derive(Default)]
pub(crate) struct TelemetryState {
    /// The telemetry store is owned by the poll loop (single writer);
    /// the Arc is shared with the Poller built in `telemetry()`. The
    /// std Mutex is the store's own lock type from manager-telemetry.
    pub(crate) store: OnceCell<Arc<std::sync::Mutex<TelemetryStore>>>,
    /// Keeps the telemetry poll loop alive for the app's lifetime.
    pub(crate) poller: OnceCell<std::sync::Mutex<manager_telemetry::store::PollHandle>>,
    /// Persistent per-GPU telemetry history (see [`gpu_history`]):
    /// loaded from the data dir on first use, appended by the
    /// recorder loop, read by the history query op.
    pub(crate) gpu_history: OnceCell<std::sync::Mutex<gpu_history::GpuHistoryCell>>,
}

impl TelemetryState {
    /// The store, if it has been initialized (or pinned); never
    /// starts the poll loop.
    pub(crate) fn get(&self) -> Option<&Arc<std::sync::Mutex<TelemetryStore>>> {
        self.store.get()
    }

    /// Pin the store before first use, so `AppState::telemetry()`
    /// returns it instead of starting the host poller (tests and
    /// fixtures). Fails, returning the store, when one is already
    /// set.
    #[cfg(test)]
    pub(crate) fn set(
        &self,
        store: Arc<std::sync::Mutex<TelemetryStore>>,
    ) -> Result<(), tokio::sync::SetError<Arc<std::sync::Mutex<TelemetryStore>>>> {
        self.store.set(store)
    }
}
