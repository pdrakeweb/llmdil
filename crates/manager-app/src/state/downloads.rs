//! Model-download subsystem state: live transfer handles, terminal
//! outcomes, the attempt-token bookkeeping that keeps a superseded
//! watcher from overwriting a newer attempt, and the set of models
//! whose post-download verification is in flight.
//!
//! Lock order (see the [`crate::state`] module docs): `config` is
//! always taken BEFORE `handles` (PS-1) — `handles` is never held
//! while waiting on the config lock. Within this subsystem the
//! readers that hold several locks at once take them in the order
//! `handles` → `outcomes` → `verifying` (`op_list_models`);
//! `latest_attempt` is never held across another acquisition.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use manager_models::DownloadHandle;
use tokio::sync::Mutex;

/// Terminal outcome of a model download, recorded by the completion
/// watcher spawned in `download_model`.
#[derive(Debug, Clone)]
pub(crate) struct DownloadOutcome {
    /// Token of the `download_model` attempt that produced this
    /// outcome (0 for outcomes recorded outside a download, i.e. a
    /// manual `model_verify_checksum` mismatch). The watcher writes
    /// only when its token is still the model's latest attempt, so a
    /// superseded download cannot overwrite a newer attempt's outcome.
    pub(crate) attempt: u64,
    /// The failure message when the download — or the hash verification
    /// that follows it — failed; `None` when it completed.
    pub(crate) failed: Option<String>,
    /// True when the failure is a SHA-256 verification failure
    /// (post-download watcher or manual verify). Only verification
    /// failures gate backend start in the preflight; transfer failures
    /// set this false and do not gate.
    pub(crate) verification_failure: bool,
}

/// The download subsystem's state (spec §14 download commands).
pub(crate) struct DownloadsState {
    /// Live model-download handles by model id.
    pub(crate) handles: Mutex<HashMap<String, DownloadHandle>>,
    /// Terminal download outcomes by model id, recorded by the
    /// completion watcher spawned in `download_model`: `failed` is the
    /// failure message, or `None` when the download completed (verified
    /// against the pinned hash when one exists). Read by `list_models`
    /// to surface `download_error` and by the backend start preflight
    /// to gate on verification failures.
    pub(crate) outcomes: Mutex<HashMap<String, DownloadOutcome>>,
    /// Monotonic download-attempt counter; each `download_model` call
    /// takes the next token (see [`DownloadOutcome::attempt`]).
    attempt_counter: AtomicU64,
    /// Latest download-attempt token per model id. The completion
    /// watcher writes its outcome only when its token still matches.
    latest_attempt: Mutex<HashMap<String, u64>>,
    /// Model ids whose post-download SHA-256 verification is currently
    /// running in the completion watcher. Backend start is refused for
    /// these: the file may be seconds from being deleted as corrupt.
    /// std Mutex like `backend_spec_ids` — only synchronous membership
    /// checks and inserts are needed.
    pub(crate) verifying: std::sync::Mutex<HashSet<String>>,
}

impl Default for DownloadsState {
    fn default() -> Self {
        Self {
            handles: Mutex::new(HashMap::new()),
            outcomes: Mutex::new(HashMap::new()),
            attempt_counter: AtomicU64::new(1),
            latest_attempt: Mutex::new(HashMap::new()),
            verifying: std::sync::Mutex::new(HashSet::new()),
        }
    }
}

impl DownloadsState {
    /// Take the next download-attempt token and record it as the
    /// model's latest attempt.
    pub(crate) async fn begin_attempt(&self, model_id: &str) -> u64 {
        let attempt = self.attempt_counter.fetch_add(1, Ordering::Relaxed);
        self.latest_attempt
            .lock()
            .await
            .insert(model_id.to_string(), attempt);
        attempt
    }

    /// Record a download outcome only if its attempt token is still
    /// the model's latest; a superseded watcher's outcome is dropped.
    pub(crate) async fn record_outcome(&self, model_id: &str, outcome: DownloadOutcome) {
        let latest = self.latest_attempt.lock().await.get(model_id).copied();
        if latest == Some(outcome.attempt) {
            self.outcomes
                .lock()
                .await
                .insert(model_id.to_string(), outcome);
        }
    }

    /// Clear a download outcome (e.g. cancellation) under the same
    /// latest-attempt gate as [`Self::record_outcome`].
    pub(crate) async fn clear_outcome(&self, model_id: &str, attempt: u64) {
        let latest = self.latest_attempt.lock().await.get(model_id).copied();
        if latest == Some(attempt) {
            self.outcomes.lock().await.remove(model_id);
        }
    }

    /// Drop handles that reached a terminal state (done, failed,
    /// cancelled); see [`crate::state::reap_downloads`].
    pub(crate) async fn reap(&self) {
        self.handles.lock().await.retain(|_, h| !h.is_terminal());
    }
}
