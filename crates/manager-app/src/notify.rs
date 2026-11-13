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
//! Delivery is the shell's [`EventSink::toast`] capability (the Tauri
//! notification plugin, registered in the shell): the events that
//! matter (a backend dying, a download finishing at 3 a.m.) happen in
//! background tasks with no webview interaction, so the JS plugin API
//! could never fire them.
//!
//! Duplicate suppression is structural, not timed: state-driven
//! events (backend failure, gateway down, activation resolution) are
//! detected by the lifecycle monitor as *transitions* — a toast
//! fires once when the state is entered, never on each poll that
//! observes it. Discrete events (a step finishing, a download
//! completing) occur exactly once at their call site by construction.

use crate::lifecycle_monitor::{self, MonitorEvent};
use crate::{AppState, EventSink};

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

/// Send one toast through the sink, ignoring the operator's category
/// settings. This is the raw delivery path: [`notify`] gates on the
/// settings before calling it, and the Settings "Send test
/// notification" command calls it directly (the test exists to prove
/// the OS delivery path, so a master switch set to off must not make
/// the button look broken — the card states the settings separately).
pub fn send_toast(sink: &dyn EventSink, title: &str, body: &str) -> Result<(), String> {
    sink.toast(title, &truncate_body(body))
        .map_err(|e| format!("toast send failed: {e}"))
}

/// The choke point: fire a toast for `category` if the operator's
/// settings allow it. Send failures are logged, never propagated —
/// a notification problem must not fail the operation it reports on.
pub async fn notify(
    sink: &dyn EventSink,
    state: &AppState,
    category: Category,
    title: &str,
    body: &str,
) {
    // The transition is an app event whether or not the operator
    // wants a toast for it: offer it to the log shipper first.
    let level = matches!(category, Category::Failures).then_some(manager_logship::ShipLevel::Warn);
    crate::logship::offer_app_line(state, level, || {
        format!("notify ({category:?}): {title} — {body}")
    });
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
    if let Err(e) = send_toast(sink, title, body) {
        eprintln!("notification ({category:?}) not shown: {e}");
    }
}

/// Transition-watch state for the activation-plan resolution toast
/// (see [`monitor_tick`]). Service up/down detection lives in
/// [`crate::lifecycle_monitor`], driven by the lifecycle registry's
/// intent and transitions. Held in `AppState` behind a std Mutex:
/// every access is a synchronous read/update of small fields.
#[derive(Debug, Default)]
pub struct NotifyWatch {
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

/// Compose the backend-failure toast body. `exit_status` is the
/// exit the lifecycle monitor recorded for the crash (the reaped exit
/// string, or the Windows service's exit code); `reason` is the
/// classified cause (e.g. the device-unavailable fail-fast verdict,
/// or the service host's credential/spawn failure). When a reason is
/// known it is named as its own sentence: it states the cause, which
/// the exit string cannot. The recorded string carries no trailing
/// period, so the sentence is terminated here when it does not
/// already end with one.
fn backend_failure_body(id: &str, exit_status: Option<&str>, reason: Option<&str>) -> String {
    let mut body = match exit_status {
        Some(status) => format!("Backend {id} exited unexpectedly ({status})."),
        None => format!("Backend {id} is no longer running."),
    };
    if let Some(reason) = reason {
        body.push(' ');
        body.push_str(reason);
        if !body.ends_with('.') {
            body.push('.');
        }
    }
    body.push_str(" Restart it from the Backends view.");
    body
}

/// The toast (category, title, body) of one lifecycle monitor event.
fn monitor_toast(event: &MonitorEvent) -> (Category, String, String) {
    match event {
        MonitorEvent::Crashed {
            id,
            exit_status,
            reason,
        } => (
            Category::Failures,
            "Backend stopped unexpectedly".to_string(),
            backend_failure_body(id, exit_status.as_deref(), reason.as_deref()),
        ),
        MonitorEvent::GatewayDown { reason } => (
            Category::Failures,
            "Gateway is down".to_string(),
            format!("The LiteLLM gateway stopped ({reason}). Restart it from the Gateway view."),
        ),
        MonitorEvent::PostgresDown { reason } => (
            Category::Failures,
            "Database is down".to_string(),
            format!(
                "The gateway's PostgreSQL database stopped ({reason}). Restart the gateway from the Gateway view to bring it back."
            ),
        ),
        MonitorEvent::LogReceiverDown { exit_code, reason } => {
            let cause = match (reason, exit_code) {
                (Some(r), _) => format!(" ({r})"),
                (None, Some(c)) => format!(" (exit code {c})"),
                (None, None) => String::new(),
            };
            (
                Category::Failures,
                "Log receiver stopped".to_string(),
                format!(
                    "The log receiver service stopped unexpectedly{cause}. Restart it from Settings > Log receiver."
                ),
            )
        }
    }
}

/// One pass of the notification monitor (the shell runs it on a
/// 3-second loop): run the lifecycle monitor ([`lifecycle_monitor::tick`],
/// which detects crashes of every managed service against operator
/// intent and writes the markers and events), turn its events into
/// toasts, and fire the activation plan's applied toast when the plan
/// resolves. Each toast fires once, on entry into the state.
pub async fn monitor_tick(state: &AppState, sink: &dyn EventSink) {
    let mut toasts: Vec<(Category, String, String)> = lifecycle_monitor::tick(state, sink)
        .await
        .iter()
        .map(monitor_toast)
        .collect();

    // -- Activation plan: pending → resolved fires the applied
    // toast unless the resolution was a cancellation (flagged by the
    // cancel command) or the completion command already fired it.
    // While an activation apply or a gateway operation is in flight the
    // plan resolves piecemeal (the start-gateway step rewrites the
    // config before the new gateway is healthy): the watch is left
    // untouched, and the apply / the gateway completion fires the one
    // toast itself when it ends (CR-1a / ACT-6).
    if !activation_resolution_suppressed(state) {
        let (unresolved, drift) = crate::activation_unresolved(state).await;
        let pending = !unresolved.is_empty() || drift;
        let mut watch = state.notify_watch.lock().unwrap_or_else(|e| e.into_inner());
        match watch.activation_pending {
            None => watch.activation_pending = Some(pending),
            Some(false) if pending => {
                watch.activation_pending = Some(true);
                watch.activation_applied_notified = false;
                watch.activation_cancelled = false;
            }
            Some(true) if !pending => {
                watch.activation_pending = Some(false);
                if !watch.activation_cancelled && !watch.activation_applied_notified {
                    toasts.push((
                        Category::Activation,
                        "Activation plan applied".to_string(),
                        "All saved changes are now live.".to_string(),
                    ));
                }
            }
            _ => {}
        }
    }

    for (category, title, body) in toasts {
        notify(sink, state, category, &title, &body).await;
    }
}

/// Whether the monitor must not judge the activation plan's resolution
/// now: an activation apply is running, or a gateway lifecycle
/// operation is in flight.
pub(crate) fn activation_resolution_suppressed(state: &AppState) -> bool {
    crate::activation_apply::current_progress(state).is_some()
        || state
            .lifecycle
            .transition(crate::lifecycle::GATEWAY)
            .is_some()
}

#[cfg(test)]
mod tests {
    use super::{backend_failure_body, monitor_toast, Category, MonitorEvent};

    #[test]
    fn monitor_events_map_to_failure_toasts() {
        let (cat, title, body) = monitor_toast(&MonitorEvent::GatewayDown {
            reason: "process exited".into(),
        });
        assert_eq!(cat, Category::Failures);
        assert_eq!(title, "Gateway is down");
        assert_eq!(
            body,
            "The LiteLLM gateway stopped (process exited). Restart it from the Gateway view."
        );
        let (_, title, body) = monitor_toast(&MonitorEvent::LogReceiverDown {
            exit_code: Some(1066),
            reason: None,
        });
        assert_eq!(title, "Log receiver stopped");
        assert!(body.contains("(exit code 1066)"), "{body}");
        let (_, title, _) = monitor_toast(&MonitorEvent::PostgresDown {
            reason: "the server is not running".into(),
        });
        assert_eq!(title, "Database is down");
    }

    const GPU_REASON: &str =
        "GPU unavailable: the device was busy or unavailable while the backend loaded";

    #[test]
    fn failure_body_without_reason_is_the_original_copy() {
        assert_eq!(
            backend_failure_body("planner", Some("exit code: 1"), None),
            "Backend planner exited unexpectedly (exit code: 1). Restart it from the Backends view."
        );
        assert_eq!(
            backend_failure_body("planner", None, None),
            "Backend planner is no longer running. Restart it from the Backends view."
        );
    }

    #[test]
    fn failure_body_names_the_recorded_reason() {
        // The device-unavailable fail-fast case: the reason is
        // the operator-actionable fact and appears as its own
        // sentence in both reap shapes. The recorded string has
        // no trailing period; the helper terminates the sentence.
        assert_eq!(
            backend_failure_body("planner-moe", Some("exit code: 1"), Some(GPU_REASON)),
            "Backend planner-moe exited unexpectedly (exit code: 1). GPU unavailable: the device was busy or unavailable while the backend loaded. Restart it from the Backends view."
        );
        assert_eq!(
            backend_failure_body("planner-moe", None, Some(GPU_REASON)),
            "Backend planner-moe is no longer running. GPU unavailable: the device was busy or unavailable while the backend loaded. Restart it from the Backends view."
        );
    }
}
