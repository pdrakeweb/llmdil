//! Status, config, and notification-settings operations.

use crate::*;

/// Overall service status snapshot.
pub async fn op_get_status(state: &Arc<AppState>, _sink: &Arc<dyn EventSink>) -> ApiResult {
    let cfg = state.config.read().await;
    let mut v = json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "backends_configured": cfg.backends.len(),
        "gateway_port": cfg.gateway.port,
        "winml_enabled": cfg.winml.enabled,
        "mxc_mode": cfg.mxc.mode,
        "mxc_runtime_installed": manager_wizard::steps::installed_mxc_runtime(&state.data_dir)
            .is_some(),
    });
    // Additive field (finding F2): present only when the shell recorded
    // the startup load outcome.
    if let Some(load) = state
        .config_load
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
    {
        v["config_load"] = load;
    }
    Ok(v)
}

/// Full application configuration, plus `config_path`: the file the
/// config was loaded from (and saves go to), for the Settings view.
pub async fn op_get_config(state: &Arc<AppState>, _sink: &Arc<dyn EventSink>) -> ApiResult {
    let cfg = state.config.read().await;
    let mut v = serde_json::to_value(&*cfg)?;
    if let Value::Object(map) = &mut v {
        map.insert(
            "config_path".to_string(),
            json!(state.config_path.to_string_lossy()),
        );
    }
    Ok(v)
}

/// Merge a JSON patch into the config, validate, and persist to disk via
/// `manager_config::save` (atomic write with `.bak` rotation). The merged
/// value is installed in memory ONLY when the save succeeds: on failure
/// the in-memory config is unchanged and the error surfaces, so memory
/// and disk never diverge.
pub async fn op_update_config(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    patch: Value,
) -> ApiResult {
    // Seeding/repair first: this command validates the patched config,
    // and a fresh install's pre-seed config (default backends not yet
    // aligned to registered models) must not fail that validation.
    ensure_catalog_models(state).await;
    let mut cfg = state.config.write().await;
    let mut v = serde_json::to_value(&*cfg)?;
    merge_json(&mut v, patch);
    let mut new_cfg: AppConfig = serde_json::from_value(v)?;
    // The mode rule of mxc_set_mode holds here too; a mode already in
    // the file is not a change and does not block other edits.
    if new_cfg.mxc.mode != cfg.mxc.mode {
        check_mxc_mode(&state.data_dir, new_cfg.mxc.mode)?;
    }
    // Reserved temp-backend namespace (code review B-3): a patch
    // may not introduce a NEW backend whose id falls in the
    // `opt-exp-*` experiment namespace — the same reservation
    // backend_add enforces at creation. An id already present in
    // the current config is not a creation and is left alone;
    // the startup sweep owns such leftovers.
    for nb in &new_cfg.backends {
        if manager_optimize::is_temp_backend_id(&nb.id)
            && !cfg.backends.iter().any(|b| b.id == nb.id)
        {
            return Err(reserved_temp_backend_id_error(&nb.id));
        }
    }
    // Advanced flags mode (tracker #60): this whole-config merge
    // is the one remaining path that could rewrite a backend's
    // flags wholesale, so it is held to the same one-writer rule —
    // a patch may not change the flags (structured or raw), the
    // mode bit itself, or remove a backend that is in advanced
    // mode. Mode flips go through backend_set_flags_mode so they
    // are never smuggled inside an unrelated patch.
    for pre_b in cfg.backends.iter().filter(|b| b.flags_advanced) {
        let Some(nb) = new_cfg.backends.iter().find(|b| b.id == pre_b.id) else {
            return Err(ApiError::InvalidInput(format!(
                "the patch removes backend {}, which is in advanced flags \
                 mode; remove it with backend_delete instead",
                pre_b.id
            )));
        };
        if !nb.flags_advanced
            || nb.raw_flags != pre_b.raw_flags
            || nb.flags.n_ctx != pre_b.flags.n_ctx
            || nb.flags.n_batch != pre_b.flags.n_batch
            || nb.flags.tensor_split != pre_b.flags.tensor_split
            || nb.flags.split_mode != pre_b.flags.split_mode
        {
            return Err(advanced_flags_refusal(&pre_b.id));
        }
    }
    // The tool-call probe is runtime state, never operator-set:
    // each backend keeps the probe the app itself recorded, and a
    // patch that changes its model file or raw flags invalidates
    // that probe (MCP design §7.2).
    for nb in new_cfg.backends.iter_mut() {
        nb.tool_call_probe = cfg
            .backends
            .iter()
            .find(|b| b.id == nb.id)
            .and_then(|b| b.tool_call_probe);
    }
    manager_config::reset_stale_tool_call_probes(&cfg, &mut new_cfg);
    // `save` validates before writing, so a failed save never clobbers
    // the last-known-good file.
    manager_config::save(&new_cfg, &state.config_path).map_err(ApiError::from)?;
    *cfg = new_cfg;
    drop(cfg);
    // Saved log-streaming settings apply now, not at the next start
    // (a no-op unless they changed).
    logship::apply_log_streaming(state).await;
    Ok(json!({ "ok": true, "persisted": true }))
}

/// Set the supervision mode (Settings → Supervision): which of the two
/// the next lifecycle operation uses. Nothing is started, stopped or
/// registered here.
pub async fn op_settings_set_supervision(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    mode: manager_config::SupervisionMode,
) -> ApiResult {
    let mut cfg = state.config.write().await;
    let mut new_cfg = cfg.clone();
    new_cfg.supervision.mode = mode;
    manager_config::save(&new_cfg, &state.config_path).map_err(ApiError::from)?;
    *cfg = new_cfg;
    Ok(json!({ "ok": true, "mode": mode, "persisted": true }))
}

/// Replace the notification preferences (Settings → Notifications).
/// Save-then-install, exactly like `update_config`: the file is
/// validated and written first; the in-memory config changes only
/// when the save succeeds.
pub async fn op_notifications_set_config(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    config: manager_config::NotificationConfig,
) -> ApiResult {
    let mut cfg = state.config.write().await;
    let mut new_cfg = cfg.clone();
    new_cfg.notifications = config;
    manager_config::save(&new_cfg, &state.config_path).map_err(ApiError::from)?;
    *cfg = new_cfg;
    Ok(json!({ "ok": true, "persisted": true }))
}

/// Fire one toast immediately (Settings → Notifications, "Send test
/// notification"). Deliberately bypasses the category settings via
/// `notify::send_toast`: the control exists to prove the OS delivery
/// path, and a master switch set to off must not make it look broken
/// — the card reports the switch state separately. A send error is
/// returned so the card can show why delivery failed.
///
/// `ok` means Windows accepted the toast. Whether it appeared is not
/// knowable from here, so `delivery_confirmed` is always false;
/// `windows_setting` is `off` when Windows Settings switches
/// notifications off (for all apps or for this one), else `on`, and
/// null where it cannot be read.
///
/// An Admin API caller's sink has no OS toast, so its test goes to the
/// desktop shell's; without a shell there is nothing to test.
pub async fn op_notifications_send_test(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let sink = match event_bus::current_origin() {
        event_bus::EventOrigin::Api => state.desktop_sink().ok_or_else(|| {
            ApiError::Unsupported("no desktop shell is attached to show a notification".to_string())
        })?,
        _ => Arc::clone(sink),
    };
    notify::send_toast(
        sink.as_ref(),
        "LLMdil test notification",
        "Toast delivery is working. Event notifications follow the switches on this card.",
    )
    .map_err(ApiError::Io)?;
    #[cfg(windows)]
    let off = Some(manager_wizard::ops::toasts_switched_off(TOAST_APP_ID));
    #[cfg(not(windows))]
    let off: Option<bool> = None;
    Ok(json!({
        "ok": true,
        "delivery_confirmed": false,
        "app_user_model_id": TOAST_APP_ID,
        "windows_setting": off.map(|off| if off { "off" } else { "on" }),
        "detail": match off {
            Some(true) => "Sent, but notifications for LLMdil are switched off in Windows Settings, so Windows will not show it.",
            _ => "Sent to Windows. If nothing appeared, look in the notification center and check Do Not Disturb.",
        },
    }))
}

/// The AppUserModelID the shell's toasts are sent under (the
/// `identifier` in src-tauri/tauri.conf.json).
const TOAST_APP_ID: &str = "com.pdrakeweb.llm-manager";

#[cfg(test)]
mod tests;
