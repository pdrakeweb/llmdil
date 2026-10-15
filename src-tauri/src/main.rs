//! Tauri 2 command/event API for the Local LLM Service Manager (spec §14).
//!
//! The webview never spawns processes, touches the filesystem, or holds
//! secrets. All privileged operations are Tauri commands executed here,
//! delegating to the `manager-*` crates.
//!
//! Wiring status: all §14 commands are wired to real crate functions.
//! Subsystems are lazily initialized (`OnceCell`) so a failure surfaces on
//! first use of that subsystem, never at startup.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use manager_config::{
    self, AppConfig, BackendPatch, ComponentUpdate, ConfigDiff, GatewayConfig, MxcMode,
};
use manager_models::{self, DownloadHandle};
use manager_mxc::{self, Policy};
use manager_supervisor::{test_request, BackoffPolicy, HealthProbe, HttpHealthProbe, Supervisor};
use manager_telemetry::{GpuSample, TelemetryStore};
use manager_wizard::{
    ops::RealSystemOps,
    steps::{continue_editor_config, default_steps, VscodeExtensions},
    RunMode, StepOutcome, StepState, Wizard, WizardContext, WizardReport,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{Mutex, OnceCell, RwLock};

// ---------------------------------------------------------------------------
// Events (Rust -> UI, spec §14)
// ---------------------------------------------------------------------------

/// Frontend event names. The four primary streams are emitted below; the rest
/// are reserved per spec §14 and go live with their producers.
pub mod events {
    pub const TELEMETRY_UPDATE: &str = "telemetry-update";
    pub const LOG_LINE: &str = "log-line";
    pub const WIZARD_PROGRESS: &str = "wizard-progress";
    pub const DOWNLOAD_PROGRESS: &str = "download-progress";
    // Reserved (spec §14): emitted by future producers.
    pub const TELEMETRY_TICK: &str = "telemetry-tick";
    pub const TASK_PROGRESS: &str = "task-progress";
    pub const BACKEND_STATUS: &str = "backend-status";
    pub const GATEWAY_REQUEST: &str = "gateway-request";
    pub const ALERT_RAISED: &str = "alert-raised";
    pub const VRAM_CHANGED: &str = "vram-changed";
    pub const CONFIG_CHANGED: &str = "config-changed";
}

/// Payload for [`events::TELEMETRY_UPDATE`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryUpdatePayload {
    pub samples: Vec<GpuSample>,
    pub at_ms: u64,
}

/// Payload for [`events::LOG_LINE`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLinePayload {
    pub scope: String,
    pub level: String,
    pub line: String,
    pub ts_ms: u64,
}

/// Payload for [`events::WIZARD_PROGRESS`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WizardProgressPayload {
    pub task_id: String,
    pub step_id: Option<String>,
    pub state: StepState,
    pub done: usize,
    pub total: usize,
    pub message: String,
}

/// Payload for [`events::DOWNLOAD_PROGRESS`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadProgressPayload {
    pub model_id: String,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub fraction: Option<f64>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Backend error surfaced to the UI as a string. Variants distinguish
/// implemented-but-failing paths from known contract gaps so the UI can
/// render them differently.
#[derive(Debug)]
pub enum ApiError {
    Config(String),
    Supervisor(String),
    Wizard(String),
    Telemetry(String),
    Gateway(String),
    Models(String),
    Mxc(String),
    Mcp(String),
    WinMl(String),
    NotFound(String),
    /// Caller supplied an invalid argument (e.g. a non-http URL).
    InvalidInput(String),
    /// A crate's public API cannot express what the command needs.
    /// Reported to the parent; do not silently work around.
    ContractGap(String),
    /// Explicitly deferred to a later version (spec §16: Updates are P6).
    Unsupported(String),
    Io(String),
    Json(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Config(e) => write!(f, "config error: {e}"),
            ApiError::Supervisor(e) => write!(f, "supervisor error: {e}"),
            ApiError::Wizard(e) => write!(f, "wizard error: {e}"),
            ApiError::Telemetry(e) => write!(f, "telemetry error: {e}"),
            ApiError::Gateway(e) => write!(f, "gateway error: {e}"),
            ApiError::Models(e) => write!(f, "models error: {e}"),
            ApiError::Mxc(e) => write!(f, "mxc error: {e}"),
            ApiError::Mcp(e) => write!(f, "mcp error: {e}"),
            ApiError::WinMl(e) => write!(f, "winml error: {e}"),
            ApiError::NotFound(e) => write!(f, "not found: {e}"),
            ApiError::InvalidInput(e) => write!(f, "invalid input: {e}"),
            ApiError::ContractGap(e) => write!(f, "contract gap: {e}"),
            ApiError::Unsupported(e) => write!(f, "unsupported in this version: {e}"),
            ApiError::Io(e) => write!(f, "io error: {e}"),
            ApiError::Json(e) => write!(f, "json error: {e}"),
        }
    }
}

impl From<manager_config::ConfigError> for ApiError {
    fn from(e: manager_config::ConfigError) -> Self {
        ApiError::Config(format!("{e:?}"))
    }
}
impl From<manager_supervisor::SupervisorError> for ApiError {
    fn from(e: manager_supervisor::SupervisorError) -> Self {
        ApiError::Supervisor(format!("{e:?}"))
    }
}
impl From<manager_wizard::WizardError> for ApiError {
    fn from(e: manager_wizard::WizardError) -> Self {
        ApiError::Wizard(format!("{e:?}"))
    }
}
impl From<manager_telemetry::TelemetryError> for ApiError {
    fn from(e: manager_telemetry::TelemetryError) -> Self {
        ApiError::Telemetry(format!("{e:?}"))
    }
}
impl From<manager_gateway::GatewayError> for ApiError {
    fn from(e: manager_gateway::GatewayError) -> Self {
        ApiError::Gateway(format!("{e:?}"))
    }
}
impl From<manager_models::ModelError> for ApiError {
    fn from(e: manager_models::ModelError) -> Self {
        ApiError::Models(format!("{e:?}"))
    }
}
impl From<manager_mxc::MxcError> for ApiError {
    fn from(e: manager_mxc::MxcError) -> Self {
        ApiError::Mxc(format!("{e:?}"))
    }
}
impl From<manager_winml::WinMlError> for ApiError {
    fn from(e: manager_winml::WinMlError) -> Self {
        ApiError::WinMl(format!("{e:?}"))
    }
}
impl From<manager_mcp::McpError> for ApiError {
    fn from(e: manager_mcp::McpError) -> Self {
        ApiError::Mcp(format!("{e:?}"))
    }
}
impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        ApiError::Io(e.to_string())
    }
}
impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        ApiError::Json(e.to_string())
    }
}

type ApiResult = Result<Value, String>;

fn err(e: ApiError) -> String {
    e.to_string()
}

/// `map_err` adapter for any error convertible into [`ApiError`]
/// (crate errors, `serde_json::Error`, `std::io::Error`).
fn jerr<E: Into<ApiError>>(e: E) -> String {
    e.into().to_string()
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

/// Lifecycle of a long-running operation surfaced to the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Running,
    Done,
    Failed,
    Aborted,
}

/// In-memory record for a long-running operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: String,
    pub kind: String,
    pub status: TaskStatus,
    pub detail: Value,
    pub created_ms: u64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

/// Shared backend state. Subsystems are lazily initialized (`OnceCell`) so
/// a failure surfaces on first use of that subsystem, never at startup.
pub struct AppState {
    config: RwLock<AppConfig>,
    config_path: PathBuf,
    data_dir: PathBuf,
    supervisor: OnceCell<Mutex<Supervisor>>,
    telemetry: OnceCell<Mutex<TelemetryStore>>,
    wizard_install: OnceCell<Mutex<Wizard>>,
    wizard_audit: OnceCell<Mutex<Wizard>>,
    tasks: Mutex<HashMap<String, TaskRecord>>,
    task_counter: AtomicU64,
    /// Live model-download handles by model id (spec §14 download commands).
    downloads: Mutex<HashMap<String, DownloadHandle>>,
    /// MCP server modes, overlaid on `manager_mcp::default_servers()`.
    /// (No MCP section exists in `AppConfig` yet; modes are in-memory until
    /// one lands.)
    mcp_modes: Mutex<HashMap<String, manager_mcp::McpMode>>,
    /// Recent gateway routing decisions (spec §14 `gateway_recent_requests`).
    /// Every `gateway_test_routing` trace records here.
    routing_log: Mutex<manager_gateway::RoutingLog>,
    /// WinML secondary-backend registry (stateful single-model rule).
    winml: OnceCell<Mutex<manager_winml::WinMlRegistry>>,
}

impl AppState {
    fn new(config: AppConfig, config_path: PathBuf, data_dir: PathBuf) -> Self {
        Self {
            config: RwLock::new(config),
            config_path,
            data_dir,
            supervisor: OnceCell::new(),
            telemetry: OnceCell::new(),
            wizard_install: OnceCell::new(),
            wizard_audit: OnceCell::new(),
            tasks: Mutex::new(HashMap::new()),
            task_counter: AtomicU64::new(1),
            downloads: Mutex::new(HashMap::new()),
            mcp_modes: Mutex::new(HashMap::new()),
            routing_log: Mutex::new(manager_gateway::RoutingLog::default()),
            winml: OnceCell::new(),
        }
    }

    fn next_task_id(&self) -> String {
        let n = self.task_counter.fetch_add(1, Ordering::Relaxed);
        format!("task-{n:06}")
    }

    async fn record_task(&self, kind: &str, detail: Value) -> String {
        let id = self.next_task_id();
        let record = TaskRecord {
            id: id.clone(),
            kind: kind.to_string(),
            status: TaskStatus::Running,
            detail,
            created_ms: now_ms(),
        };
        self.tasks.lock().await.insert(id.clone(), record);
        id
    }

    async fn finish_task(&self, id: &str, status: TaskStatus, detail: Value) {
        if let Some(rec) = self.tasks.lock().await.get_mut(id) {
            rec.status = status;
            rec.detail = detail;
        }
    }

    async fn wizard_ctx(&self, mode: RunMode) -> WizardContext {
        WizardContext {
            config: self.config.read().await.clone(),
            data_dir: self.data_dir.clone(),
            mode,
            ops: Arc::new(RealSystemOps),
        }
    }

    async fn supervisor(&self) -> &Mutex<Supervisor> {
        self.supervisor
            .get_or_init(|| async {
                Mutex::new(Supervisor::new(BackoffPolicy {
                    base: Duration::from_secs(1),
                    max: Duration::from_secs(60),
                    max_retries: 5,
                }))
            })
            .await
    }

    async fn telemetry(&self) -> &Mutex<TelemetryStore> {
        self.telemetry
            .get_or_init(|| async { Mutex::new(TelemetryStore::new()) })
            .await
    }

    /// WinML registry, built lazily with the real HTTP probe transport.
    /// Transport setup can only fail on local client misconfiguration;
    /// the error is surfaced to the caller instead of panicking.
    async fn winml(&self) -> Result<&Mutex<manager_winml::WinMlRegistry>, String> {
        if let Some(r) = self.winml.get() {
            return Ok(r);
        }
        let transport = manager_winml::ReqwestProbeTransport::new(Duration::from_secs(10))
            .map_err(|e| format!("winml transport setup failed: {e:?}"))?;
        Ok(self
            .winml
            .get_or_init(|| async { Mutex::new(manager_winml::WinMlRegistry::new(transport)) })
            .await)
    }

    async fn wizard(&self, mode: RunMode) -> &Mutex<Wizard> {
        let cell = match mode {
            RunMode::Install => &self.wizard_install,
            RunMode::Audit => &self.wizard_audit,
        };
        cell.get_or_init(|| async {
            Mutex::new(
                Wizard::new(default_steps(), mode)
                    .expect("default_steps() has unique step ids by construction"),
            )
        })
        .await
    }

    /// MCP server registry with in-memory mode overrides applied.
    async fn mcp_servers(&self) -> Vec<manager_mcp::ServerSpec> {
        let modes = self.mcp_modes.lock().await;
        manager_mcp::default_servers()
            .into_iter()
            .map(|mut s| {
                if let Some(m) = modes.get(&s.id) {
                    s.mode = *m;
                }
                s
            })
            .collect()
    }
}

/// Deep-merge a JSON patch into a base value (objects merge recursively).
fn merge_json(base: &mut Value, patch: Value) {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            for (k, v) in p {
                merge_json(b.entry(k).or_insert(Value::Null), v);
            }
        }
        (b, p) => *b = p,
    }
}

// ---------------------------------------------------------------------------
// Status / config
// ---------------------------------------------------------------------------

/// Overall service status snapshot.
#[tauri::command]
async fn get_status(state: State<'_, AppState>) -> ApiResult {
    let cfg = state.config.read().await;
    Ok(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "backends_configured": cfg.backends.len(),
        "gateway_port": cfg.gateway.port,
        "winml_enabled": cfg.winml.enabled,
        "mxc_mode": cfg.mxc.mode,
    }))
}

/// Full application configuration.
#[tauri::command]
async fn get_config(state: State<'_, AppState>) -> ApiResult {
    let cfg = state.config.read().await;
    Ok(serde_json::to_value(&*cfg).map_err(jerr)?)
}

/// Merge a JSON patch into the config, validate, and persist to disk via
/// `manager_config::save` (atomic write with `.bak` rotation). The merged
/// value is kept in memory even when the save fails; `persisted` reports
/// which happened.
#[tauri::command]
async fn update_config(state: State<'_, AppState>, patch: Value) -> ApiResult {
    let mut cfg = state.config.write().await;
    let mut v = serde_json::to_value(&*cfg).map_err(jerr)?;
    merge_json(&mut v, patch);
    let new_cfg: AppConfig = serde_json::from_value(v).map_err(jerr)?;
    // `save` validates before writing, so a failed save never clobbers the
    // last-known-good file; the in-memory value is kept either way.
    match manager_config::save(&new_cfg, &state.config_path) {
        Ok(()) => {
            *cfg = new_cfg;
            Ok(json!({ "ok": true, "persisted": true }))
        }
        Err(e) => {
            *cfg = new_cfg;
            Ok(json!({
                "ok": true,
                "persisted": false,
                "error": e.to_string(),
            }))
        }
    }
}

// ---------------------------------------------------------------------------
// Backends
// ---------------------------------------------------------------------------

/// Configured backends with their launch ports and models.
#[tauri::command]
async fn list_backends(state: State<'_, AppState>) -> ApiResult {
    let cfg = state.config.read().await;
    let backends: Vec<Value> = cfg
        .backends
        .iter()
        .map(|b| {
            json!({
                "id": b.id,
                "enabled": b.enabled,
                "port": b.port,
                "model_file": b.model_file,
            })
        })
        .collect();
    Ok(json!({ "backends": backends }))
}

/// Start a backend (supervisor spawn). Returns a task id.
#[tauri::command]
async fn start_backend(app: AppHandle, state: State<'_, AppState>, id: String) -> ApiResult {
    let task_id = state
        .record_task("backend_start", json!({ "backend_id": id }))
        .await;
    let outcome = state
        .supervisor()
        .await
        .lock()
        .await
        .spawn_backend(&id)
        .await;
    match outcome {
        Ok(()) => {
            state
                .finish_task(&task_id, TaskStatus::Done, json!({ "backend_id": id }))
                .await;
            let _ = app.emit(
                events::BACKEND_STATUS,
                json!({ "id": id, "event": "started" }),
            );
            Ok(json!({ "task_id": task_id, "backend_id": id }))
        }
        Err(e) => {
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Failed,
                    json!({ "backend_id": id, "error": format!("{e:?}") }),
                )
                .await;
            Err(err(ApiError::from(e)))
        }
    }
}

/// Stop a backend, gracefully (drain) or immediately.
#[tauri::command]
async fn stop_backend(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    graceful: bool,
) -> ApiResult {
    let task_id = state
        .record_task(
            "backend_stop",
            json!({ "backend_id": id, "graceful": graceful }),
        )
        .await;
    let outcome = state
        .supervisor()
        .await
        .lock()
        .await
        .stop(&id, graceful)
        .await;
    match outcome {
        Ok(()) => {
            state
                .finish_task(&task_id, TaskStatus::Done, json!({ "backend_id": id }))
                .await;
            let _ = app.emit(
                events::BACKEND_STATUS,
                json!({ "id": id, "event": "stopped" }),
            );
            Ok(json!({ "task_id": task_id, "backend_id": id }))
        }
        Err(e) => {
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Failed,
                    json!({ "error": format!("{e:?}") }),
                )
                .await;
            Err(err(ApiError::from(e)))
        }
    }
}

/// Restart a backend (stop + start under the supervisor).
#[tauri::command]
async fn restart_backend(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    graceful: bool,
) -> ApiResult {
    let task_id = state
        .record_task(
            "backend_restart",
            json!({ "backend_id": id, "graceful": graceful }),
        )
        .await;
    let outcome = state
        .supervisor()
        .await
        .lock()
        .await
        .restart(&id, graceful)
        .await;
    match outcome {
        Ok(()) => {
            state
                .finish_task(&task_id, TaskStatus::Done, json!({ "backend_id": id }))
                .await;
            let _ = app.emit(
                events::BACKEND_STATUS,
                json!({ "id": id, "event": "restarted" }),
            );
            Ok(json!({ "task_id": task_id, "backend_id": id }))
        }
        Err(e) => {
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Failed,
                    json!({ "error": format!("{e:?}") }),
                )
                .await;
            Err(err(ApiError::from(e)))
        }
    }
}

/// One raw prompt against a backend; raw JSON result, no history (spec §1.3).
/// Minimal inference test request against a backend's
/// `/v1/chat/completions` (spec §14 `backend_test_request`).
#[tauri::command]
async fn test_backend(state: State<'_, AppState>, id: String, prompt: String) -> ApiResult {
    let cfg = state.config.read().await;
    let backend = cfg
        .backends
        .iter()
        .find(|b| b.id == id)
        .ok_or_else(|| err(ApiError::NotFound(format!("backend {id}"))))?;
    let base_url = format!("http://127.0.0.1:{}", backend.port);
    let result = test_request(&base_url, &prompt)
        .await
        .map_err(ApiError::from)
        .map_err(jerr)?;
    Ok(serde_json::to_value(&result).map_err(jerr)?)
}

// ---------------------------------------------------------------------------
// GPU telemetry
// ---------------------------------------------------------------------------

/// Current GPU telemetry snapshot; also emits `telemetry-update`.
#[tauri::command]
async fn get_gpu_telemetry(app: AppHandle, state: State<'_, AppState>) -> ApiResult {
    let samples: Vec<GpuSample> = state.telemetry().await.lock().await.snapshot();
    let payload = TelemetryUpdatePayload {
        samples: samples.clone(),
        at_ms: now_ms(),
    };
    let _ = app.emit(events::TELEMETRY_UPDATE, &payload);
    Ok(serde_json::to_value(&payload).map_err(jerr)?)
}

// ---------------------------------------------------------------------------
// Wizard / audit
// ---------------------------------------------------------------------------

/// Run one wizard step; streams `wizard-progress`. Returns a task id.
#[tauri::command]
async fn run_wizard_step(app: AppHandle, state: State<'_, AppState>, step_id: String) -> ApiResult {
    let task_id = state
        .record_task("wizard_step", json!({ "step_id": step_id }))
        .await;
    let ctx = state.wizard_ctx(RunMode::Install).await;
    let wizard = state.wizard(RunMode::Install).await;
    let mut guard = wizard.lock().await;
    let (_done_before, total) = guard.progress();
    let outcome: StepOutcome = guard
        .run_step(&ctx, &step_id)
        .await
        .map_err(ApiError::from)
        .map_err(jerr)?;
    let (done, _) = guard.progress();
    drop(guard);
    let payload = WizardProgressPayload {
        task_id: task_id.clone(),
        step_id: Some(step_id.clone()),
        state: outcome.state,
        done,
        total,
        message: outcome.message.clone(),
    };
    let _ = app.emit(events::WIZARD_PROGRESS, &payload);
    state
        .finish_task(
            &task_id,
            TaskStatus::Done,
            serde_json::to_value(&outcome).unwrap_or(Value::Null),
        )
        .await;
    Ok(json!({ "task_id": task_id, "outcome": outcome }))
}

/// Re-run audit: re-executes check-type steps without reinstalling (spec §4.1).
#[tauri::command]
async fn run_audit(app: AppHandle, state: State<'_, AppState>) -> ApiResult {
    let task_id = state.record_task("audit", json!({})).await;
    let ctx = state.wizard_ctx(RunMode::Audit).await;
    let wizard = state.wizard(RunMode::Audit).await;
    let mut guard = wizard.lock().await;
    let report: WizardReport = guard.run_all(&ctx).await;
    let (done, total) = report.progress();
    drop(guard);
    let payload = WizardProgressPayload {
        task_id: task_id.clone(),
        step_id: None,
        state: StepState::Done,
        done,
        total,
        message: format!("audit complete: {done}/{total} checks passed"),
    };
    let _ = app.emit(events::WIZARD_PROGRESS, &payload);
    state
        .finish_task(
            &task_id,
            TaskStatus::Done,
            json!({ "done": done, "total": total }),
        )
        .await;
    Ok(json!({ "task_id": task_id, "done": done, "total": total }))
}

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

/// Model catalog from config. Fit-check is unavailable until the catalog
/// carries size/download-URL fields (see CONTRACT GAP in download_model).
#[tauri::command]
async fn list_models(state: State<'_, AppState>) -> ApiResult {
    let cfg = state.config.read().await;
    let models: Vec<Value> = cfg
        .models
        .iter()
        .map(|m| {
            json!({
                "id": m.id,
                "gguf_path": m.gguf_path,
                "quant": m.quant,
                "params_b": m.params_b,
                "sha256": m.sha256,
                "verified_at": m.verified_at,
                "fit_check_available": false,
            })
        })
        .collect();
    Ok(json!({ "models": models }))
}

/// Build a download spec for a configured model: the curated catalog
/// provides URL/size; the config's sha256 wins when it is a real pinned
/// hash (64 hex chars), otherwise the catalog value stands.
fn download_spec_for(cfg: &AppConfig, id: &str) -> Result<manager_models::ModelSpec, String> {
    let m = cfg
        .models
        .iter()
        .find(|m| m.id == id)
        .ok_or_else(|| err(ApiError::NotFound(format!("model {id}"))))?;
    let mut spec = manager_models::curated_models()
        .into_iter()
        .find(|s| s.id == id)
        .ok_or_else(|| {
            err(ApiError::NotFound(format!(
                "no catalog entry for model {id}"
            )))
        })?;
    if m.sha256.len() == 64 && m.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        spec.sha256 = m.sha256.clone();
    }
    Ok(spec)
}

/// Start a model download; progress streams via `download-progress`.
#[tauri::command]
async fn download_model(app: AppHandle, state: State<'_, AppState>, id: String) -> ApiResult {
    let (spec, dest) = {
        let cfg = state.config.read().await;
        let spec = download_spec_for(&cfg, &id)?;
        (
            spec,
            state.data_dir.join("models").join(format!("{id}.gguf")),
        )
    };
    if state.downloads.lock().await.contains_key(&id) {
        return Err(err(ApiError::Models(format!(
            "download already in flight for model {id}"
        ))));
    }
    let app_handle = app.clone();
    let model_id = id.clone();
    let handle = manager_models::download(&spec, &dest, move |progress| {
        let payload = DownloadProgressPayload {
            model_id: model_id.clone(),
            downloaded_bytes: progress.downloaded_bytes,
            total_bytes: Some(progress.total_bytes),
            fraction: Some(progress.fraction()),
        };
        let _ = app_handle.emit(events::DOWNLOAD_PROGRESS, &payload);
    })
    .await
    .map_err(ApiError::from)
    .map_err(jerr)?;
    state.downloads.lock().await.insert(id.clone(), handle);
    Ok(json!({ "model_id": id, "dest": dest }))
}

/// Cancel an in-flight download (partial file removed by the worker).
#[tauri::command]
async fn cancel_download(state: State<'_, AppState>, id: String) -> ApiResult {
    let handle = state.downloads.lock().await.remove(&id).ok_or_else(|| {
        err(ApiError::NotFound(format!(
            "no download in flight for {id}"
        )))
    })?;
    handle
        .cancel()
        .await
        .map_err(ApiError::from)
        .map_err(jerr)?;
    Ok(json!({ "model_id": id, "cancelled": true }))
}

// ---------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------

/// Query captured logs. `scope` is a process id or `"all"`; `level` is an
/// optional error/warn/info/debug heuristic filter; `search` is an optional
/// case-insensitive substring; newest-first, capped at `limit` (default 200).
#[tauri::command]
async fn get_logs(
    state: State<'_, AppState>,
    scope: String,
    level: Option<String>,
    search: Option<String>,
    limit: Option<u64>,
) -> ApiResult {
    let records = state.supervisor().await.lock().await.logs_query(
        &scope,
        level.as_deref(),
        search.as_deref(),
        limit.unwrap_or(200) as usize,
    );
    Ok(serde_json::to_value(&records).map_err(jerr)?)
}

// ---------------------------------------------------------------------------
// Gateway
// ---------------------------------------------------------------------------

/// Current gateway configuration (from AppConfig).
#[tauri::command]
async fn get_gateway_config(state: State<'_, AppState>) -> ApiResult {
    let cfg = state.config.read().await;
    let gw: &GatewayConfig = &cfg.gateway;
    Ok(serde_json::to_value(gw).map_err(jerr)?)
}

/// Merge a JSON patch into the gateway config (in-memory until save lands).
#[tauri::command]
async fn update_gateway_config(state: State<'_, AppState>, patch: Value) -> ApiResult {
    let mut cfg = state.config.write().await;
    let mut v = serde_json::to_value(&cfg.gateway).map_err(jerr)?;
    merge_json(&mut v, patch);
    let new_gw: GatewayConfig = serde_json::from_value(v).map_err(jerr)?;
    cfg.gateway = new_gw;
    Ok(json!({ "ok": true, "persisted": false }))
}

// ---------------------------------------------------------------------------
// MXC
// ---------------------------------------------------------------------------

/// Current MXC policy: the configured policy file, else the built-in
/// default. Same source rule as `get_mxc_policy`.
fn current_mxc_policy(cfg: &AppConfig) -> Result<Policy, String> {
    match std::fs::read_to_string(&cfg.mxc.path) {
        Ok(text) => serde_json::from_str::<Policy>(&text).map_err(jerr),
        Err(_) => Ok(Policy::default_policy()),
    }
}

/// Sandbox policy JSON: from the configured path, else the built-in default.
#[tauri::command]
async fn get_mxc_policy(state: State<'_, AppState>) -> ApiResult {
    let cfg = state.config.read().await;
    let (policy, source) = match std::fs::read_to_string(&cfg.mxc.path) {
        Ok(text) => (serde_json::from_str::<Value>(&text).map_err(jerr)?, "file"),
        Err(_) => (
            serde_json::to_value(Policy::default_policy()).map_err(jerr)?,
            "default",
        ),
    };
    Ok(json!({ "policy": policy, "source": source, "mode": cfg.mxc.mode }))
}

/// Validate and store a new sandbox policy.
#[tauri::command]
async fn update_mxc_policy(state: State<'_, AppState>, policy: Value) -> ApiResult {
    let p: Policy = serde_json::from_value(policy).map_err(jerr)?;
    // Wired to the real validator; live when manager-mxc lands.
    let report = manager_mxc::validate(&p)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    let cfg = state.config.read().await;
    if let Some(parent) = cfg.mxc.path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(ApiError::from)
            .map_err(jerr)?;
    }
    std::fs::write(
        &cfg.mxc.path,
        serde_json::to_string_pretty(&p).map_err(jerr)?,
    )
    .map_err(ApiError::from)
    .map_err(jerr)?;
    Ok(serde_json::to_value(report).map_err(jerr)?)
}

// ---------------------------------------------------------------------------
// Setup wizard: additional §14 commands
// ---------------------------------------------------------------------------

/// Prerequisite verification matrix: per-step prerequisite lists from
/// [`manager_wizard::steps::default_steps`] (each step's `prerequisites()`
/// are the step ids that must be Done or Manual before it may run).
#[tauri::command]
async fn setup_get_prerequisites(state: State<'_, AppState>) -> ApiResult {
    let _ = state;
    let steps: Vec<Value> = manager_wizard::steps::default_steps()
        .iter()
        .map(|s| {
            json!({
                "id": s.id(),
                "name": s.name(),
                "prerequisites": s.prerequisites(),
            })
        })
        .collect();
    Ok(json!({ "steps": steps }))
}

/// Reset a failed step to Queued so it can run again.
#[tauri::command]
async fn setup_retry_step(state: State<'_, AppState>, step_id: String) -> ApiResult {
    let wizard = state.wizard(RunMode::Install).await;
    wizard.lock().await.retry(&step_id);
    Ok(json!({ "ok": true, "step_id": step_id }))
}

/// Skip a step with a note.
#[tauri::command]
async fn setup_skip_step(state: State<'_, AppState>, step_id: String, note: String) -> ApiResult {
    let wizard = state.wizard(RunMode::Install).await;
    wizard.lock().await.skip(&step_id, note);
    Ok(json!({ "ok": true, "step_id": step_id }))
}

/// Exact commands a step will run ("Show commands" UI). Must equal the argv
/// actually spawned by the step.
#[tauri::command]
async fn setup_get_commands(state: State<'_, AppState>, step_id: String) -> ApiResult {
    let ctx = state.wizard_ctx(RunMode::Install).await;
    let wizard = state.wizard(RunMode::Install).await;
    let guard = wizard.lock().await;
    let step = guard
        .get_step(&step_id)
        .ok_or_else(|| err(ApiError::NotFound(format!("wizard step {step_id}"))))?;
    let commands = step.planned_commands(&ctx);
    Ok(serde_json::to_value(&commands).map_err(jerr)?)
}

/// Live wizard step states for the Setup view: id, name, state, last
/// outcome message/log tail, plus remediation + help URL per step.
#[tauri::command]
async fn setup_get_steps(state: State<'_, AppState>) -> ApiResult {
    let wizard = state.wizard(RunMode::Install).await;
    let guard = wizard.lock().await;
    let report = guard.report();
    let steps: Vec<Value> = report
        .steps
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "name": s.name,
                "state": s.state,
                "message": s.message,
                "duration_ms": s.duration_ms,
                "log_tail": s.log_tail,
                "note": s.note,
                "remediation": guard.remediation_hint(&s.id),
                "help_url": guard.help_url(&s.id),
            })
        })
        .collect();
    let (done, total) = report.progress();
    Ok(json!({ "steps": steps, "done": done, "total": total }))
}

/// Whether the user has finished the setup wizard. The UI opens the wizard
/// on launch until this is true.
#[tauri::command]
async fn setup_status(state: State<'_, AppState>) -> ApiResult {
    let cfg = state.config.read().await;
    Ok(json!({ "setup_completed": cfg.setup_completed }))
}

/// Mark the setup wizard complete and persist the flag.
#[tauri::command]
async fn setup_mark_complete(state: State<'_, AppState>) -> ApiResult {
    let mut cfg = state.config.write().await;
    cfg.setup_completed = true;
    match manager_config::save(&cfg, &state.config_path) {
        Ok(()) => Ok(json!({ "ok": true, "persisted": true })),
        Err(e) => Ok(json!({ "ok": true, "persisted": false, "error": e.to_string() })),
    }
}

/// Help URL a wizard step offers alongside its remediation hint
/// (e.g. the NVIDIA driver download page on the hardware-detect step).
/// Returns `{"url": string|null, "step_id": string}`.
#[tauri::command]
async fn setup_step_help_url(state: State<'_, AppState>, step_id: String) -> ApiResult {
    let wizard = state.wizard(RunMode::Install).await;
    let url = wizard.lock().await.help_url(&step_id).map(String::from);
    Ok(json!({ "url": url, "step_id": step_id }))
}

/// Open an external URL in the system browser. Only http/https URLs are
/// accepted; anything else is rejected before spawning.
#[tauri::command]
async fn open_url(url: String) -> ApiResult {
    let ok = url.starts_with("http://") || url.starts_with("https://");
    if !ok {
        return Err(err(ApiError::InvalidInput(
            "open_url accepts http/https URLs only".to_string(),
        )));
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", &url])
            .spawn()
            .map_err(|e| err(ApiError::Io(format!("failed to open URL: {e}"))))?;
        Ok(json!({ "ok": true }))
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err(err(ApiError::Unsupported(
            "open_url is only supported on Windows".to_string(),
        )))
    }
}

/// Mark a step as manually completed by the user.
#[tauri::command]
async fn setup_mark_manual(
    state: State<'_, AppState>,
    step_id: String,
    path: String,
    version: String,
) -> ApiResult {
    let wizard = state.wizard(RunMode::Install).await;
    wizard.lock().await.mark_manual(&step_id, path, version);
    Ok(json!({ "ok": true, "step_id": step_id }))
}

// ---------------------------------------------------------------------------
// Backends: additional §14 commands
// ---------------------------------------------------------------------------

/// Backend configuration by id.
#[tauri::command]
async fn backend_get_config(state: State<'_, AppState>, id: String) -> ApiResult {
    let cfg = state.config.read().await;
    let b = cfg
        .backends
        .iter()
        .find(|b| b.id == id)
        .ok_or_else(|| err(ApiError::NotFound(format!("backend {id}"))))?;
    Ok(serde_json::to_value(b).map_err(jerr)?)
}

/// Preview a config diff before applying (spec §14 `backend_preview_diff`).
#[tauri::command]
async fn backend_preview_diff(state: State<'_, AppState>, id: String, patch: Value) -> ApiResult {
    let patch: BackendPatch = serde_json::from_value(patch).map_err(jerr)?;
    let cfg = state.config.read().await;
    let backend = cfg
        .backends
        .iter()
        .find(|b| b.id == id)
        .ok_or_else(|| err(ApiError::NotFound(format!("backend {id}"))))?;
    let diff: ConfigDiff = manager_config::diff_config(&id, backend, &patch);
    Ok(serde_json::to_value(&diff).map_err(jerr)?)
}

/// Apply a config patch with restart semantics (spec §14
/// `backend_apply_config`). Applied in-memory and validated; the backend is
/// rolled back untouched if validation fails. `apply_mode` is
/// `RestartNow | OnNextRestart` — `RestartNow` records a restart task for
/// the UI to dispatch; the restart itself goes through `restart_backend`.
#[tauri::command]
async fn backend_apply_config(
    state: State<'_, AppState>,
    id: String,
    patch: Value,
    apply_mode: String,
) -> ApiResult {
    let patch: BackendPatch = serde_json::from_value(patch).map_err(jerr)?;
    let apply_mode = match apply_mode.as_str() {
        "RestartNow" => "RestartNow",
        "OnNextRestart" => "OnNextRestart",
        other => {
            return Err(err(ApiError::Config(format!(
                "unknown apply_mode {other:?}; expected RestartNow | OnNextRestart"
            ))))
        }
    };
    let diff: ConfigDiff = {
        let mut cfg = state.config.write().await;
        manager_config::apply_config_diff(&mut *cfg, &id, &patch)
            .map_err(ApiError::from)
            .map_err(jerr)?
    };
    let restart_task = if apply_mode == "RestartNow" {
        Some(
            state
                .record_task("backend_restart", json!({ "backend_id": id }))
                .await,
        )
    } else {
        None
    };
    Ok(json!({
        "backend_id": id,
        "diff": diff,
        "apply_mode": apply_mode,
        "restart_task_id": restart_task,
        "persisted": false,
    }))
}

/// Exact argv the supervisor would spawn (must equal the wizard's
/// planned_commands output per spec).
#[tauri::command]
async fn backend_get_launch_command(state: State<'_, AppState>, id: String) -> ApiResult {
    let argv = state
        .supervisor()
        .await
        .lock()
        .await
        .launch_argv(&id)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    Ok(json!({ "id": id, "argv": argv }))
}

// ---------------------------------------------------------------------------
// Gateway: additional §14 commands
// ---------------------------------------------------------------------------

/// Gateway liveness summary.
#[tauri::command]
async fn gateway_status(state: State<'_, AppState>) -> ApiResult {
    let cfg = state.config.read().await;
    Ok(json!({
        "port": cfg.gateway.port,
        "groups": cfg.gateway.groups.len(),
        "routing_rules": cfg.gateway.routing_rules.len(),
    }))
}

/// Preview the generated LiteLLM config.yaml.
#[tauri::command]
async fn gateway_preview_yaml(state: State<'_, AppState>) -> ApiResult {
    let cfg = state.config.read().await;
    let yaml = manager_gateway::generate_config_yaml(&cfg)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    Ok(json!({ "yaml": yaml }))
}

/// Trace routing for a tagged request: `request` carries the model group
/// name (`{"group": "planner"}`). Runs the live `pick_backend` logic as a
/// dry run (every group member assumed healthy, no load/latency data — the
/// reason string says so) and records the decision in the request ring.
#[tauri::command]
async fn gateway_test_routing(state: State<'_, AppState>, request: Value) -> ApiResult {
    let group_name = request
        .get("group")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            err(ApiError::Gateway(
                "request must carry a \"group\" string".to_string(),
            ))
        })?;
    let cfg = state.config.read().await;
    let group = cfg
        .gateway
        .groups
        .iter()
        .find(|g| g.name == group_name)
        .ok_or_else(|| err(ApiError::NotFound(format!("model group {group_name}"))))?;
    let stats: Vec<manager_gateway::BackendStats> = group
        .members
        .iter()
        .map(|m| manager_gateway::BackendStats::healthy(m.clone()))
        .collect();
    let decision = manager_gateway::trace_routing(group, &stats, false, now_ms());
    state.routing_log.lock().await.record(decision.clone());
    Ok(serde_json::to_value(&decision).map_err(jerr)?)
}

/// Recent gateway routing decisions (traced via `gateway_test_routing`),
/// newest first.
#[tauri::command]
async fn gateway_recent_requests(state: State<'_, AppState>) -> ApiResult {
    let recent = state.routing_log.lock().await.recent();
    Ok(json!({ "requests": recent }))
}

// ---------------------------------------------------------------------------
// Models: additional §14 commands
// ---------------------------------------------------------------------------

/// Pause an in-flight download (partial file kept for resume).
#[tauri::command]
async fn model_pause_download(state: State<'_, AppState>, id: String) -> ApiResult {
    let downloads = state.downloads.lock().await;
    let handle = downloads.get(&id).ok_or_else(|| {
        err(ApiError::NotFound(format!(
            "no download in flight for {id}"
        )))
    })?;
    handle.pause().await.map_err(ApiError::from).map_err(jerr)?;
    Ok(json!({ "model_id": id, "paused": true }))
}

/// Resume a paused download.
#[tauri::command]
async fn model_resume_download(state: State<'_, AppState>, id: String) -> ApiResult {
    let downloads = state.downloads.lock().await;
    let handle = downloads.get(&id).ok_or_else(|| {
        err(ApiError::NotFound(format!(
            "no download in flight for {id}"
        )))
    })?;
    handle
        .resume()
        .await
        .map_err(ApiError::from)
        .map_err(jerr)?;
    Ok(json!({ "model_id": id, "resumed": true }))
}

/// SHA-256-verify an installed model file against the catalog hash.
#[tauri::command]
async fn model_verify_checksum(state: State<'_, AppState>, id: String) -> ApiResult {
    let cfg = state.config.read().await;
    let m = cfg
        .models
        .iter()
        .find(|m| m.id == id)
        .ok_or_else(|| err(ApiError::NotFound(format!("model {id}"))))?;
    let ok = manager_models::verify_sha256(&m.gguf_path, &m.sha256)
        .await
        .map_err(ApiError::from)
        .map_err(jerr)?;
    Ok(json!({ "id": id, "ok": ok }))
}

/// Delete a model file (guarded): refuses while any backend references the
/// model, or while a download is in flight for it. Removes the model from
/// the config on success.
#[tauri::command]
async fn model_delete(state: State<'_, AppState>, id: String) -> ApiResult {
    if state.downloads.lock().await.contains_key(&id) {
        return Err(err(ApiError::Models(format!(
            "download in flight for model {id}; cancel it before deleting"
        ))));
    }
    let mut cfg = state.config.write().await;
    let pos = cfg
        .models
        .iter()
        .position(|m| m.id == id)
        .ok_or_else(|| err(ApiError::NotFound(format!("model {id}"))))?;
    let model = cfg.models[pos].clone();
    // Delete the file first: on failure the config entry is kept.
    let report = manager_models::delete_model_file(&model, &cfg.backends)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    cfg.models.remove(pos);
    Ok(json!({
        "model_id": id,
        "deleted": true,
        "path": report.path,
        "file_existed": report.file_existed,
        "bytes_freed": report.bytes_freed,
    }))
}

/// Assign a model to a backend with a tensor split (spec §14
/// `model_assign`). Sets the backend's model file, applies the split, and
/// records a restart task for the guided rolling restart.
#[tauri::command]
async fn model_assign(
    state: State<'_, AppState>,
    id: String,
    backend_id: String,
    split: Vec<f32>,
) -> ApiResult {
    let mut cfg = state.config.write().await;
    let model = cfg
        .models
        .iter()
        .find(|m| m.id == id)
        .ok_or_else(|| err(ApiError::NotFound(format!("model {id}"))))?;
    let gguf_path = model.gguf_path.clone();
    {
        let backend = cfg
            .backends
            .iter_mut()
            .find(|b| b.id == backend_id)
            .ok_or_else(|| err(ApiError::NotFound(format!("backend {backend_id}"))))?;
        backend.model_file = gguf_path;
        manager_models::apply_placement(backend, &split)
            .map_err(ApiError::from)
            .map_err(jerr)?;
    }
    if let Some(model) = cfg.models.iter_mut().find(|m| m.id == id) {
        if !model.assigned_backends.contains(&backend_id) {
            model.assigned_backends.push(backend_id.clone());
        }
    }
    manager_config::validate(&*cfg)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    let task_id = state
        .record_task(
            "model_assign",
            json!({ "model_id": id, "backend_id": backend_id, "split": split }),
        )
        .await;
    Ok(json!({ "task_id": task_id, "persisted": false }))
}

// ---------------------------------------------------------------------------
// Telemetry: additional §14 commands
// ---------------------------------------------------------------------------

/// Set the telemetry poll interval (in-memory until save lands).
#[tauri::command]
async fn telemetry_set_interval(state: State<'_, AppState>, ms: u64) -> ApiResult {
    state.config.write().await.telemetry.poll_interval_ms = ms;
    Ok(json!({ "ok": true, "poll_interval_ms": ms }))
}

/// Reset peak trackers (spec §5: peak marks are resettable).
#[tauri::command]
async fn telemetry_reset_peaks(state: State<'_, AppState>) -> ApiResult {
    state.telemetry().await.lock().await.reset_peaks();
    Ok(json!({ "ok": true }))
}

/// Write the current snapshot to the data dir.
#[tauri::command]
async fn telemetry_save_snapshot(state: State<'_, AppState>) -> ApiResult {
    let samples: Vec<GpuSample> = state.telemetry().await.lock().await.snapshot();
    let path = state
        .data_dir
        .join(format!("telemetry-snapshot-{}.json", now_ms()));
    std::fs::write(&path, serde_json::to_string_pretty(&samples).map_err(jerr)?)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    Ok(json!({ "path": path }))
}

/// Project a tensor split onto discovered VRAM (spec §14
/// `placement_propose`). Free VRAM comes from the latest telemetry
/// snapshot; stale GPUs count their last-known values (the projection is
/// labeled with the snapshot age by the UI).
#[tauri::command]
async fn placement_propose(
    state: State<'_, AppState>,
    model_id: String,
    split: Vec<f32>,
) -> ApiResult {
    let spec = manager_models::curated_models()
        .into_iter()
        .find(|s| s.id == model_id)
        .ok_or_else(|| {
            err(ApiError::NotFound(format!(
                "no catalog entry for model {model_id}"
            )))
        })?;
    let mut samples = state.telemetry().await.lock().await.snapshot();
    samples.sort_by_key(|s| s.index);
    if samples.is_empty() {
        return Err(err(ApiError::Telemetry(
            "no telemetry yet; cannot project placement".to_string(),
        )));
    }
    let free_mib: Vec<u64> = samples
        .iter()
        .map(|s| s.vram_total_mib.saturating_sub(s.vram_used_mib))
        .collect();
    let projection = manager_models::propose_placement(&spec, &split, &free_mib)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    Ok(serde_json::to_value(&projection).map_err(jerr)?)
}

/// Apply a tensor split to a backend (spec §14 `placement_apply`).
/// Validates the split, writes it to the backend config in-memory, and
/// records a rolling-restart task for the UI to dispatch via
/// `restart_backend`.
#[tauri::command]
async fn placement_apply(
    state: State<'_, AppState>,
    model_id: String,
    split: Vec<f32>,
) -> ApiResult {
    // placement_apply targets the backends serving the model: it applies the
    // split to every backend the model is assigned to (assign via
    // model_assign first).
    let cfg = state.config.read().await;
    let backend_ids: Vec<String> = cfg
        .models
        .iter()
        .find(|m| m.id == model_id)
        .map(|m| m.assigned_backends.clone())
        .unwrap_or_default();
    drop(cfg);
    if backend_ids.is_empty() {
        return Err(err(ApiError::NotFound(format!(
            "model {model_id} is not assigned to any backend; use model_assign first"
        ))));
    }
    let mut cfg = state.config.write().await;
    for bid in &backend_ids {
        let backend = cfg
            .backends
            .iter_mut()
            .find(|b| &b.id == bid)
            .ok_or_else(|| err(ApiError::NotFound(format!("backend {bid}"))))?;
        manager_models::apply_placement(backend, &split)
            .map_err(ApiError::from)
            .map_err(jerr)?;
    }
    manager_config::validate(&*cfg)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    let task_id = state
        .record_task(
            "placement_apply",
            json!({ "model_id": model_id, "backends": backend_ids, "split": split }),
        )
        .await;
    Ok(json!({ "task_id": task_id, "backends": backend_ids, "persisted": false }))
}

// ---------------------------------------------------------------------------
// Tasks / diagnostics
// ---------------------------------------------------------------------------

/// Best-effort abort of a tracked task.
#[tauri::command]
async fn task_abort(state: State<'_, AppState>, task_id: String) -> ApiResult {
    let mut tasks = state.tasks.lock().await;
    match tasks.get_mut(&task_id) {
        Some(rec) => {
            rec.status = TaskStatus::Aborted;
            Ok(json!({ "ok": true, "task_id": task_id }))
        }
        None => Err(err(ApiError::NotFound(format!("task {task_id}")))),
    }
}

/// All tracked tasks.
#[tauri::command]
async fn tasks_list(state: State<'_, AppState>) -> ApiResult {
    let tasks = state.tasks.lock().await;
    let list: Vec<&TaskRecord> = tasks.values().collect();
    Ok(serde_json::to_value(list).map_err(jerr)?)
}

/// Export a diagnostics bundle (spec §14 `diagnostics_export_bundle`):
/// zip of config snapshot + per-scope recent logs + telemetry snapshot +
/// versions file. Returns the bundle path.
#[tauri::command]
async fn diagnostics_export_bundle(state: State<'_, AppState>) -> ApiResult {
    let cfg = state.config.read().await.clone();
    let samples: Vec<GpuSample> = state.telemetry().await.lock().await.snapshot();
    let telemetry_json = serde_json::to_string_pretty(&samples).map_err(jerr)?;
    let scopes: Vec<String> = {
        let sup = state.supervisor().await.lock().await;
        let mut ids = sup.spec_ids();
        ids.sort();
        ids
    };
    let mut log_scopes: Vec<(String, Vec<String>)> = Vec::new();
    for scope in scopes {
        let records = state
            .supervisor()
            .await
            .lock()
            .await
            .logs_query(&scope, None, None, 5000);
        let lines: Vec<String> = records
            .iter()
            .rev()
            .map(|r| {
                format!(
                    "[{}] [{:?}] {}",
                    r.ts.format("%Y-%m-%dT%H:%M:%S%.3fZ"),
                    r.stream,
                    r.line
                )
            })
            .collect();
        log_scopes.push((scope, lines));
    }
    let versions = vec![
        ("app".to_string(), "0.1.0".to_string()),
        (
            "config_schema".to_string(),
            manager_config::CURRENT_VERSION.to_string(),
        ),
    ];
    let dest_dir = state.data_dir.join("diagnostics");
    let path = manager_config::export_diagnostics(
        &cfg,
        &log_scopes,
        &telemetry_json,
        &versions,
        &dest_dir,
    )
    .map_err(ApiError::from)
    .map_err(jerr)?;
    Ok(json!({ "path": path }))
}

// ---------------------------------------------------------------------------
// MCP / VS Code (§14)
// ---------------------------------------------------------------------------

/// MCP server list with live status (spec §14 `mcp_list`). Includes
/// credential presence per env key (Windows Credential Manager lookup;
/// values never read).
#[tauri::command]
async fn mcp_list(state: State<'_, AppState>) -> ApiResult {
    let servers = state.mcp_servers().await;
    let statuses = {
        let sup = state.supervisor().await.lock().await;
        manager_mcp::list_status(&*sup, &servers)
    };
    // Presence checks are blocking cmdkey calls; run off the async worker.
    let presence: Vec<Vec<(String, bool)>> = tokio::task::spawn_blocking(move || {
        servers
            .iter()
            .map(|s| manager_mcp::credential_presence(s, cmdkey_target_exists))
            .collect()
    })
    .await
    .map_err(|e| err(ApiError::Mcp(format!("presence check failed: {e}"))))?;
    let mut out = serde_json::to_value(&statuses).map_err(jerr)?;
    if let Value::Array(arr) = &mut out {
        for (entry, pres) in arr.iter_mut().zip(presence.iter()) {
            entry["credential_presence"] = serde_json::to_value(pres).unwrap_or(Value::Null);
        }
    }
    Ok(out)
}

/// Check whether a Windows Credential Manager target exists.
/// Presence only — the value is never read. Off Windows, always false.
fn cmdkey_target_exists(target: &str) -> bool {
    #[cfg(windows)]
    {
        std::process::Command::new("cmdkey")
            .arg(format!("/list:{target}"))
            .output()
            .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains(target))
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        let _ = target;
        false
    }
}

/// Connection test for one MCP server (spec §14 `mcp_test_connection`).
/// Local: supervised process running. Remote (http): HTTP probe of the URL.
/// Disabled servers report not-ok.
#[tauri::command]
async fn mcp_test_connection(state: State<'_, AppState>, id: String) -> ApiResult {
    let servers = state.mcp_servers().await;
    let server = servers
        .iter()
        .find(|s| s.id == id)
        .ok_or_else(|| err(ApiError::NotFound(format!("mcp server {id}"))))?;
    match server.mode {
        manager_mcp::McpMode::Disabled => {
            Ok(json!({ "id": id, "ok": false, "detail": "server disabled" }))
        }
        manager_mcp::McpMode::Local => {
            let running = {
                let sup = state.supervisor().await.lock().await;
                manager_mcp::list_status(&*sup, std::slice::from_ref(server))
                    .into_iter()
                    .next()
                    .map(|s| s.running)
                    .unwrap_or(false)
            };
            Ok(json!({
                "id": id,
                "ok": running,
                "detail": if running { "supervised process running" } else { "supervised process not running" },
            }))
        }
        manager_mcp::McpMode::Remote => {
            let url = server.url.clone().ok_or_else(|| {
                err(ApiError::Mcp(format!(
                    "remote server {id} has no url configured"
                )))
            })?;
            let probe = HttpHealthProbe::new(url, Duration::from_secs(5));
            match probe.probe().await {
                Ok(manager_supervisor::HealthStatus::Healthy) => {
                    Ok(json!({ "id": id, "ok": true, "detail": "http probe healthy" }))
                }
                Ok(other) => Ok(json!({ "id": id, "ok": false, "detail": format!("{other:?}") })),
                Err(e) => Ok(json!({ "id": id, "ok": false, "detail": e.to_string() })),
            }
        }
    }
}

/// Set an MCP server's mode (spec §14 `mcp_set_mode`). Mode is stored
/// in-memory (no MCP section in `AppConfig` yet). Switching to Local
/// spawns the supervised process; switching away stops it.
#[tauri::command]
async fn mcp_set_mode(state: State<'_, AppState>, id: String, mode: String) -> ApiResult {
    let mode = match mode.as_str() {
        "local" => manager_mcp::McpMode::Local,
        "remote" => manager_mcp::McpMode::Remote,
        "disabled" => manager_mcp::McpMode::Disabled,
        other => {
            return Err(err(ApiError::Mcp(format!(
                "unknown mcp mode {other:?}; expected local | remote | disabled"
            ))))
        }
    };
    {
        let known = manager_mcp::default_servers();
        if !known.iter().any(|s| s.id == id) {
            return Err(err(ApiError::NotFound(format!("mcp server {id}"))));
        }
        state.mcp_modes.lock().await.insert(id.clone(), mode);
    }
    let servers = state.mcp_servers().await;
    let server = servers
        .iter()
        .find(|s| s.id == id)
        .expect("mode was just set for a known server");
    let mut sup = state.supervisor().await.lock().await;
    match mode {
        manager_mcp::McpMode::Local => {
            manager_mcp::spawn_local(&mut *sup, server)
                .await
                .map_err(ApiError::from)
                .map_err(jerr)?;
        }
        _ => {
            manager_mcp::stop_local(&mut *sup, server)
                .await
                .map_err(ApiError::from)
                .map_err(jerr)?;
        }
    }
    Ok(json!({ "id": id, "mode": mode }))
}

/// Install Continue.dev + Cline via the wizard's `vscode-extensions` step
/// (spec §9, §14 `vscode_install_extensions`). Runs `code
/// --install-extension` for the pinned extensions through the real system
/// ops; returns the step outcome.
#[tauri::command]
async fn vscode_install_extensions(state: State<'_, AppState>) -> ApiResult {
    let task_id = state
        .record_task("vscode_install_extensions", json!({}))
        .await;
    let cfg = state.config.read().await.clone();
    let data_dir = state.data_dir.clone();
    let ctx = WizardContext::new(cfg, data_dir, RunMode::Install, Arc::new(RealSystemOps));
    let mut wizard = Wizard::new(
        vec![Box::new(VscodeExtensions::default())],
        RunMode::Install,
    )
    .map_err(jerr)?;
    let report = wizard.run_all(&ctx).await;
    let (done, total) = report.progress();
    state
        .finish_task(
            &task_id,
            TaskStatus::Done,
            json!({ "done": done, "total": total }),
        )
        .await;
    Ok(serde_json::to_value(&report).map_err(jerr)?)
}

/// User's home directory (`USERPROFILE` on Windows, `HOME` elsewhere).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// Write the Continue config pointing at the local gateway (spec §9, §14
/// `vscode_write_config`). When `backup` is true, an existing
/// `~/.continue/config.json` is first renamed to a timestamped backup;
/// when false, the config is only staged under the data dir and the
/// canonical file is left untouched. Returns old/new model lists so the UI
/// can show the diff before apply.
#[tauri::command]
async fn vscode_write_config(state: State<'_, AppState>, backup: bool) -> ApiResult {
    let cfg = state.config.read().await;
    let new_json = continue_editor_config(&cfg);
    let new_models: Value = serde_json::from_str::<Value>(&new_json)
        .map_err(jerr)?
        .get("models")
        .cloned()
        .unwrap_or(Value::Null);
    drop(cfg);

    let staged = state.data_dir.join("vscode").join("continue-config.json");
    if let Some(parent) = staged.parent() {
        std::fs::create_dir_all(parent)
            .map_err(ApiError::from)
            .map_err(jerr)?;
    }
    std::fs::write(&staged, new_json.as_bytes())
        .map_err(ApiError::from)
        .map_err(jerr)?;

    let mut out = json!({
        "staged": staged,
        "canonical": Value::Null,
        "backup_path": Value::Null,
        "old_models": Value::Null,
        "new_models": new_models,
    });
    if !backup {
        return Ok(out);
    }
    let Some(home) = home_dir() else {
        return Ok(out);
    };
    let canonical = home.join(".continue").join("config.json");
    let mut old_models = Value::Null;
    let mut backup_path = Value::Null;
    if canonical.exists() {
        let existing = std::fs::read_to_string(&canonical)
            .map_err(ApiError::from)
            .map_err(jerr)?;
        old_models = serde_json::from_str::<Value>(&existing)
            .ok()
            .and_then(|v| v.get("models").cloned())
            .unwrap_or(Value::Null);
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let bak = home
            .join(".continue")
            .join(format!("config.json.bak-{ts_ms}"));
        std::fs::rename(&canonical, &bak)
            .map_err(ApiError::from)
            .map_err(jerr)?;
        backup_path = serde_json::to_value(&bak).map_err(jerr)?;
    }
    if let Some(parent) = canonical.parent() {
        std::fs::create_dir_all(parent)
            .map_err(ApiError::from)
            .map_err(jerr)?;
    }
    std::fs::write(&canonical, new_json.as_bytes())
        .map_err(ApiError::from)
        .map_err(jerr)?;
    out["canonical"] = serde_json::to_value(&canonical).map_err(jerr)?;
    out["backup_path"] = backup_path;
    out["old_models"] = old_models;
    Ok(out)
}

// ---------------------------------------------------------------------------
// MXC: additional §14 commands
// ---------------------------------------------------------------------------

/// Set the MXC enforcement mode (in-memory until save lands).
#[tauri::command]
async fn mxc_set_mode(state: State<'_, AppState>, mode: MxcMode) -> ApiResult {
    state.config.write().await.mxc.mode = mode;
    Ok(json!({ "ok": true, "mode": mode }))
}

/// Run the MXC policy self-test against the current policy. Harmless by
/// construction: probes are pure policy evaluations, nothing is executed
/// and no network traffic is sent.
#[tauri::command]
async fn mxc_run_self_test(state: State<'_, AppState>) -> ApiResult {
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
#[tauri::command]
async fn mxc_get_activity_report(state: State<'_, AppState>, since_ms: Option<u64>) -> ApiResult {
    let path = state.data_dir.join("mxc-activity-report.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({ "entries": [], "source": "none" }));
        }
        Err(e) => return Err(jerr(ApiError::from(e))),
    };
    let report = manager_mxc::parse_learning_report(&text)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    let report = match since_ms {
        Some(since) => manager_mxc::filter_since(&report, since),
        None => report,
    };
    Ok(json!({ "entries": report.entries, "source": path }))
}

/// Propose policy tightenings from a learning report (JSON) against the
/// current policy, via `manager_mxc::suggest_rules`.
#[tauri::command]
async fn mxc_propose_tightening(state: State<'_, AppState>, report: Value) -> ApiResult {
    let cfg = state.config.read().await;
    let policy = current_mxc_policy(&cfg)?;
    let text = serde_json::to_string(&report).map_err(jerr)?;
    let parsed = manager_mxc::parse_learning_report(&text)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    let suggestions = manager_mxc::suggest_rules(&parsed, &policy);
    Ok(json!({ "suggestions": suggestions }))
}

// ---------------------------------------------------------------------------
// Windows ML (§14)
// ---------------------------------------------------------------------------

/// Probe WinMLServer availability; limits recorded verbatim.
#[tauri::command]
async fn winml_probe(state: State<'_, AppState>) -> ApiResult {
    let cfg = state.config.read().await;
    let port = if cfg.winml.port == 0 {
        manager_winml::DEFAULT_PORT
    } else {
        cfg.winml.port
    };
    let transport = manager_winml::ReqwestProbeTransport::new(Duration::from_secs(10))
        .map_err(|e| format!("winml transport setup failed: {e:?}"))?;
    let report = manager_winml::probe(&transport, &manager_winml::base_url(port))
        .await
        .map_err(ApiError::from)
        .map_err(jerr)?;
    Ok(serde_json::to_value(report).map_err(jerr)?)
}

/// Register a model with the Windows ML secondary backend.
#[tauri::command]
async fn winml_register(state: State<'_, AppState>, model_id: String) -> ApiResult {
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
        .map_err(ApiError::from)
        .map_err(jerr)?;
    let value = serde_json::to_value(reg).map_err(jerr)?;
    drop(guard);
    Ok(value)
}

/// Unregister the Windows ML backend.
#[tauri::command]
async fn winml_unregister(state: State<'_, AppState>) -> ApiResult {
    let registry = state.winml().await?;
    let removed = registry.lock().await.unregister();
    Ok(json!({ "ok": true, "removed": removed.is_some() }))
}

/// Registration status plus the researched limits (implemented, no stub).
#[tauri::command]
async fn winml_status(state: State<'_, AppState>) -> ApiResult {
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

// ---------------------------------------------------------------------------
// Updates (§14)
// ---------------------------------------------------------------------------

/// Check all components for updates. Explicitly deferred: spec §16 puts
/// Updates in P6 (post-v1); per-component pin/auto-update toggles work now.
#[tauri::command]
async fn updates_check_all(state: State<'_, AppState>) -> ApiResult {
    let _ = state;
    Err(err(ApiError::Unsupported(
        "Update checking ships in P6; per-component pin/auto-update toggles work now".to_string(),
    )))
}

/// Apply an update. Explicitly deferred: spec §16 puts Updates in P6
/// (post-v1); per-component pin/auto-update toggles work now.
#[tauri::command]
async fn updates_apply(_state: State<'_, AppState>, component_id: String) -> ApiResult {
    let _ = component_id;
    Err(err(ApiError::Unsupported(
        "Update checking ships in P6; per-component pin/auto-update toggles work now".to_string(),
    )))
}

/// Pin a component to a version (in-memory until save lands).
#[tauri::command]
async fn updates_pin(
    state: State<'_, AppState>,
    component_id: String,
    version: Option<String>,
) -> ApiResult {
    let mut cfg = state.config.write().await;
    cfg.updates
        .components
        .entry(component_id.clone())
        .and_modify(|c| c.pinned_version = version.clone())
        .or_insert(ComponentUpdate {
            pinned_version: version.clone(),
            auto_update: true,
            last_checked: None,
        });
    Ok(json!({ "ok": true, "component_id": component_id, "pinned_version": version }))
}

/// Toggle auto-update for a component (in-memory until save lands).
#[tauri::command]
async fn updates_set_auto(
    state: State<'_, AppState>,
    component_id: String,
    enabled: bool,
) -> ApiResult {
    let mut cfg = state.config.write().await;
    cfg.updates
        .components
        .entry(component_id.clone())
        .and_modify(|c| c.auto_update = enabled)
        .or_insert(ComponentUpdate {
            pinned_version: None,
            auto_update: enabled,
            last_checked: None,
        });
    Ok(json!({ "ok": true, "component_id": component_id, "auto_update": enabled }))
}

// ---------------------------------------------------------------------------
// Entrypoint
// ---------------------------------------------------------------------------

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            // Load the persisted config; `manager_config::load` falls back
            // to the `.bak` last-known-good copy on parse/validation
            // failure (spec §10). First run (or total corruption) falls
            // back to defaults with a stderr banner.
            let data_dir = manager_config::default_data_dir();
            let config_path = data_dir.join("config.json");
            let config = match manager_config::load(&config_path) {
                Ok(cfg) => cfg,
                Err(e) => {
                    eprintln!(
                        "config load failed ({e}); using defaults: {}",
                        config_path.display()
                    );
                    manager_config::AppConfig::default_config()
                }
            };
            app.manage(AppState::new(config, config_path, data_dir));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // Status / config
            get_status,
            get_config,
            update_config,
            // Backends
            list_backends,
            start_backend,
            stop_backend,
            restart_backend,
            test_backend,
            backend_get_config,
            backend_preview_diff,
            backend_apply_config,
            backend_get_launch_command,
            // Wizard / audit
            run_wizard_step,
            run_audit,
            setup_get_prerequisites,
            setup_retry_step,
            setup_skip_step,
            setup_get_commands,
            setup_mark_manual,
            setup_get_steps,
            setup_status,
            setup_mark_complete,
            setup_step_help_url,
            open_url,
            // Models
            list_models,
            download_model,
            cancel_download,
            model_pause_download,
            model_resume_download,
            model_verify_checksum,
            model_delete,
            model_assign,
            // Logs
            get_logs,
            // Gateway
            get_gateway_config,
            update_gateway_config,
            gateway_status,
            gateway_preview_yaml,
            gateway_test_routing,
            gateway_recent_requests,
            // Telemetry
            get_gpu_telemetry,
            telemetry_set_interval,
            telemetry_reset_peaks,
            telemetry_save_snapshot,
            placement_propose,
            placement_apply,
            // Tasks / diagnostics
            task_abort,
            tasks_list,
            diagnostics_export_bundle,
            // MCP / VS Code
            mcp_list,
            mcp_test_connection,
            mcp_set_mode,
            vscode_install_extensions,
            vscode_write_config,
            // MXC
            get_mxc_policy,
            update_mxc_policy,
            mxc_set_mode,
            mxc_run_self_test,
            mxc_get_activity_report,
            mxc_propose_tightening,
            // Windows ML
            winml_probe,
            winml_register,
            winml_unregister,
            winml_status,
            // Updates
            updates_check_all,
            updates_apply,
            updates_pin,
            updates_set_auto,
        ])
        .run(tauri::generate_context!())
        .expect("failed to run Local LLM Service Manager");
}
