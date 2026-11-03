//! Log receiver deployment (installer integration, Oct 2026): the
//! Prometheus-local receiver (crate `manager-logreceiver`) ships in
//! the LLMdil installer beside the app, and deployment is driven
//! from here — Settings → Supervision → Log receiver — instead of
//! the retired hand-run Deploy-LogReceiver.ps1 script.
//!
//! The elevated work (ACLs, token seeding, service registration,
//! the read-listener firewall rule) lives in the receiver binary's
//! own `deploy` / `remove` subcommands, which reuse the helper logic
//! the script drove; these ops only locate the bundled binary, run
//! it through the app's existing runas pattern
//! ([`manager_wizard::SystemOps::run_elevated`], the same path
//! service registration uses), and read the outcome back from
//! `deploy-result.json` (an elevated child's stdout is not visible
//! to the app). The endpoint flip is a plain config save — no
//! elevation — and stays a separate, deliberate op: deploying never
//! flips, and flipping never deploys.

use std::sync::Arc;

use manager_wizard::SystemOps as _;
use serde_json::{json, Value};

use crate::{ApiError, ApiResult, AppState, EventSink};

/// The local receiver's ingest endpoint — the flip's target value
/// (design §3.6: the only config change the relocation makes).
pub const LOCAL_RECEIVER_ENDPOINT: &str = "http://127.0.0.1:8473";

/// The bundled receiver binary's path: beside the app executable,
/// where the installer's externalBin staging places it.
pub fn receiver_exe_path() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("llmdil-log-receiver.exe")))
        .unwrap_or_else(|| std::path::PathBuf::from("llmdil-log-receiver.exe"))
}

fn require_receiver_exe() -> Result<std::path::PathBuf, ApiError> {
    let exe = receiver_exe_path();
    if exe.exists() {
        Ok(exe)
    } else {
        Err(ApiError::Services(format!(
            "llmdil-log-receiver.exe not found beside the app executable (expected {}); \
             it ships in the LLMdil installer — reinstall or update LLMdil",
            exe.display()
        )))
    }
}

/// The receiver's directory under the data dir (store, token files,
/// deploy result/log), via manager-services' single spelling.
fn lr_dir(state: &AppState) -> std::path::PathBuf {
    manager_services::log_receiver_dir(&state.data_dir)
}

/// Read and delete `deploy-result.json` (it can carry a one-time
/// read token; it never lingers). A missing or unreadable file is
/// `None` — the caller falls back to the exit-code verdict.
fn take_deploy_result(state: &AppState) -> Option<Value> {
    let path = lr_dir(state).join("deploy-result.json");
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    serde_json::from_str(&text).ok()
}

/// Deployment state for the Settings card: bundled binaries, the
/// service's registration + SCM state, the token files' presence,
/// and the shipper's current endpoint. Reads only — the SCM query
/// runs unelevated (the registration's DACL grants the operator
/// query), so opening Settings never prompts.
pub async fn op_log_receiver_status(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let cfg = state.config.read().await.clone();
    let dir = lr_dir(state);
    let queried = tokio::task::spawn_blocking(move || {
        use manager_services::ServiceControl;
        let control = manager_services::scm::platform_control();
        control
            .query(manager_services::LOG_RECEIVER_SERVICE_NAME)
            .ok()
    })
    .await
    .unwrap_or(None);
    let live = queried
        .as_ref()
        .filter(|s| !matches!(s.state, manager_services::ServiceState::NotInstalled));
    let receiver_exe = receiver_exe_path();
    Ok(json!({
        "service_name": manager_services::LOG_RECEIVER_SERVICE_NAME,
        "registered": live.is_some(),
        "state": live.map(|s| &s.state),
        "pid": live.and_then(|s| s.pid),
        "receiver_exe": receiver_exe.display().to_string(),
        "receiver_present": receiver_exe.exists(),
        "data_dir": dir.display().to_string(),
        "ingest_token_seeded": dir.join("ingest-tokens.txt").exists(),
        "read_token_seeded": dir.join("read-token.txt").exists(),
        "endpoint": cfg.log_streaming.endpoint,
        "endpoint_is_local": cfg.log_streaming.endpoint.trim() == LOCAL_RECEIVER_ENDPOINT,
        "streaming_enabled": cfg.log_streaming.enabled,
    }))
}

/// Deploy (or repair) the receiver: run the bundled binary's
/// `deploy` subcommand elevated. On success the result carries
/// `read_token_new` when this deploy generated the read token —
/// the one moment the value is visible, for Secure Vault filing;
/// a repair that kept the existing token reports it as null.
pub async fn op_log_receiver_deploy(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let exe = require_receiver_exe()?;
    // Clear any stale result so the read-back below can only be
    // this run's outcome.
    let _ = std::fs::remove_file(lr_dir(state).join("deploy-result.json"));
    let install_dir = exe
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let cmd = manager_wizard::PlannedCommand::new(vec![
        exe.display().to_string(),
        "deploy".to_string(),
        "--data-dir".to_string(),
        state.data_dir.display().to_string(),
        "--install-dir".to_string(),
        install_dir,
    ]);
    let out = manager_wizard::ops::RealSystemOps::default()
        .run_elevated(&cmd)
        .await
        .map_err(|e| {
            ApiError::Services(format!(
                "elevated deploy could not run (was the prompt declined?): {e}"
            ))
        })?;
    let result = take_deploy_result(state);
    if out.exit_code != 0 {
        let detail = result
            .as_ref()
            .and_then(|r| r.get("error"))
            .and_then(|e| e.as_str())
            .map(|e| format!(": {e}"))
            .unwrap_or_default();
        return Err(ApiError::Services(format!(
            "log receiver deploy failed (helper exit {}){detail}; see log-receiver/deploy.log in the data directory",
            out.exit_code
        )));
    }
    let read_token_new = result
        .as_ref()
        .and_then(|r| r.get("read_token_new"))
        .and_then(|t| t.as_str())
        .map(|t| t.to_string());
    Ok(json!({
        "ok": true,
        "service_name": manager_services::LOG_RECEIVER_SERVICE_NAME,
        "read_token_new": read_token_new,
    }))
}

/// Remove the receiver: run the bundled binary's `remove`
/// subcommand elevated (stop + unregister + firewall-rule delete).
/// The store and token files stay in the data dir.
pub async fn op_log_receiver_remove(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let exe = require_receiver_exe()?;
    let _ = state;
    let cmd =
        manager_wizard::PlannedCommand::new(vec![exe.display().to_string(), "remove".to_string()]);
    let out = manager_wizard::ops::RealSystemOps::default()
        .run_elevated(&cmd)
        .await
        .map_err(|e| {
            ApiError::Services(format!(
                "elevated remove could not run (was the prompt declined?): {e}"
            ))
        })?;
    if out.exit_code != 0 {
        return Err(ApiError::Services(format!(
            "log receiver remove failed (helper exit {})",
            out.exit_code
        )));
    }
    Ok(json!({
        "ok": true,
        "service_name": manager_services::LOG_RECEIVER_SERVICE_NAME,
    }))
}

/// Flip the shipper endpoint to the local receiver (the retired
/// script's -FlipEndpoint): back up config.json beside itself with
/// a timestamped name, set `log_streaming.endpoint` through the
/// validated save path, and report the backup. Nothing else in the
/// config changes; the app must restart for the shipper to pick
/// the new endpoint up. Deliberately separate from deploy.
pub async fn op_log_receiver_flip_endpoint(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let mut cfg = state.config.write().await;
    if cfg.log_streaming.endpoint.trim() == LOCAL_RECEIVER_ENDPOINT {
        return Ok(json!({
            "ok": true,
            "endpoint": LOCAL_RECEIVER_ENDPOINT,
            "already": true,
            "backup": Value::Null,
        }));
    }
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let backup = state
        .config_path
        .with_extension(format!("json.bak-{stamp}"));
    std::fs::copy(&state.config_path, &backup).map_err(|e| {
        ApiError::Io(format!(
            "could not back up {} before the flip: {e}",
            state.config_path.display()
        ))
    })?;
    let mut new_cfg = cfg.clone();
    new_cfg.log_streaming.endpoint = LOCAL_RECEIVER_ENDPOINT.to_string();
    manager_config::save(&new_cfg, &state.config_path).map_err(ApiError::from)?;
    *cfg = new_cfg;
    Ok(json!({
        "ok": true,
        "endpoint": LOCAL_RECEIVER_ENDPOINT,
        "already": false,
        "backup": backup.display().to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_endpoint_is_the_design_value() {
        assert_eq!(LOCAL_RECEIVER_ENDPOINT, "http://127.0.0.1:8473");
    }

    #[test]
    fn receiver_exe_name_is_the_bundled_binary() {
        let path = receiver_exe_path();
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("llmdil-log-receiver.exe")
        );
    }
}
