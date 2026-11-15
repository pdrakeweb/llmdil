//! Windows service registration operations (gateway, backends, log receiver).

use crate::*;

/// Supervision state for Settings: the mode, the resolved service
/// account, shim presence, and every managed service's registration
/// + SCM state.
pub async fn op_services_list(state: &Arc<AppState>, _sink: &Arc<dyn EventSink>) -> ApiResult {
    let cfg = state.config.read().await.clone();
    let host = service_host_path();
    // (target, kind, service name, lifecycle key)
    let mut targets: Vec<(String, &'static str, String, String)> = cfg
        .backends
        .iter()
        .map(|b| {
            (
                b.id.clone(),
                "backend",
                manager_services::backend_service_name(&b.id),
                lifecycle::backend_key(&b.id),
            )
        })
        .collect();
    targets.push((
        "gateway".to_string(),
        "gateway",
        manager_services::GATEWAY_SERVICE_NAME.to_string(),
        lifecycle::GATEWAY.to_string(),
    ));
    targets.push((
        lifecycle::LOG_RECEIVER.to_string(),
        "log-receiver",
        manager_services::LOG_RECEIVER_SERVICE_NAME.to_string(),
        lifecycle::LOG_RECEIVER.to_string(),
    ));
    // One batched SCM query (the shared snapshot, at most 1 s old).
    let ids: Vec<String> = cfg.backends.iter().map(|b| b.id.clone()).collect();
    let snap = lifecycle::scm_snapshot_ids(state, Duration::from_secs(1), ids).await;
    let services: Vec<Value> = targets
        .into_iter()
        .map(|(target, kind, name, key)| {
            let live = snap.get(&name).filter(|s| lifecycle::registered(s));
            // An operation in flight on the service wins (the same
            // precedence every lifecycle view applies).
            let transition = state.lifecycle.transition(&key);
            let lifecycle_state = match (&transition, live) {
                (Some(t), _) => t.kind.into(),
                (None, Some(s)) => s.lifecycle_state(),
                (None, None) => lifecycle::LifecycleState::NotInstalled,
            };
            json!({
                "target": target,
                "kind": kind,
                "service_name": name,
                "registered": live.is_some(),
                "state": live.map(|s| serde_json::to_value(s.state).unwrap_or(Value::Null)),
                "lifecycle": lifecycle_state.as_str(),
                "transition": transition,
                "pid": live.and_then(|s| s.pid),
                "child_pid": live.and_then(|s| s.child_pid),
                "exit_code": live.and_then(lifecycle::scm_exit_code),
                "failure_reason": live.and_then(|s| s.failure_reason()),
            })
        })
        .collect();
    Ok(json!({
        "mode": serde_json::to_value(cfg.supervision.mode).unwrap_or(Value::Null),
        "account": manager_services::scm::current_user_account(),
        "host_path": host.display().to_string(),
        "host_present": host.exists(),
        "services": services,
    }))
}

/// Register one service (`target`: a backend id, or "gateway").
/// Builds the current definition, then runs the elevated helper
/// (`llm-service-host register`) through the app's existing runas
/// pattern. The helper collects the account password itself with
/// the Windows system credential dialog (CredUI) and passes it
/// straight to SCM — the password never crosses this command's
/// boundary, is never typed into an LLMdil field, and is never
/// written to config or logs.
pub async fn op_services_register(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    target: String,
) -> ApiResult {
    refuse_log_receiver_target(&target)?;
    // A live process-route instance would keep holding the port while
    // the next start routes to the SCM: refuse until it is stopped
    // (SCM-18). An operation in flight on it is refused the same way.
    if target == "gateway" {
        let view = lifecycle::gateway_view(state).await;
        if view.transition.is_some()
            || (!view.service_managed && (view.pid.is_some() || view.state.is_live()))
        {
            return Err(ApiError::InvalidInput(
                "stop the gateway first (it is running under process supervision)".to_string(),
            ));
        }
    } else if let Some(b) = lifecycle::backend_liveness(state).await.get(&target) {
        if b.transition.is_some() || (b.state.is_live() && !b.service_managed) {
            return Err(ApiError::InvalidInput(format!(
                "stop backend {target} first (it is running under process supervision)"
            )));
        }
    }
    let host = require_service_host()?;
    let cfg = state.config.read().await.clone();
    let mut def = if target == "gateway" {
        let launch = manager_wizard::steps::gateway_service_launch(&cfg, &state.data_dir);
        manager_services::gateway_definition(
            launch,
            state.data_dir.join("logs").join("gateway.stdout.log"),
            state.data_dir.join("logs").join("gateway.stderr.log"),
            manager_wizard::gateway_pid_path(&state.data_dir),
        )
    } else {
        if !cfg.backends.iter().any(|b| b.id == target) {
            return Err(ApiError::NotFound(format!("backend {target}")));
        }
        let path = state
            .write_backend_service_definition(&target)
            .await
            .map_err(ApiError::Services)?;
        manager_services::read_definition(&path).map_err(|e| ApiError::Services(e.to_string()))?
    };
    let account = manager_services::scm::current_user_account().ok_or_else(|| {
        ApiError::Services("could not resolve the current user account for the service".to_string())
    })?;
    def.account = Some(account.clone());
    let service_name = def.name.clone();
    let def_path = manager_services::write_definition(&state.data_dir, &def)
        .map_err(|e| ApiError::Services(e.to_string()))?;
    let cmd = manager_wizard::PlannedCommand::new(vec![
        host.display().to_string(),
        "register".to_string(),
        "--definition".to_string(),
        def_path.display().to_string(),
    ]);
    let outcome =
        <RealSystemOps as manager_wizard::SystemOps>::run_elevated(&RealSystemOps::default(), &cmd)
            .await;
    let out = outcome.map_err(|e| {
        ApiError::Services(format!(
            "elevated registration could not run (was a prompt declined?): {e}"
        ))
    })?;
    if out.exit_code != 0 {
        return Err(ApiError::Services(format!(
            "registration of {service_name} failed (helper exit {}); see logs/service-manage.log in the data directory",
            out.exit_code
        )));
    }
    service_registration_changed(
        state,
        sink,
        &target,
        "registered as a Windows service",
        &service_name,
    );
    Ok(json!({ "ok": true, "service_name": service_name, "account": account }))
}

/// The log receiver is deployed and removed from its own card
/// (`log_receiver_deploy` / `log_receiver_remove`), never through the
/// generic services register/unregister.
pub(crate) fn refuse_log_receiver_target(target: &str) -> Result<(), ApiError> {
    if target == lifecycle::LOG_RECEIVER {
        return Err(ApiError::InvalidInput(
            "the log receiver is deployed and removed from Settings > Log receiver".to_string(),
        ));
    }
    Ok(())
}

/// The lifecycle key of a services register/unregister `target`.
pub(crate) fn service_target_key(target: &str) -> String {
    if target == "gateway" {
        lifecycle::GATEWAY.to_string()
    } else {
        lifecycle::backend_key(target)
    }
}

/// After a (un)registration: a marker in the target's own lifecycle
/// log, a SERVICE_LIFECYCLE event (so every surface repaints its
/// route and state) and a fresh SCM snapshot on the next read.
pub(crate) fn service_registration_changed(
    state: &AppState,
    sink: &Arc<dyn EventSink>,
    target: &str,
    event: &str,
    service_name: &str,
) {
    let key = service_target_key(target);
    lifecycle::mark(
        &state.data_dir,
        &key,
        event,
        Some(service_name),
        Some(lifecycle::Origin::Ui),
    );
    lifecycle::invalidate_scm(state);
    lifecycle::emit(
        sink.as_ref(),
        &key,
        if event.starts_with("registered") {
            "registered"
        } else {
            "unregistered"
        },
        lifecycle::LifecycleState::Stopped,
        Some(lifecycle::Origin::Ui),
        Some(service_name),
    );
}

/// Unregister one service (`target`: a backend id, or "gateway")
/// through the elevated helper. The service returns to process
/// supervision; its definition file is left in place. Refused while
/// the service is running or pending (unregistering would stop it
/// silently inside the SCM delete): stop it first (SCM-18).
pub async fn op_services_unregister(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    target: String,
) -> ApiResult {
    refuse_log_receiver_target(&target)?;
    let service_name = if target == "gateway" {
        manager_services::GATEWAY_SERVICE_NAME.to_string()
    } else {
        manager_services::backend_service_name(&target)
    };
    if let Some(status) = lifecycle::scm_status(state, &service_name, Duration::from_secs(1)).await
    {
        if lifecycle::scm_live(&status) {
            return Err(ApiError::InvalidInput(format!(
                "stop {} first (its service is {})",
                if target == "gateway" {
                    "the gateway".to_string()
                } else {
                    format!("backend {target}")
                },
                status.state.as_str()
            )));
        }
    }
    let host = require_service_host()?;
    let cmd = manager_wizard::PlannedCommand::new(vec![
        host.display().to_string(),
        "unregister".to_string(),
        "--name".to_string(),
        service_name.clone(),
    ]);
    let out =
        <RealSystemOps as manager_wizard::SystemOps>::run_elevated(&RealSystemOps::default(), &cmd)
            .await
            .map_err(|e| {
                ApiError::Services(format!(
                    "elevated unregister could not run (was the prompt declined?): {e}"
                ))
            })?;
    if out.exit_code != 0 {
        return Err(ApiError::Services(format!(
            "unregister of {service_name} failed (helper exit {})",
            out.exit_code
        )));
    }
    service_registration_changed(
        state,
        sink,
        &target,
        "unregistered (back to process supervision)",
        &service_name,
    );
    Ok(json!({ "ok": true, "service_name": service_name }))
}

pub(crate) fn require_service_host() -> Result<std::path::PathBuf, ApiError> {
    let host = service_host_path();
    if host.exists() {
        Ok(host)
    } else {
        Err(ApiError::Services(format!(
            "llm-service-host.exe not found beside the app executable (expected {}); \
             build it with `cargo build -p manager-services --bin llm-service-host` and place it there",
            host.display()
        )))
    }
}

/// The service host shim's expected location: beside the app
/// executable. Installed builds ship it there; a branch test build
/// copies the cargo-built binary next to the app (see
/// docs/windows-services-design.md).
pub(crate) fn service_host_path() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("llm-service-host.exe")))
        .unwrap_or_else(|| std::path::PathBuf::from("llm-service-host.exe"))
}
