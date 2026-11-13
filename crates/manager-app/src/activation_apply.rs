//! The one server-side activation plan apply (service-lifecycle design
//! WP7; findings ACT-1, ACT-3, ACT-5, ACT-6, ACT-9, GW-7, CR-6).
//!
//! [`op_activation_apply`] replaces both the desktop UI's
//! `applyActivationPlan` orchestration and the REST composite. One
//! apply:
//!
//! 1. takes the apply lock (`AppState::activation_apply_lock`) with
//!    `try_lock`: a second apply, from any surface, is refused with
//!    [`ApiError::Busy`] naming the running one (ACT-9);
//! 2. re-reads the plan server-side (never a stale client copy, ACT-3b);
//! 3. restarts each stale backend through the shared
//!    [`backend_lifecycle::restart_backend`] (origin `activation`,
//!    graceful, waiting for `/health`, ACT-6), stopping at the first
//!    failure;
//! 4. when a gateway change is pending, restarts the gateway through
//!    [`crate::op_gateway_restart`] (origin `activation`; it never stops
//!    PostgreSQL, and only a Done outcome counts as success, GW-7/ACT-1);
//! 5. records ONE activation-history execution with the real source
//!    (`desktop`, `api` or `cli`) and one item per plan change, with the
//!    same outcome/detail strings the desktop funnel used;
//! 6. publishes its progress in `AppState::activation_progress` for the
//!    status and plan payloads (`activation_applying`), cleared on every
//!    path out by a drop guard.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::activation_history::{self, ActivationExecutionItem};
use crate::backend_lifecycle::{self, BackendOpOpts};
use crate::lifecycle::{self, LifecycleState, Origin};
use crate::{notify, ApiError, ApiResult, AppState, EventSink};

/// How long one backend restart waits for `/health` before the apply
/// reports it failed.
const BACKEND_READY_TIMEOUT: Duration = Duration::from_secs(600);

/// Detail of an item the apply never reached.
const NOT_ATTEMPTED: &str = "Not attempted — the apply did not reach this item";
/// Detail of a backend-scope item whose backends are stopped.
const ALREADY_EFFECTIVE: &str = "Already effective — takes effect at the backend's next start";

/// What an in-flight apply is doing, as `activation_applying` on the
/// gateway status and activation plan payloads (`null` when idle).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApplyProgress {
    /// The surface that started the apply (`desktop`, `api`, `cli`).
    pub source: String,
    /// The apply mode as requested (`quiesce`, `drain`, `restart`, `now`,
    /// `start`).
    pub mode: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// `plan`, `backend` or `gateway`.
    pub step: String,
    /// The backend id or `gateway` being restarted; `None` while the
    /// plan is read.
    pub target: Option<String>,
    /// Operator-facing progress line ("Restarting backend planner…").
    pub message: String,
}

/// The apply mode: drain in-flight gateway requests first, or restart
/// at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyMode {
    Drain,
    Now,
}

/// Normalize the requested mode: `drain`/`quiesce` drain first;
/// `now`/`restart`/`start` (and no mode) restart at once. Returns the
/// mode and the spelling recorded in the history (the requested one,
/// lowercased, so the Activation Plan view's labels keep working).
pub fn parse_mode(mode: Option<&str>) -> Result<(ApplyMode, String), ApiError> {
    let raw = mode.map(|m| m.trim().to_ascii_lowercase());
    match raw.as_deref() {
        None | Some("") => Ok((ApplyMode::Now, "now".to_string())),
        Some(m @ ("drain" | "quiesce")) => Ok((ApplyMode::Drain, m.to_string())),
        Some(m @ ("now" | "restart" | "start")) => Ok((ApplyMode::Now, m.to_string())),
        Some(other) => Err(ApiError::InvalidInput(format!(
            "unknown apply mode `{other}`: expected \"drain\" or \"now\""
        ))),
    }
}

/// The initiating surface recorded in the history: `desktop` (the
/// default — the desktop shell's invocation), `api` or `cli`.
pub fn parse_source(source: Option<&str>) -> Result<String, ApiError> {
    let raw = source.map(|s| s.trim().to_ascii_lowercase());
    match raw.as_deref() {
        None | Some("") => Ok("desktop".to_string()),
        Some(s @ ("desktop" | "api" | "cli")) => Ok(s.to_string()),
        Some(other) => Err(ApiError::InvalidInput(format!(
            "unknown apply source `{other}`: expected \"desktop\", \"api\" or \"cli\""
        ))),
    }
}

/// One change of the plan as the apply read it (the
/// `gateway_activation_plan` payload's `changes[]` entry).
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct PlanChange {
    #[serde(default)]
    pub ts: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub targets: Vec<String>,
    #[serde(default)]
    pub activation: String,
}

/// The plan state the apply acts on.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct PlanSnapshot {
    #[serde(default)]
    pub gateway_up: bool,
    #[serde(default)]
    pub gateway_pending: bool,
    #[serde(default)]
    pub drift: bool,
    #[serde(default)]
    pub backend_restart_ids: Vec<String>,
    #[serde(default)]
    pub changes: Vec<PlanChange>,
}

/// The history items of a plan before anything ran: every change an
/// unattempted skip. Each step overwrites the items it acts on, so an
/// abort leaves an accurate trail with no cleanup pass. Pure.
pub(crate) fn plan_items(changes: &[PlanChange]) -> Vec<ActivationExecutionItem> {
    changes
        .iter()
        .map(|c| ActivationExecutionItem {
            ts: c.ts.clone(),
            kind: c.kind.clone(),
            summary: c.summary.clone(),
            scope: c.scope.clone(),
            targets: c.targets.clone(),
            activation: c.activation.clone(),
            outcome: "skipped".to_string(),
            detail: Some(NOT_ATTEMPTED.to_string()),
        })
        .collect()
}

fn applied(item: &mut ActivationExecutionItem) {
    item.outcome = "applied".to_string();
    item.detail = None;
}

/// Backend `failed_id`'s restart failed with `error`: the apply stops
/// here. Items of the failed backend fail; items whose backends all
/// restarted already are applied; items waiting on a later restart are
/// unattempted; the rest were already effective. Gateway items note
/// that the apply never reached the gateway. Pure.
pub(crate) fn mark_backend_failure(
    items: &mut [ActivationExecutionItem],
    failed_id: &str,
    error: &str,
    restart_ids: &[String],
    restarted: &HashSet<String>,
    gateway_pending: bool,
) {
    for item in items.iter_mut() {
        if item.scope != "backend" {
            if gateway_pending {
                item.detail = Some(
                    "Not attempted — the apply stopped before the gateway restart".to_string(),
                );
            }
            continue;
        }
        if item.targets.iter().any(|t| t == failed_id) {
            item.outcome = "failed".to_string();
            item.detail = Some(error.to_string());
        } else if !item.targets.is_empty() && item.targets.iter().all(|t| restarted.contains(t)) {
            applied(item);
        } else if item.targets.iter().any(|t| restart_ids.contains(t)) {
            item.detail = Some(
                "Not attempted — the apply stopped after a failed backend restart".to_string(),
            );
        } else {
            item.detail = Some(ALREADY_EFFECTIVE.to_string());
        }
    }
}

/// Every named backend restarted: restart-class backend items are
/// applied; the rest were already effective (their backends are
/// stopped — the saved change is what the next start launches with).
/// Pure.
pub(crate) fn mark_backends_restarted(
    items: &mut [ActivationExecutionItem],
    restarted: &HashSet<String>,
) {
    for item in items.iter_mut() {
        if item.scope != "backend" {
            continue;
        }
        if item.targets.iter().any(|t| restarted.contains(t)) {
            applied(item);
        } else {
            item.detail = Some(ALREADY_EFFECTIVE.to_string());
        }
    }
}

/// The gateway restart ended: every non-backend item is applied, or
/// failed with `error`. Pure.
pub(crate) fn mark_gateway_result(items: &mut [ActivationExecutionItem], ok: bool, error: &str) {
    for item in items.iter_mut() {
        if item.scope == "backend" {
            continue;
        }
        if ok {
            applied(item);
        } else {
            item.outcome = "failed".to_string();
            item.detail = Some(if error.is_empty() {
                "The gateway restart did not complete".to_string()
            } else {
                error.to_string()
            });
        }
    }
}

/// The execution status after the gateway step (the last step: reaching
/// it means every step was attempted, so a failure there completes the
/// execution with failures rather than aborting it). Pure.
pub(crate) fn completed_status(items: &[ActivationExecutionItem]) -> &'static str {
    if items.iter().any(|i| i.outcome == "failed") {
        "completed_with_failures"
    } else {
        "completed"
    }
}

/// Clears `AppState::activation_progress` on every path out of the
/// apply, including an early `?` or a panic unwinding through it.
struct ProgressGuard<'a> {
    state: &'a AppState,
}

impl ProgressGuard<'_> {
    fn set(&self, progress: ApplyProgress) {
        *self
            .state
            .activation_progress
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(progress);
    }
}

impl Drop for ProgressGuard<'_> {
    fn drop(&mut self) {
        *self
            .state
            .activation_progress
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// The running apply's progress, `None` when no apply is in flight.
pub fn current_progress(state: &AppState) -> Option<ApplyProgress> {
    state
        .activation_progress
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// The [`ApiError::Busy`] message of a refused second apply.
pub(crate) fn busy_message(progress: Option<&ApplyProgress>) -> String {
    match progress {
        Some(p) => {
            let secs = (chrono::Utc::now() - p.started_at).num_seconds().max(0);
            format!(
                "an activation apply started by {} is running (step {}: {}; {secs}s so far)",
                p.source, p.step, p.message
            )
        }
        None => "an activation apply is already running".to_string(),
    }
}

/// The first line of an error text (history details are one line).
fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Apply the pending activation plan server-side: restart the plan's
/// stale backends, then — when a gateway change is pending — restart
/// the gateway (draining in-flight requests first in `drain` mode), and
/// record the execution in the activation history. See the module docs.
///
/// `mode`: `drain`|`quiesce` or `now`|`restart`|`start` (default `now`).
/// `source`: `desktop` (default), `api` or `cli`.
///
/// Errors: [`ApiError::InvalidInput`] for an unknown mode or source,
/// [`ApiError::Busy`] while another apply runs. A failed step is NOT an
/// error: the payload reports it (`ok: false`, `status`, `message`,
/// `manual_command`) and the execution is recorded.
///
/// Payload: `{ok, status, message, manual_command?, mode, source,
/// restarted_backends, gateway_restarted, quiesce?, execution_id?,
/// items}`. `status` is `completed`, `completed_with_failures`,
/// `failed`, or `nothing_pending` (nothing was pending; nothing ran and
/// nothing is recorded).
pub async fn op_activation_apply(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    mode: Option<String>,
    source: Option<String>,
) -> ApiResult {
    let (apply_mode, mode_label) = parse_mode(mode.as_deref())?;
    let source = parse_source(source.as_deref())?;
    let _lock = match state.activation_apply_lock.try_lock() {
        Ok(g) => g,
        Err(_) => {
            return Err(ApiError::Busy(busy_message(
                current_progress(state).as_ref(),
            )));
        }
    };
    let started_at = chrono::Utc::now();
    // The plan is read before the progress is published: an apply in
    // progress makes the plan treat the gateway as up, which must not
    // decide what THIS apply does (a down gateway's changes apply at
    // its next start, not by this apply starting it).
    let plan_value = crate::op_gateway_activation_plan(state, sink).await?;
    let plan: PlanSnapshot = serde_json::from_value(plan_value).unwrap_or_default();
    let progress = ProgressGuard { state };
    let make_progress = |step: &str, target: Option<&str>, message: String| ApplyProgress {
        source: source.clone(),
        mode: mode_label.clone(),
        started_at,
        step: step.to_string(),
        target: target.map(str::to_string),
        message,
    };
    progress.set(make_progress(
        "plan",
        None,
        "Reading the activation plan…".to_string(),
    ));

    let restart_ids = plan.backend_restart_ids.clone();
    if restart_ids.is_empty() && !plan.gateway_pending {
        return Ok(json!({
            "ok": true,
            // Older REST clients read `applied`.
            "applied": true,
            "status": "nothing_pending",
            "message": "No saved changes need a restart to go live.",
            "mode": mode_label,
            "source": source,
            "restarted_backends": [],
            "gateway_restarted": false,
            "items": [],
        }));
    }

    // The apply fires the one "applied" toast itself at its end (the
    // monitor stays quiet while it runs); a stale flag from an earlier
    // resolution must not swallow it.
    {
        let mut watch = state.notify_watch.lock().unwrap_or_else(|e| e.into_inner());
        watch.activation_applied_notified = false;
    }

    let mut items = plan_items(&plan.changes);
    let mut restarted: HashSet<String> = HashSet::new();
    let mut restarted_order: Vec<String> = Vec::new();
    let mut message = String::new();
    let mut manual_command: Option<String> = None;
    let mut quiesce: Option<Value> = None;
    let mut gateway_restarted = false;
    let mut status: &'static str = "completed";
    let mut failure_toast: Option<String> = None;

    let mut backend_failed = false;
    for id in &restart_ids {
        let line = format!("Restarting backend {id}…");
        progress.set(make_progress("backend", Some(id), line.clone()));
        lifecycle::emit(
            sink.as_ref(),
            &lifecycle::backend_key(id),
            "applying",
            LifecycleState::Restarting,
            Some(Origin::Activation),
            Some(&line),
        );
        let opts = BackendOpOpts {
            graceful: true,
            wait_ready: Some(BACKEND_READY_TIMEOUT),
            ..BackendOpOpts::new(Origin::Activation)
        };
        match backend_lifecycle::restart_backend(state, sink, id, opts).await {
            Ok(_) => {
                restarted.insert(id.clone());
                restarted_order.push(id.clone());
            }
            Err(e) => {
                let err = first_line(&e.to_string());
                message = format!("Backend {id} restart failed: {err}");
                mark_backend_failure(
                    &mut items,
                    id,
                    &err,
                    &restart_ids,
                    &restarted,
                    plan.gateway_pending,
                );
                status = "failed";
                failure_toast = Some(format!(
                    "Backend {id} could not be restarted to apply the saved changes: {err}"
                ));
                backend_failed = true;
                break;
            }
        }
    }

    if !backend_failed {
        mark_backends_restarted(&mut items, &restarted);
        if plan.gateway_pending {
            let drain = apply_mode == ApplyMode::Drain;
            let line = if drain {
                "Waiting for in-flight gateway requests, then restarting the gateway…".to_string()
            } else {
                "Restarting the gateway…".to_string()
            };
            progress.set(make_progress(
                "gateway",
                Some(lifecycle::GATEWAY),
                line.clone(),
            ));
            lifecycle::emit(
                sink.as_ref(),
                lifecycle::GATEWAY,
                "applying",
                LifecycleState::Restarting,
                Some(Origin::Activation),
                Some(&line),
            );
            let (ok, error) = match crate::op_gateway_restart(
                state,
                sink,
                Some(Origin::Activation.as_str().to_string()),
                Some(drain),
            )
            .await
            {
                Ok(p) => {
                    if p.get("quiesce").is_some() {
                        quiesce = p.get("quiesce").cloned();
                    }
                    if p["ok"] == json!(true) {
                        (true, String::new())
                    } else {
                        manual_command = p["manual_command"].as_str().map(str::to_string);
                        let has_outcome = !p["outcome"].is_null();
                        let msg = p["message"].as_str().unwrap_or("").trim().to_string();
                        let error = if p["busy"] == json!(true) {
                            if msg.is_empty() {
                                "A setup step is already running — wait for it to finish, then apply the plan again.".to_string()
                            } else {
                                msg
                            }
                        } else if msg.is_empty() {
                            "Start-gateway step failed".to_string()
                        } else {
                            format!("Start-gateway step failed — {msg}")
                        };
                        // A step that ran and failed already raised the
                        // "Activation plan failed" toast itself.
                        if !has_outcome {
                            failure_toast = Some(format!(
                                "The gateway could not be restarted to apply the saved changes: {error}"
                            ));
                        }
                        (false, error)
                    }
                }
                Err(e) => {
                    let error = format!("Gateway restart failed: {}", first_line(&e.to_string()));
                    failure_toast = Some(format!(
                        "The gateway could not be restarted to apply the saved changes: {e}"
                    ));
                    (false, error)
                }
            };
            mark_gateway_result(&mut items, ok, &error);
            gateway_restarted = ok;
            status = completed_status(&items);
            message = if ok {
                "Gateway restarted — the saved changes are live.".to_string()
            } else {
                error
            };
        } else {
            message = "Backends restarted — the saved changes are live.".to_string();
        }
    }

    let finished_at = chrono::Utc::now();
    let mut execution = activation_history::ActivationExecution {
        id: String::new(),
        started_at: started_at.to_rfc3339(),
        finished_at: finished_at.to_rfc3339(),
        status: status.to_string(),
        source: source.clone(),
        mode: mode_label.clone(),
        drift: plan.drift,
        gateway_up: plan.gateway_up,
        gateway_pending: plan.gateway_pending,
        items: items.clone(),
    };
    let execution_id = activation_history::record_execution(&state.data_dir, &mut execution);
    let ok = status == "completed";

    // The apply's own toasts: one "applied" when it left nothing
    // pending (the gateway completion may already have fired it), one
    // "failed" for a failure no step reported itself.
    if ok {
        let (unresolved, drift) = crate::activation_unresolved(state).await;
        let already = state
            .notify_watch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .activation_applied_notified;
        if unresolved.is_empty() && !drift && !already {
            state
                .notify_watch
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .activation_applied_notified = true;
            notify::notify(
                sink.as_ref(),
                state,
                notify::Category::Activation,
                "Activation plan applied",
                "All saved changes are now live.",
            )
            .await;
        }
    } else if let Some(body) = failure_toast {
        notify::notify(
            sink.as_ref(),
            state,
            notify::Category::Activation,
            "Activation plan failed",
            &body,
        )
        .await;
    }

    drop(progress);
    let mut result = json!({
        "ok": ok,
        // Older REST clients read `applied`.
        "applied": ok,
        "status": status,
        "message": message,
        "mode": mode_label,
        "source": source,
        "restarted_backends": restarted_order,
        "gateway_restarted": gateway_restarted,
        "execution_id": execution_id,
        "items": items,
    });
    if let Some(cmd) = manual_command {
        result["manual_command"] = json!(cmd);
    }
    if let Some(q) = quiesce {
        result["quiesce"] = q;
    }
    Ok(result)
}

#[cfg(test)]
mod tests;
