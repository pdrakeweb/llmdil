//! What the Admin API decides about a long-running operation before it
//! answers `202`: unknown ids and "somebody else is at it" are the
//! caller's answer, not a failed operation to poll for. Only checks that
//! wait for nothing are made here; the operation repeats every one of
//! them, so a state that changes in between still ends as a failed
//! operation with the same error kind.

use serde_json::{json, Value};

use crate::registry::Op;
use crate::*;

/// A refusal, with the machine-readable context of a busy one.
#[derive(Debug)]
pub struct Refusal {
    pub error: ApiError,
    pub details: Option<Value>,
}

impl From<ApiError> for Refusal {
    fn from(error: ApiError) -> Self {
        Self {
            error,
            details: None,
        }
    }
}

/// Refuse `op` with `args` when it can already be told that it will be.
pub async fn precheck_operation(
    state: &Arc<AppState>,
    op: Op,
    args: &Value,
) -> Result<(), Refusal> {
    let text = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default();
    match op {
        Op::StartBackend | Op::RestartBackend | Op::StopBackend => {
            let id = text("id");
            let configured = state
                .config
                .read()
                .await
                .backends
                .iter()
                .any(|b| b.id == id);
            // A process without a configuration can still be stopped.
            if !configured
                && (op != Op::StopBackend
                    || !lifecycle::backend_liveness(state).await.contains_key(id))
            {
                return Err(ApiError::NotFound(format!("backend {id}")).into());
            }
            service_free(state, &lifecycle::backend_key(id))
        }
        Op::GatewayRestart | Op::GatewayStop => service_free(state, lifecycle::GATEWAY),
        Op::PostgresStop => service_free(state, lifecycle::POSTGRES),
        Op::LogReceiverStart | Op::LogReceiverStop | Op::LogReceiverRestart => {
            service_free(state, lifecycle::LOG_RECEIVER)
        }
        Op::RunWizardStep | Op::SetupRetryStep => {
            let step = text("step_id");
            let steps = manager_wizard::steps::default_steps();
            if !steps.iter().any(|s| s.id() == step) {
                return Err(ApiError::NotFound(format!("setup step {step}")).into());
            }
            if op == Op::RunWizardStep && step == "start-gateway" {
                service_free(state, lifecycle::GATEWAY)?;
            }
            install_wizard_free(state)
        }
        Op::RunAudit => match state.wizard(RunMode::Audit).await.try_lock() {
            Ok(_) => Ok(()),
            Err(_) => Err(Refusal {
                error: ApiError::Busy("an audit is already in progress".to_string()),
                details: Some(json!({ "busy": true, "already_running": true })),
            }),
        },
        Op::ActivationApply => match state.activation.apply_lock.try_lock() {
            Ok(_) => Ok(()),
            Err(_) => {
                let progress = activation_apply::current_progress(state);
                Err(Refusal {
                    error: ApiError::Busy(activation_apply::busy_message(progress.as_ref())),
                    details: progress.map(|p| json!({ "activation_applying": p })),
                })
            }
        },
        Op::DownloadModel => {
            let id = text("id");
            ensure_catalog_models(state).await;
            download_spec_for(&*state.config.read().await, id)?;
            let handles = state.downloads.handles.lock().await;
            if handles.get(id).is_some_and(|h| !h.is_terminal()) {
                let e = format!("download already in flight for model {id}");
                return Err(ApiError::Busy(e).into());
            }
            Ok(())
        }
        Op::ModelResumeDownload => {
            let id = text("id");
            if !state.downloads.handles.lock().await.contains_key(id) {
                let e = format!("no download in flight for {id}");
                return Err(ApiError::NotFound(e).into());
            }
            Ok(())
        }
        Op::ModelVerifyChecksum => {
            let id = text("id");
            let pinned = state
                .config
                .read()
                .await
                .models
                .iter()
                .find(|m| m.id == id)
                .map(|m| is_real_pin(&m.sha256))
                .ok_or_else(|| ApiError::NotFound(format!("model {id}")))?;
            if !pinned {
                return Err(no_pin_error(id).into());
            }
            let verifying = state
                .downloads
                .verifying
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if verifying.contains(id) {
                let e = format!("verification already in progress for model {id}");
                return Err(ApiError::Busy(e).into());
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Busy while a lifecycle operation holds the service.
fn service_free(state: &AppState, key: &str) -> Result<(), Refusal> {
    let Some(t) = state.lifecycle.transition(key) else {
        return Ok(());
    };
    let busy = lifecycle::BusyInfo {
        key: key.to_string(),
        kind: t.kind,
        origin: t.origin,
        since: t.since,
    };
    Err(Refusal {
        error: ApiError::Busy(busy.to_string()),
        details: Some(json!({ "busy": true, "transition": t })),
    })
}

/// Busy while a setup step runs, naming it. (Not the wizard lock: a
/// checklist read holds it for a moment.)
fn install_wizard_free(state: &AppState) -> Result<(), Refusal> {
    let running = state
        .wizard
        .step_running
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some((id, since)) = running.as_ref() else {
        return Ok(());
    };
    let secs = since.elapsed().as_secs();
    Err(Refusal {
        error: ApiError::Busy(format!(
            "setup step {id} is already running (started {secs} s ago)"
        )),
        details: Some(json!({ "busy": true, "running_step": id, "elapsed_secs": secs })),
    })
}

#[cfg(test)]
mod tests;
