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
//! detected by the monitor in the shell as *transitions* — a toast
//! fires once when the state is entered, never on each poll that
//! observes it. Discrete events (a step finishing, a download
//! completing) occur exactly once at their call site by construction.

use std::collections::HashMap;

use manager_supervisor::ProcessState;

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

/// Transition-watch state for the notification monitor (see
/// `notify_monitor_tick`). Held in `AppState` behind a std Mutex:
/// every access is a synchronous read/update of small fields.
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

/// One pass of the notification monitor (the shell runs it on a
/// 5-second loop): detect the state-driven transitions — a backend
/// dying unexpectedly, the gateway going down after being up, an
/// activation plan resolving — and fire each toast exactly once, on
/// entry into the state. Detection state lives in
/// `AppState.notify_watch`; baselines are set on first observation
/// and never fire.
pub async fn monitor_tick(state: &AppState, sink: &dyn EventSink) {
    let mut toasts: Vec<(Category, String, String)> = Vec::new();

    // -- Backends: a move into Failed from Running/Starting is an
    // unexpected exit. The reap runs here as elsewhere so a corpse is
    // marked Failed even between UI polls; which caller reaps first
    // does not matter, the Failed state persists in `status()`.
    let (statuses, dead) = {
        let mut sup = state.supervisor().await.lock().await;
        let dead: HashMap<String, String> = sup
            .reap_exited()
            .into_iter()
            .map(|(id, status)| (id, status.to_string()))
            .collect();
        (sup.status(), dead)
    };
    let backend_owned: std::collections::HashSet<String> = state
        .backend_spec_ids
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    {
        let mut watch = state.notify_watch.lock().unwrap_or_else(|e| e.into_inner());
        for st in &statuses {
            if !backend_owned.contains(&st.id) {
                continue;
            }
            let prev = watch.backend_states.insert(st.id.clone(), st.state);
            let was_active = matches!(
                prev,
                Some(ProcessState::Running) | Some(ProcessState::Starting)
            );
            if was_active && st.state == ProcessState::Failed {
                let body = match dead.get(&st.id) {
                    Some(status) => format!(
                        "Backend {} exited unexpectedly ({status}). It stays down until restarted from the Backends view.",
                        st.id
                    ),
                    None => format!(
                        "Backend {} is no longer running. It stays down until restarted from the Backends view.",
                        st.id
                    ),
                };
                toasts.push((
                    Category::Failures,
                    "Backend stopped unexpectedly".to_string(),
                    body,
                ));
            }
        }
    }

    // -- Gateway: probe liveness unless a wizard step is in flight
    // (a step may be mid-restart of the gateway itself; the step's
    // own outcome hook reports that case). Down fires only for a
    // gateway the app has seen up and did not stop itself, and only
    // after two consecutive missed probes.
    let step_running = state
        .wizard_step_running
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some();
    if !step_running {
        let (probe_host, port) = {
            let cfg = state.config.read().await;
            (
                manager_wizard::steps::gateway_probe_host(&cfg),
                cfg.gateway.port,
            )
        };
        let up = tokio::task::spawn_blocking(move || crate::probe_gateway(&probe_host, port))
            .await
            .unwrap_or(false);
        let mut watch = state.notify_watch.lock().unwrap_or_else(|e| e.into_inner());
        if up {
            watch.gateway_up = Some(true);
            watch.gateway_expected = true;
            watch.gateway_down_strikes = 0;
        } else {
            if watch.gateway_expected {
                watch.gateway_down_strikes = watch.gateway_down_strikes.saturating_add(1);
                if watch.gateway_down_strikes >= 2 && watch.gateway_up == Some(true) {
                    toasts.push((
                        Category::Failures,
                        "Gateway is down".to_string(),
                        format!(
                            "The LiteLLM gateway on port {port} stopped responding. Start it again from the Gateway view or the Setup Wizard."
                        ),
                    ));
                    watch.gateway_up = Some(false);
                    watch.gateway_down_strikes = 0;
                }
            } else {
                watch.gateway_up = Some(false);
                watch.gateway_down_strikes = 0;
            }
        }
    }

    // -- Activation plan: pending → resolved fires the applied
    // toast unless the resolution was a cancellation (flagged by the
    // cancel command) or the completion command already fired it.
    let (unresolved, drift) = crate::activation_unresolved(state).await;
    let pending = !unresolved.is_empty() || drift;
    {
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
