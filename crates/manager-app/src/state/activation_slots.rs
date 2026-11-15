//! Activation-apply state: the one-apply-at-a-time lock and the
//! running apply's progress (see [`crate::activation_apply`]).
//!
//! The two locks stay separate (as they were as `AppState` fields):
//! `apply_lock` is a tokio lock taken with `try_lock` for the whole
//! apply; `progress` is a std lock only read/replaced synchronously.

use tokio::sync::Mutex;

use crate::activation_apply;

/// Activation plan apply serialization and progress.
#[derive(Default)]
pub(crate) struct ActivationState {
    /// Serializes activation plan applies ([`activation_apply`]): taken
    /// with `try_lock`, so a second apply from any surface is refused
    /// instead of interleaving restarts (ACT-9).
    pub(crate) apply_lock: Mutex<()>,
    /// What the running apply is doing (`activation_applying` on the
    /// gateway status and plan payloads); `None` when idle. std Mutex:
    /// synchronous read/replace only.
    pub(crate) progress: std::sync::Mutex<Option<activation_apply::ApplyProgress>>,
}
