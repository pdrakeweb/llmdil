//! Windows toast notifications (Settings → Notifications).
//!
//! Every event toast in the app goes through [`notify`], the single
//! choke point that reads the operator's `NotificationConfig` (master
//! switch + per-category switch) before anything is shown. Call sites
//! supply only the category and finished copy; gating, body
//! truncation, and send-failure logging live here, so no hook can
//! accidentally bypass the settings or spam a raw multi-kilobyte
//! message into a toast.
//!
//! Delivery is the Tauri notification plugin (registered in `main`),
//! invoked from Rust: the events that matter (a backend dying, a
//! download finishing at 3 a.m.) happen in background tasks with no
//! webview interaction, so the JS plugin API could never fire them.
//!
//! Duplicate suppression is structural, not timed: state-driven
//! events (backend failure, gateway down, activation resolution) are
//! detected by the monitor in `main` as *transitions* — a toast fires
//! once when the state is entered, never on each poll that observes
//! it. Discrete events (a step finishing, a download completing)
//! occur exactly once at their call site by construction.

use std::collections::HashMap;

use manager_supervisor::ProcessState;
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

use crate::AppState;

/// Toast event categories; each maps to one switch in
/// `NotificationConfig` (and one toggle in Settings → Notifications).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// Long-running work finishing: setup step runs, model downloads.
    Operations,
    /// Things breaking: backends stopping unexpectedly, the gateway
    /// going down or failing to start, step failures, download
    /// failures.
    Failures,
    /// An activation plan finishing applying, or failing to apply.
    Activation,
}

/// Longest toast body handed to the OS. Windows renders roughly two
/// lines; anything longer is cut at a character boundary with an
/// ellipsis rather than arriving as a wall of text.
const MAX_BODY_CHARS: usize = 240;

fn truncate_body(body: &str) -> String {
    if body.chars().count() <= MAX_BODY_CHARS {
        return body.to_string();
    }
    let cut: String = body.chars().take(MAX_BODY_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

/// Send one toast through the plugin, ignoring the operator's
/// category settings. This is the raw delivery path: [`notify`] gates
/// on the settings before calling it, and the Settings "Send test
/// notification" command calls it directly (the test exists to prove
/// the OS delivery path, so a master switch set to off must not make
/// the button look broken — the card states the settings separately).
pub fn send_toast(app: &AppHandle, title: &str, body: &str) -> Result<(), String> {
    app.notification()
        .builder()
        .title(title)
        .body(truncate_body(body))
        .show()
        .map_err(|e| format!("toast send failed: {e}"))
}

/// The choke point: fire a toast for `category` if the operator's
/// settings allow it. Send failures are logged, never propagated —
/// a notification problem must not fail the operation it reports on.
pub async fn notify(
    app: &AppHandle,
    state: &AppState,
    category: Category,
    title: &str,
    body: &str,
) {
    let allowed = {
        let cfg = state.config.read().await;
        match category {
            Category::Operations => cfg.notifications.operations_enabled(),
            Category::Failures => cfg.notifications.failures_enabled(),
            Category::Activation => cfg.notifications.activation_enabled(),
        }
    };
    if !allowed {
        return;
    }
    if let Err(e) = send_toast(app, title, body) {
        eprintln!("notification ({category:?}) not shown: {e}");
    }
}

/// Transition-watch state for the notification monitor (see
/// `notify_monitor_tick` in `main`). Held in `AppState` behind a std
/// Mutex: every access is a synchronous read/update of small fields.
#[derive(Debug, Default)]
pub struct NotifyWatch {
    /// Last observed supervisor state per backend-owned process id.
    /// A move into `Failed` from `Running`/`Starting` is the
    /// unexpected-exit event; the baseline (first observation) never
    /// fires, so a backend already dead at launch raises nothing.
    pub backend_states: HashMap<String, ProcessState>,
    /// Last gateway probe result (`None` until the first probe).
    pub gateway_up: Option<bool>,
    /// The gateway has been observed up since launch and the app has
    /// not stopped it since. Only an expected gateway going down is
    /// a failure event; one that was never up is just off.
    pub gateway_expected: bool,
    /// Consecutive down probes while expected. The down toast fires
    /// at 2: a single 500 ms probe miss (a gateway mid-restart, a
    /// loaded machine) must not cry wolf.
    pub gateway_down_strikes: u8,
    /// Last computed activation-plan pending state (`None` until the
    /// monitor's first computation, which only sets the baseline).
    pub activation_pending: Option<bool>,
    /// Set by the `gateway_activation_complete` hook when it fires
    /// the applied toast, so the monitor does not fire a second one
    /// when it observes the same resolution.
    pub activation_applied_notified: bool,
    /// Set by `gateway_activation_cancel` when the operator cancels
    /// a plan: the monitor's pending→resolved transition that follows
    /// is a cancellation, not an application, and stays silent.
    pub activation_cancelled: bool,
}
