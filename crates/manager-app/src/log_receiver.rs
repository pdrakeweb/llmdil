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
use std::time::Duration;

use manager_services::{ServiceControl, ServiceState, ServiceTimeouts, StopOutcome};
use manager_wizard::SystemOps as _;
use serde_json::{json, Value};

use crate::lifecycle::{self, Intent, LifecycleState, Origin, TransitionKind};
use crate::{ApiError, ApiResult, AppState, EventSink};

/// Freshness of the SCM snapshot the receiver's status reads.
const SCM_MAX_AGE: Duration = Duration::from_secs(1);

/// How long a start or restart waits for the ingest port to accept
/// connections after the SCM reports Running (the service host reports
/// Running at spawn, before the receiver has bound its port).
const INGEST_READY_WAIT: Duration = Duration::from_secs(10);

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
    // The raw SCM row (from the shared, briefly cached SCM snapshot)
    // and the one derived lifecycle view: a transition first, then the
    // SCM state, with running downgraded to degraded while the ingest
    // port refuses connections (OS-12).
    let view = lifecycle::log_receiver_view(state).await;
    let queried = lifecycle::scm_status(
        state,
        manager_services::LOG_RECEIVER_SERVICE_NAME,
        SCM_MAX_AGE,
    )
    .await;
    let live = queried
        .as_ref()
        .filter(|s| !matches!(s.state, manager_services::ServiceState::NotInstalled));
    let receiver_exe = receiver_exe_path();
    Ok(json!({
        "service_name": manager_services::LOG_RECEIVER_SERVICE_NAME,
        "registered": live.is_some(),
        // The raw SCM state (kept for older clients); `lifecycle` is the
        // shared vocabulary every surface renders.
        "state": live.map(|s| &s.state),
        "lifecycle": view.state,
        "transition": view.transition,
        "exit_code": view.exit_code,
        "failure_reason": view.failure_reason,
        "ingest_port": lifecycle::LOG_RECEIVER_INGEST_PORT,
        "ingest_ok": view.state == LifecycleState::Running,
        "pid": live.and_then(|s| s.pid),
        "receiver_exe": receiver_exe.display().to_string(),
        "receiver_present": receiver_exe.exists(),
        "data_dir": dir.display().to_string(),
        "ingest_token_seeded": dir.join("ingest-tokens.txt").exists(),
        "read_token_seeded": dir.join("read-token.txt").exists(),
        "endpoint": cfg.log_streaming.endpoint,
        "endpoint_is_local": cfg.log_streaming.endpoint.trim() == LOCAL_RECEIVER_ENDPOINT,
        "streaming_enabled": cfg.log_streaming.enabled,
        // What the shipper is doing, per source (active / disabled /
        // file_missing / …): see `logship::shipper_surface`.
        "shipper": crate::logship::shipper_surface(
            &cfg.log_streaming,
            crate::logship::log_streaming_status(state).as_ref(),
            crate::logship::log_streaming_note(state).as_deref(),
        ),
    }))
}

/// Drive the receiver's service through one lifecycle transition with
/// the shared SCM helpers (blocking): `Stopping` → stop_and_wait,
/// `Starting` → start_and_wait, `Restarting` → both, inside the one
/// transition. Returns the marker detail, or the failure reason (a
/// start that falls back to Stopped carries the service host's
/// exit-code reason).
pub fn receiver_scm_transition(
    control: &dyn ServiceControl,
    name: &str,
    kind: TransitionKind,
) -> Result<String, String> {
    let mut parts: Vec<String> = Vec::new();
    if matches!(kind, TransitionKind::Stopping | TransitionKind::Restarting) {
        let outcome = manager_services::stop_and_wait(control, name, ServiceTimeouts::stop(name))
            .map_err(|e| format!("stop failed: {e}"))?;
        match outcome {
            StopOutcome::NotRegistered => {
                return Err(format!("service {name} is not registered"));
            }
            StopOutcome::AlreadyStopped => parts.push("was not running".to_string()),
            StopOutcome::Stopped { waited_ms } => {
                parts.push(format!("service stopped in {waited_ms} ms"))
            }
        }
    }
    if matches!(kind, TransitionKind::Starting | TransitionKind::Restarting) {
        let status = manager_services::start_and_wait(control, name, ServiceTimeouts::start(name))
            .map_err(|e| format!("start failed: {e}"))?;
        parts.push(match status.pid {
            Some(pid) => format!("service running, pid {pid}"),
            None => "service running".to_string(),
        });
    }
    Ok(parts.join("; "))
}

/// Wait up to `timeout` for the receiver's local ingest port to accept
/// a TCP connection.
async fn wait_ingest_ready(timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let ok = tokio::time::timeout(
            Duration::from_millis(300),
            tokio::net::TcpStream::connect(("127.0.0.1", lifecycle::LOG_RECEIVER_INGEST_PORT)),
        )
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false);
        if ok {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// One receiver lifecycle operation (start / stop / restart), shared by
/// the three ops: refused when the receiver is not deployed; takes the
/// receiver's lifecycle transition (another receiver operation in
/// flight refuses it with [`ApiError::Busy`]); records the intent
/// (an operator stop before the stop is issued, so the monitor never
/// reads it as a crash); drives the SCM through the shared
/// stop_and_wait / start_and_wait helpers on the blocking pool; after a
/// start, waits briefly for the ingest port; and writes the markers
/// into `receiver.lifecycle.log` (the same file the service host
/// writes). Runs unelevated: the registration's DACL grants the
/// operator start/stop (SCM-15/SCM-19/OS-12).
async fn receiver_lifecycle(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    kind: TransitionKind,
    origin: Option<String>,
) -> ApiResult {
    let origin = origin.as_deref().map(Origin::parse).unwrap_or(Origin::Ui);
    let name = manager_services::LOG_RECEIVER_SERVICE_NAME;
    let registered = lifecycle::scm_status(state, name, Duration::ZERO)
        .await
        .is_some_and(|s| !matches!(s.state, ServiceState::NotInstalled));
    if !registered {
        return Err(ApiError::InvalidInput(
            "the log receiver is not deployed: deploy it from Settings > Supervision > Log receiver"
                .to_string(),
        ));
    }
    let guard = lifecycle::begin(state, sink, lifecycle::LOG_RECEIVER, kind, origin, None).await?;
    state.lifecycle.set_intent(
        lifecycle::LOG_RECEIVER,
        if kind == TransitionKind::Stopping {
            Intent::Stopped
        } else {
            Intent::Running
        },
    );
    let result = tokio::task::spawn_blocking(move || {
        let control = manager_services::scm::platform_control();
        receiver_scm_transition(&control, name, kind)
    })
    .await
    .map_err(|e| format!("log receiver {} task failed: {e}", kind.verb()))
    .and_then(|r| r);
    let detail = match result {
        Ok(detail) => detail,
        Err(reason) => {
            lifecycle::finish(state, sink.as_ref(), guard, Err(reason.clone()));
            return Err(ApiError::Services(format!(
                "log receiver {}: {reason}",
                kind.verb()
            )));
        }
    };
    let ingest_ok = if kind == TransitionKind::Stopping {
        None
    } else {
        Some(wait_ingest_ready(INGEST_READY_WAIT).await)
    };
    let detail = match ingest_ok {
        Some(false) => format!(
            "{detail}; ingest port {} not accepting connections yet",
            lifecycle::LOG_RECEIVER_INGEST_PORT
        ),
        _ => detail,
    };
    lifecycle::finish(state, sink.as_ref(), guard, Ok(detail.clone()));
    let view = lifecycle::log_receiver_view(state).await;
    Ok(json!({
        "ok": true,
        "service_name": name,
        "action": kind.verb(),
        "detail": detail,
        "ingest_ok": ingest_ok,
        "lifecycle": view.state,
        "pid": view.pid,
    }))
}

/// Start the log receiver service (unelevated). See [`receiver_lifecycle`].
pub async fn op_log_receiver_start(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    origin: Option<String>,
) -> ApiResult {
    receiver_lifecycle(state, sink, TransitionKind::Starting, origin).await
}

/// Stop the log receiver service (unelevated). The operator's intent is
/// recorded before the stop, so the stop is never reported as a crash.
pub async fn op_log_receiver_stop(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    origin: Option<String>,
) -> ApiResult {
    receiver_lifecycle(state, sink, TransitionKind::Stopping, origin).await
}

/// Restart the log receiver service (unelevated): stop and start inside
/// one `restarting` transition, so no surface shows it down meanwhile.
/// No re-deploy (and no UAC prompt) is needed to restart it (SCM-19).
pub async fn op_log_receiver_restart(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    origin: Option<String>,
) -> ApiResult {
    receiver_lifecycle(state, sink, TransitionKind::Restarting, origin).await
}

/// Deploy (or repair) the receiver: run the bundled binary's
/// `deploy` subcommand elevated. On success the result carries
/// `read_token_new` when this deploy generated the read token —
/// the one moment the value is visible, for Secure Vault filing;
/// a repair that kept the existing token reports it as null.
///
/// The elevated helper stops, re-registers and starts the service, so
/// the whole run is one receiver lifecycle transition (`restarting`
/// when the receiver is live — a repair — else `starting`): every
/// surface reads it as such instead of stopping / not installed /
/// starting, and the monitor never records the helper's stop as an
/// outside stop. Intent Running is recorded before the helper runs.
pub async fn op_log_receiver_deploy(state: &Arc<AppState>, sink: &Arc<dyn EventSink>) -> ApiResult {
    let exe = require_receiver_exe()?;
    let live = lifecycle::scm_status(
        state,
        manager_services::LOG_RECEIVER_SERVICE_NAME,
        Duration::ZERO,
    )
    .await
    .is_some_and(|s| lifecycle::scm_live(&s));
    let kind = if live {
        TransitionKind::Restarting
    } else {
        TransitionKind::Starting
    };
    let guard = lifecycle::begin(
        state,
        sink,
        lifecycle::LOG_RECEIVER,
        kind,
        Origin::Ui,
        Some("deploy"),
    )
    .await?;
    state
        .lifecycle
        .set_intent(lifecycle::LOG_RECEIVER, Intent::Running);
    let result = deploy_inner(state, exe).await;
    match &result {
        Ok(_) => lifecycle::finish(state, sink.as_ref(), guard, Ok("deployed".to_string())),
        Err(e) => lifecycle::finish(state, sink.as_ref(), guard, Err(e.to_string())),
    }
    result
}

/// The elevated `deploy` run of [`op_log_receiver_deploy`].
async fn deploy_inner(state: &Arc<AppState>, exe: std::path::PathBuf) -> ApiResult {
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
/// The store and token files stay in the data dir. Runs as one
/// `stopping` transition with intent Stopped, so the helper's stop is
/// never recorded as an outside stop or a crash.
pub async fn op_log_receiver_remove(state: &Arc<AppState>, sink: &Arc<dyn EventSink>) -> ApiResult {
    let exe = require_receiver_exe()?;
    let guard = lifecycle::begin(
        state,
        sink,
        lifecycle::LOG_RECEIVER,
        TransitionKind::Stopping,
        Origin::Ui,
        Some("remove"),
    )
    .await?;
    state
        .lifecycle
        .set_intent(lifecycle::LOG_RECEIVER, Intent::Stopped);
    let result = remove_inner(exe).await;
    match &result {
        Ok(_) => lifecycle::finish(state, sink.as_ref(), guard, Ok("removed".to_string())),
        Err(e) => lifecycle::finish(state, sink.as_ref(), guard, Err(e.to_string())),
    }
    result
}

/// The elevated `remove` run of [`op_log_receiver_remove`].
async fn remove_inner(exe: std::path::PathBuf) -> ApiResult {
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
/// config changes; the shipper restarts onto the new endpoint as
/// part of the flip. Deliberately separate from deploy.
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
    drop(cfg);
    // The shipper restarts onto the new endpoint now.
    crate::logship::apply_log_streaming(state).await;
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

    use std::collections::VecDeque;
    use std::sync::Mutex;

    use manager_services::{ServiceDefinition, ServiceError, ServiceStatus};

    /// A scripted SCM: `query` replays `states` (the last repeats);
    /// `start`/`stop` are recorded.
    struct ScriptedScm {
        states: Mutex<VecDeque<ServiceStatus>>,
        calls: Mutex<Vec<&'static str>>,
    }

    impl ScriptedScm {
        fn new(states: Vec<ServiceStatus>) -> Self {
            Self {
                states: Mutex::new(states.into()),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ServiceControl for ScriptedScm {
        fn query(&self, _name: &str) -> Result<ServiceStatus, ServiceError> {
            let mut states = self.states.lock().unwrap();
            Ok(if states.len() > 1 {
                states.pop_front().unwrap()
            } else {
                states.front().cloned().unwrap()
            })
        }
        fn start(&self, _name: &str) -> Result<(), ServiceError> {
            self.calls.lock().unwrap().push("start");
            Ok(())
        }
        fn stop(&self, _name: &str) -> Result<(), ServiceError> {
            self.calls.lock().unwrap().push("stop");
            Ok(())
        }
        fn register(
            &self,
            _: &ServiceDefinition,
            _: &str,
            _: Option<&str>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        fn unregister(&self, _name: &str) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    fn st(state: ServiceState, specific: u32) -> ServiceStatus {
        ServiceStatus {
            name: "svc".to_string(),
            state,
            pid: matches!(state, ServiceState::Running).then_some(42),
            child_pid: None,
            win32_exit_code: if specific != 0 {
                manager_services::ERROR_SERVICE_SPECIFIC_ERROR
            } else {
                0
            },
            service_specific_exit_code: specific,
        }
    }

    #[test]
    fn receiver_transition_start_stop_restart() {
        let scm = ScriptedScm::new(vec![
            st(ServiceState::Stopped, 0),
            st(ServiceState::Running, 0),
        ]);
        let detail = receiver_scm_transition(&scm, "svc", TransitionKind::Starting).unwrap();
        assert_eq!(detail, "service running, pid 42");
        assert_eq!(scm.calls(), vec!["start"]);

        let scm = ScriptedScm::new(vec![
            st(ServiceState::Running, 0),
            st(ServiceState::Stopped, 0),
        ]);
        let detail = receiver_scm_transition(&scm, "svc", TransitionKind::Stopping).unwrap();
        assert!(detail.starts_with("service stopped in "), "{detail}");
        assert_eq!(scm.calls(), vec!["stop"]);

        let scm = ScriptedScm::new(vec![st(ServiceState::Stopped, 0)]);
        assert_eq!(
            receiver_scm_transition(&scm, "svc", TransitionKind::Stopping).unwrap(),
            "was not running"
        );
        assert!(scm.calls().is_empty());

        // Restart: stop, then start, inside one transition.
        let scm = ScriptedScm::new(vec![
            st(ServiceState::Running, 0),
            st(ServiceState::Stopped, 0),
            st(ServiceState::Stopped, 0),
            st(ServiceState::Running, 0),
        ]);
        let detail = receiver_scm_transition(&scm, "svc", TransitionKind::Restarting).unwrap();
        assert!(detail.ends_with("; service running, pid 42"), "{detail}");
        assert_eq!(scm.calls(), vec!["stop", "start"]);
    }

    #[test]
    fn receiver_transition_failures_carry_the_reason() {
        let scm = ScriptedScm::new(vec![st(ServiceState::NotInstalled, 0)]);
        let err = receiver_scm_transition(&scm, "svc", TransitionKind::Stopping).unwrap_err();
        assert!(err.contains("not registered"), "{err}");

        // A start that falls back to Stopped with the shim's spawn code.
        let scm = ScriptedScm::new(vec![
            st(ServiceState::Stopped, 0),
            st(ServiceState::Stopped, 2),
        ]);
        let err = receiver_scm_transition(&scm, "svc", TransitionKind::Starting).unwrap_err();
        assert!(err.starts_with("start failed: "), "{err}");
        assert!(err.contains("could not spawn"), "{err}");
    }

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
