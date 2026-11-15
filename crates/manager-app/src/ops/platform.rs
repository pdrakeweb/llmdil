//! MXC policy, Windows ML, and component-update operations.

use crate::*;

/// Current MXC policy: the configured policy file, else the built-in
/// default. Same source rule as `get_mxc_policy`.
pub(crate) fn current_mxc_policy(cfg: &AppConfig) -> Result<Policy, ApiError> {
    match std::fs::read_to_string(&cfg.mxc.path) {
        Ok(text) => serde_json::from_str::<Policy>(&text).map_err(ApiError::from),
        Err(_) => Ok(Policy::default_policy()),
    }
}

/// Sandbox policy JSON: from the configured path, else the built-in default.
pub async fn op_get_mxc_policy(state: &Arc<AppState>, _sink: &Arc<dyn EventSink>) -> ApiResult {
    let cfg = state.config.read().await;
    let (policy, source) = match std::fs::read_to_string(&cfg.mxc.path) {
        Ok(text) => (serde_json::from_str::<Value>(&text)?, "file"),
        Err(_) => (serde_json::to_value(Policy::default_policy())?, "default"),
    };
    Ok(json!({ "policy": policy, "source": source, "mode": cfg.mxc.mode }))
}

/// Validate and store a new sandbox policy. The validator returns a
/// report rather than an error for a bad policy (`valid: false` with
/// the problems listed): the write is refused in that case — an
/// invalid policy file must never reach disk, where the sandbox would
/// read it as authoritative.
pub async fn op_update_mxc_policy(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    policy: Value,
) -> ApiResult {
    let p: Policy = serde_json::from_value(policy)?;
    let report = manager_mxc::validate(&p).map_err(ApiError::from)?;
    if !report.valid {
        return Err(ApiError::Mxc(format!(
            "MXC policy failed validation: {}",
            report.errors.join("; ")
        )));
    }
    let cfg = state.config.read().await;
    if let Some(parent) = cfg.mxc.path.parent() {
        std::fs::create_dir_all(parent).map_err(ApiError::from)?;
    }
    // Atomic replace (the manager_config::save pattern): serialize to
    // a temp file in the same directory, then rename over the target,
    // so a crash mid-write can never leave a truncated policy behind.
    let serialized = serde_json::to_string_pretty(&p)?;
    let mut tmp_path = cfg.mxc.path.clone();
    tmp_path.as_mut_os_string().push(".tmp");
    std::fs::write(&tmp_path, serialized).map_err(ApiError::from)?;
    if let Err(e) = std::fs::rename(&tmp_path, &cfg.mxc.path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(ApiError::from(e));
    }
    Ok(serde_json::to_value(report)?)
}

/// Set the MXC enforcement mode and persist it: the mode is set on a
/// clone of the config, the clone is saved via `manager_config::save`,
/// and only a successful save installs it in memory. `persisted` is
/// true only after that save.
pub async fn op_mxc_set_mode(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    mode: MxcMode,
) -> ApiResult {
    let mut cfg = state.config.write().await;
    let mut new_cfg = cfg.clone();
    new_cfg.mxc.mode = mode;
    manager_config::save(&new_cfg, &state.config_path).map_err(ApiError::from)?;
    *cfg = new_cfg;
    Ok(json!({ "ok": true, "mode": mode, "persisted": true }))
}

/// Run the MXC policy self-test against the current policy. Harmless by
/// construction: probes are pure policy evaluations, nothing is executed
/// and no network traffic is sent.
pub async fn op_mxc_run_self_test(state: &Arc<AppState>, _sink: &Arc<dyn EventSink>) -> ApiResult {
    let cfg = state.config.read().await;
    let policy = current_mxc_policy(&cfg)?;
    let cases = manager_mxc::self_test(&policy);
    let summary = manager_mxc::summarize_self_test(&cases);
    Ok(json!({ "cases": cases, "summary": summary }))
}

/// Learning-mode activity report from `<data_dir>/mxc-activity-report.json`;
/// an empty report when the file is absent. `since_ms` filters to entries
/// at or after that epoch-ms (entries without a parseable timestamp are
/// kept).
pub async fn op_mxc_get_activity_report(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    since_ms: Option<u64>,
) -> ApiResult {
    let path = state.data_dir.join("mxc-activity-report.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({ "entries": [], "source": "none" }));
        }
        Err(e) => return Err(ApiError::from(e)),
    };
    let report = manager_mxc::parse_learning_report(&text).map_err(ApiError::from)?;
    let report = match since_ms {
        Some(since) => manager_mxc::filter_since(&report, since),
        None => report,
    };
    Ok(json!({ "entries": report.entries, "source": path }))
}

/// Propose policy tightenings from a learning report (JSON) against the
/// current policy, via `manager_mxc::suggest_rules`.
pub async fn op_mxc_propose_tightening(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    report: Value,
) -> ApiResult {
    let cfg = state.config.read().await;
    let policy = current_mxc_policy(&cfg)?;
    let text = serde_json::to_string(&report)?;
    let parsed = manager_mxc::parse_learning_report(&text).map_err(ApiError::from)?;
    let suggestions = manager_mxc::suggest_rules(&parsed, &policy);
    Ok(json!({ "suggestions": suggestions }))
}

/// Probe WinMLServer availability; limits recorded verbatim. This is
/// the WinML card's "Re-check capabilities": with a registration
/// active it re-probes the registered port through
/// `WinMlRegistry::recheck_capabilities`, refreshing the
/// registration's limits; otherwise it probes the configured port.
pub async fn op_winml_probe(state: &Arc<AppState>, _sink: &Arc<dyn EventSink>) -> ApiResult {
    // "Re-check capabilities" (tech spec §6.5 / P7): with a model
    // registered, the probe goes through the registry so the active
    // registration's banner limits (and a missing adapter hint) are
    // refreshed from what the server reports now. Same report shape
    // either way; the refresh only replaces probe-derived data.
    if let Ok(registry) = state.winml().await {
        let mut guard = registry.lock().await;
        if guard.status().is_some() {
            let report = guard.recheck_capabilities().await.map_err(ApiError::from)?;
            return Ok(serde_json::to_value(report)?);
        }
    }
    let cfg = state.config.read().await;
    let port = if cfg.winml.port == 0 {
        manager_winml::DEFAULT_PORT
    } else {
        cfg.winml.port
    };
    let transport = manager_winml::ReqwestProbeTransport::new(Duration::from_secs(10))
        .map_err(|e| ApiError::WinMl(format!("winml transport setup failed: {e:?}")))?;
    let report = manager_winml::probe(&transport, &manager_winml::base_url(port))
        .await
        .map_err(ApiError::from)?;
    Ok(serde_json::to_value(report)?)
}

/// Register a model with the Windows ML secondary backend.
pub async fn op_winml_register(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    model_id: String,
) -> ApiResult {
    let cfg = state.config.read().await.clone();
    let size_mib = manager_models::curated_models()
        .iter()
        .find(|m| m.id == model_id)
        .map(|m| m.size_bytes / manager_models::MIB);
    let opts = manager_winml::RegisterOptions {
        model_size_mib: size_mib,
        gpu_selection: manager_winml::GpuSelection::default(),
    };
    let registry = state.winml().await?;
    let mut guard = registry.lock().await;
    let reg = guard
        .register(&cfg.winml, &model_id, &opts)
        .await
        .map_err(ApiError::from)?;
    let value = serde_json::to_value(reg)?;
    drop(guard);
    Ok(value)
}

/// Unregister the Windows ML backend.
pub async fn op_winml_unregister(state: &Arc<AppState>, _sink: &Arc<dyn EventSink>) -> ApiResult {
    let registry = state.winml().await?;
    let removed = registry.lock().await.unregister();
    Ok(json!({ "ok": true, "removed": removed.is_some() }))
}

/// Registration status plus the researched limits (implemented, no stub).
pub async fn op_winml_status(state: &Arc<AppState>, _sink: &Arc<dyn EventSink>) -> ApiResult {
    let cfg = state.config.read().await;
    let registered = match state.winml().await {
        Ok(registry) => registry.lock().await.status().cloned(),
        Err(_) => None,
    };
    Ok(json!({
        "enabled": cfg.winml.enabled,
        "port": cfg.winml.port,
        "model_id": cfg.winml.model_id,
        "registered": registered,
        "limits": manager_winml::WinMlLimits::known_limits(),
    }))
}

/// Check every managed component (installed / latest / pinned /
/// update available); per-component feed failures are reported, not
/// fatal. See the `updates` module.
pub async fn op_updates_check_all(state: &Arc<AppState>, sink: &Arc<dyn EventSink>) -> ApiResult {
    updates::op_check_all(state, sink).await
}

/// Apply an update to one component, or to every applicable unpinned
/// component (`component_id = "all"`), as a cancellable task. LiteLLM
/// requires an explicit `version` (it moves only by a newer pin). See
/// the `updates` module.
pub async fn op_updates_apply(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    component_id: String,
    version: Option<String>,
) -> ApiResult {
    updates::op_apply(state, sink, component_id, version).await
}

/// Pin a component to a version and persist it: the pin is set on a
/// clone of the config, the clone is saved via `manager_config::save`,
/// and only a successful save installs it in memory. `persisted` is
/// true only after that save; on failure the in-memory config is
/// unchanged and the error surfaces.
pub async fn op_updates_pin(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    component_id: String,
    version: Option<String>,
) -> ApiResult {
    let mut cfg = state.config.write().await;
    let mut new_cfg = cfg.clone();
    new_cfg
        .updates
        .components
        .entry(component_id.clone())
        .and_modify(|c| c.pinned_version = version.clone())
        .or_insert(ComponentUpdate {
            pinned_version: version.clone(),
            auto_update: true,
            last_checked: None,
        });
    manager_config::save(&new_cfg, &state.config_path).map_err(ApiError::from)?;
    *cfg = new_cfg;
    Ok(
        json!({ "ok": true, "persisted": true, "component_id": component_id, "pinned_version": version }),
    )
}

/// Toggle auto-update for a component and persist it: same
/// clone → save → install pattern as [`updates_pin`].
pub async fn op_updates_set_auto(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    component_id: String,
    enabled: bool,
) -> ApiResult {
    let mut cfg = state.config.write().await;
    let mut new_cfg = cfg.clone();
    new_cfg
        .updates
        .components
        .entry(component_id.clone())
        .and_modify(|c| c.auto_update = enabled)
        .or_insert(ComponentUpdate {
            pinned_version: None,
            auto_update: enabled,
            last_checked: None,
        });
    manager_config::save(&new_cfg, &state.config_path).map_err(ApiError::from)?;
    *cfg = new_cfg;
    Ok(
        json!({ "ok": true, "persisted": true, "component_id": component_id, "auto_update": enabled }),
    )
}

#[cfg(test)]
mod tests;
