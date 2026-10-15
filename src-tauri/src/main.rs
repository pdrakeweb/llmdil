//! Tauri 2 command/event API for the Local LLM Service Manager (spec §14).
//!
//! The webview never spawns processes, touches the filesystem, or holds
//! secrets. All privileged operations are Tauri commands executed here,
//! delegating to the `manager-*` crates.
//!
//! Wiring status: all §14 commands are wired to real crate functions.
//! Known gap, documented at the command: model fit-check
//! (`fit_check_available` is always false in v0.1.x).
//!
//! LOAD-BEARING CONFIG: `app.withGlobalTauri` in tauri.conf.json must stay
//! true. The UI detects Tauri via `window.__TAURI__.core.invoke`, and that
//! global is injected only with the flag on. Removing it silently degrades
//! every view to sample data (the v0.1.1 bug).
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
use manager_supervisor::{
    test_request, BackoffPolicy, HealthProbe, HttpHealthProbe, ProcessSpec, ProcessState,
    Supervisor,
};
use manager_telemetry::{
    FallbackSource, GpuSample, NvidiaSmiSource, NvmlSource, PollCadence, Poller, TelemetryStore,
};
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

/// Frontend event names. The primary streams are emitted below;
/// BACKEND_STATUS is emitted by the backend start/stop/restart commands;
/// the rest are reserved per spec §14 and go live with their producers.
pub mod events {
    pub const TELEMETRY_UPDATE: &str = "telemetry-update";
    pub const WIZARD_PROGRESS: &str = "wizard-progress";
    pub const DOWNLOAD_PROGRESS: &str = "download-progress";
    // Emitted by the backend start/stop/restart commands.
    pub const BACKEND_STATUS: &str = "backend-status";
    // Reserved (spec §14): emitted by future producers. LOG_LINE is
    // declared (with its LogLinePayload shape) but nothing emits it yet;
    // log records reach the UI through the get_logs command instead.
    pub const LOG_LINE: &str = "log-line";
    pub const TELEMETRY_TICK: &str = "telemetry-tick";
    pub const TASK_PROGRESS: &str = "task-progress";
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

// Conversions format with Display (`{e}`), not Debug: these strings
// reach the UI, and every crate error type derives thiserror::Error
// with operator-readable messages (e.g. SupervisorError::PortInUse
// reads "port 8081 is already in use", not "PortInUse(8081)").
impl From<manager_config::ConfigError> for ApiError {
    fn from(e: manager_config::ConfigError) -> Self {
        ApiError::Config(format!("{e}"))
    }
}
impl From<manager_supervisor::SupervisorError> for ApiError {
    fn from(e: manager_supervisor::SupervisorError) -> Self {
        ApiError::Supervisor(format!("{e}"))
    }
}
impl From<manager_wizard::WizardError> for ApiError {
    fn from(e: manager_wizard::WizardError) -> Self {
        ApiError::Wizard(format!("{e}"))
    }
}
impl From<manager_telemetry::TelemetryError> for ApiError {
    fn from(e: manager_telemetry::TelemetryError) -> Self {
        ApiError::Telemetry(format!("{e}"))
    }
}
impl From<manager_gateway::GatewayError> for ApiError {
    fn from(e: manager_gateway::GatewayError) -> Self {
        ApiError::Gateway(format!("{e}"))
    }
}
impl From<manager_models::ModelError> for ApiError {
    fn from(e: manager_models::ModelError) -> Self {
        ApiError::Models(format!("{e}"))
    }
}
impl From<manager_mxc::MxcError> for ApiError {
    fn from(e: manager_mxc::MxcError) -> Self {
        ApiError::Mxc(format!("{e}"))
    }
}
impl From<manager_winml::WinMlError> for ApiError {
    fn from(e: manager_winml::WinMlError) -> Self {
        ApiError::WinMl(format!("{e}"))
    }
}
impl From<manager_mcp::McpError> for ApiError {
    fn from(e: manager_mcp::McpError) -> Self {
        ApiError::Mcp(format!("{e}"))
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

/// Terminal outcome of a model download, recorded by the completion
/// watcher spawned in `download_model`.
#[derive(Debug, Clone)]
struct DownloadOutcome {
    /// Token of the `download_model` attempt that produced this
    /// outcome (0 for outcomes recorded outside a download, i.e. a
    /// manual `model_verify_checksum` mismatch). The watcher writes
    /// only when its token is still the model's latest attempt, so a
    /// superseded download cannot overwrite a newer attempt's outcome.
    attempt: u64,
    /// The failure message when the download — or the hash verification
    /// that follows it — failed; `None` when it completed.
    failed: Option<String>,
    /// True when the failure is a SHA-256 verification failure
    /// (post-download watcher or manual verify). Only verification
    /// failures gate backend start in the preflight; transfer failures
    /// set this false and do not gate.
    verification_failure: bool,
}

/// Shared backend state. Subsystems are lazily initialized (`OnceCell`) so
/// a failure surfaces on first use of that subsystem, never at startup.
pub struct AppState {
    config: RwLock<AppConfig>,
    config_path: PathBuf,
    data_dir: PathBuf,
    supervisor: OnceCell<Mutex<Supervisor>>,
    /// The telemetry store is owned by the poll loop (single writer); the
    /// Arc is shared with the Poller built in `telemetry()`. The std Mutex
    /// is the store's own lock type from manager-telemetry.
    telemetry: OnceCell<Arc<std::sync::Mutex<TelemetryStore>>>,
    /// Keeps the telemetry poll loop alive for the app's lifetime.
    telemetry_poller: OnceCell<std::sync::Mutex<manager_telemetry::store::PollHandle>>,
    wizard_install: OnceCell<Mutex<Wizard>>,
    wizard_audit: OnceCell<Mutex<Wizard>>,
    tasks: Mutex<HashMap<String, TaskRecord>>,
    task_counter: AtomicU64,
    /// Live model-download handles by model id (spec §14 download commands).
    downloads: Mutex<HashMap<String, DownloadHandle>>,
    /// Terminal download outcomes by model id, recorded by the
    /// completion watcher spawned in `download_model`: `failed` is the
    /// failure message, or `None` when the download completed (verified
    /// against the pinned hash when one exists). Read by `list_models`
    /// to surface `download_error` and by the backend start preflight
    /// to gate on verification failures.
    download_outcomes: Mutex<HashMap<String, DownloadOutcome>>,
    /// Monotonic download-attempt counter; each `download_model` call
    /// takes the next token (see [`DownloadOutcome::attempt`]).
    download_attempt_counter: AtomicU64,
    /// Latest download-attempt token per model id. The completion
    /// watcher writes its outcome only when its token still matches.
    download_latest_attempt: Mutex<HashMap<String, u64>>,
    /// Model ids whose post-download SHA-256 verification is currently
    /// running in the completion watcher. Backend start is refused for
    /// these: the file may be seconds from being deleted as corrupt.
    /// std Mutex like `backend_spec_ids` — only synchronous membership
    /// checks and inserts are needed.
    verifying_downloads: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Ids of the supervisor specs this layer registered as backends.
    /// The Supervisor is shared with the MCP subsystem (process ids
    /// `mcp-<id>`); pruning and orphan-surfacing in the backend commands
    /// must only ever touch ids in this set.
    backend_spec_ids: std::sync::Mutex<std::collections::HashSet<String>>,
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
            telemetry_poller: OnceCell::new(),
            wizard_install: OnceCell::new(),
            wizard_audit: OnceCell::new(),
            tasks: Mutex::new(HashMap::new()),
            task_counter: AtomicU64::new(1),
            downloads: Mutex::new(HashMap::new()),
            download_outcomes: Mutex::new(HashMap::new()),
            download_attempt_counter: AtomicU64::new(1),
            download_latest_attempt: Mutex::new(HashMap::new()),
            verifying_downloads: std::sync::Mutex::new(std::collections::HashSet::new()),
            backend_spec_ids: std::sync::Mutex::new(std::collections::HashSet::new()),
            mcp_modes: Mutex::new(HashMap::new()),
            routing_log: Mutex::new(manager_gateway::RoutingLog::default()),
            winml: OnceCell::new(),
        }
    }

    fn next_task_id(&self) -> String {
        let n = self.task_counter.fetch_add(1, Ordering::Relaxed);
        format!("task-{n:06}")
    }

    /// Take the next download-attempt token and record it as the
    /// model's latest attempt.
    async fn begin_download_attempt(&self, model_id: &str) -> u64 {
        let attempt = self
            .download_attempt_counter
            .fetch_add(1, Ordering::Relaxed);
        self.download_latest_attempt
            .lock()
            .await
            .insert(model_id.to_string(), attempt);
        attempt
    }

    /// Record a download outcome only if its attempt token is still
    /// the model's latest; a superseded watcher's outcome is dropped.
    async fn record_download_outcome(&self, model_id: &str, outcome: DownloadOutcome) {
        let latest = self
            .download_latest_attempt
            .lock()
            .await
            .get(model_id)
            .copied();
        if latest == Some(outcome.attempt) {
            self.download_outcomes
                .lock()
                .await
                .insert(model_id.to_string(), outcome);
        }
    }

    /// Clear a download outcome (e.g. cancellation) under the same
    /// latest-attempt gate as [`Self::record_download_outcome`].
    async fn clear_download_outcome(&self, model_id: &str, attempt: u64) {
        let latest = self
            .download_latest_attempt
            .lock()
            .await
            .get(model_id)
            .copied();
        if latest == Some(attempt) {
            self.download_outcomes.lock().await.remove(model_id);
        }
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

    /// The shared telemetry store. The first call builds the poll loop
    /// (NVML with an nvidia-smi fallback) at the configured cadence and
    /// starts it; the store the loop writes is the store readers see.
    /// Before this wiring the store was never fed and snapshots were
    /// always empty.
    async fn telemetry(&self) -> Arc<std::sync::Mutex<TelemetryStore>> {
        self.telemetry
            .get_or_init(|| async {
                let interval_ms = self.config.read().await.telemetry.poll_interval_ms;
                let cadence = if interval_ms == 0 {
                    PollCadence::Off
                } else if interval_ms <= 1500 {
                    PollCadence::Every1s
                } else {
                    PollCadence::Every5s
                };
                let source = FallbackSource::new(NvmlSource::new(), NvidiaSmiSource::default());
                let (poller, store) = Poller::new(source, cadence);
                let handle = poller.start();
                let _ = self.telemetry_poller.set(std::sync::Mutex::new(handle));
                store
            })
            .await
            .clone()
    }

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

/// Lock the shared telemetry store, recovering from a poisoned mutex
/// (a panicked poll write must not wedge every later read).
fn lock_telemetry(
    store: &Arc<std::sync::Mutex<TelemetryStore>>,
) -> std::sync::MutexGuard<'_, TelemetryStore> {
    store.lock().unwrap_or_else(|e| e.into_inner())
}

/// WinML registry, built lazily with the real HTTP probe transport.

/// Resolve a config path against the data dir (absolute paths pass
/// through). Config model paths are relative (`models/{id}.gguf`); every
/// filesystem operation must go through this so it agrees with the path
/// list_models probes and download_model writes.
fn resolve_under_data_dir(data_dir: &PathBuf, path: &PathBuf) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        data_dir.join(path)
    }
}

/// Drop download handles that reached a terminal state (done, failed,
/// cancelled). Without this, a finished download sits in the map
/// forever: list_models reports `downloading: true` permanently and the
/// start preflight rejects the backend as "still downloading" until the
/// app restarts. Called on the paths that consult the map.
async fn reap_downloads(state: &AppState) {
    state.downloads.lock().await.retain(|_, h| !h.is_terminal());
}

/// True when `sha` is a real pinned SHA-256 — 64 hex chars — as opposed
/// to the catalog placeholder (`PLACEHOLDER_SHA256`, which contains
/// non-hex characters and is not 64 chars) or an unset value. Matches
/// the check `download_spec_for` has always applied.
fn is_real_pin(sha: &str) -> bool {
    sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Seed the model config from the curated catalog on first run: the
/// Models view and the download path operate on configured models, so
/// without seeding a fresh install would show an empty list even
/// though the catalog exists. Seeding happens at most once per config
/// file, gated on `AppConfig.catalog_seeded`: after seeding, the flag
/// is set and saved, so models the operator later deletes stay deleted
/// across restarts. A config that already holds models but predates
/// the flag gets the flag set in memory only (it persists on the next
/// save) and is never reseeded.
async fn ensure_catalog_models(state: &AppState) {
    let mut cfg = state.config.write().await;
    if cfg.catalog_seeded {
        return;
    }
    if !cfg.models.is_empty() {
        cfg.catalog_seeded = true;
        return;
    }
    cfg.models = manager_models::curated_models()
        .into_iter()
        .map(|spec| {
            let file_name = spec
                .url
                .rsplit('/')
                .next()
                .unwrap_or("model.gguf")
                .to_string();
            manager_config::ModelConfig {
                id: spec.id,
                gguf_path: PathBuf::from("models").join(file_name),
                quant: spec.quant,
                params_b: 0.0,
                sha256: spec.sha256,
                verified_at: None,
                source_url: spec.url,
                assigned_backends: Vec::new(),
            }
        })
        .collect();
    cfg.catalog_seeded = true;
    // Best effort: the seeded entries are live in memory either way; a
    // failed save only means the seeding repeats on the next launch.
    let _ = manager_config::save(&cfg, &state.config_path);
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

/// Full application configuration, plus `config_path`: the file the
/// config was loaded from (and saves go to), for the Settings view.
#[tauri::command]
async fn get_config(state: State<'_, AppState>) -> ApiResult {
    let cfg = state.config.read().await;
    let mut v = serde_json::to_value(&*cfg).map_err(jerr)?;
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
#[tauri::command]
async fn update_config(state: State<'_, AppState>, patch: Value) -> ApiResult {
    let mut cfg = state.config.write().await;
    let mut v = serde_json::to_value(&*cfg).map_err(jerr)?;
    merge_json(&mut v, patch);
    let new_cfg: AppConfig = serde_json::from_value(v).map_err(jerr)?;
    // `save` validates before writing, so a failed save never clobbers
    // the last-known-good file.
    manager_config::save(&new_cfg, &state.config_path)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    *cfg = new_cfg;
    Ok(json!({ "ok": true, "persisted": true }))
}

// ---------------------------------------------------------------------------
// Backends
// ---------------------------------------------------------------------------

/// Build the supervisor process spec for one configured backend: the
/// wizard-installed llama-server binary, the configured model (relative
/// paths resolve under the data dir), port, and structured flags.
fn backend_spec(cfg: &AppConfig, b: &manager_config::BackendConfig) -> ProcessSpec {
    let program = cfg.data_dir.join("llamacpp").join(if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    });
    let model = if b.model_file.is_absolute() {
        b.model_file.clone()
    } else {
        cfg.data_dir.join(&b.model_file)
    };
    let mut args: Vec<String> = vec![
        "-m".into(),
        model.to_string_lossy().into_owned(),
        "--port".into(),
        b.port.to_string(),
        "-c".into(),
        b.flags.n_ctx.to_string(),
        "-b".into(),
        b.flags.n_batch.to_string(),
    ];
    if !b.flags.tensor_split.is_empty() {
        args.push("--tensor-split".into());
        args.push(
            b.flags
                .tensor_split
                .iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    args.push("--split-mode".into());
    args.push(
        match b.flags.split_mode {
            manager_config::SplitMode::None => "none",
            manager_config::SplitMode::Layer => "layer",
            manager_config::SplitMode::Row => "row",
        }
        .into(),
    );
    if let Some(raw) = &b.raw_flags {
        // raw_flags are operator-supplied llama-server arguments, with one
        // reservation: --host is filtered out (both "--host X" and
        // "--host=X"). Backends bind loopback; the LiteLLM gateway is the
        // only network entry point, and an unauthenticated llama-server
        // bound to the LAN by a stray flag would bypass it.
        let tokens: Vec<&str> = raw.split_whitespace().collect();
        let mut i = 0;
        while i < tokens.len() {
            let t = tokens[i];
            if t == "--host" {
                i += 2; // skip the flag and its value
                continue;
            }
            if t.starts_with("--host=") {
                i += 1;
                continue;
            }
            args.push(t.to_string());
            i += 1;
        }
    }
    ProcessSpec {
        id: b.id.clone(),
        program,
        args,
        env: Vec::new(),
        workdir: None,
        port: Some(b.port),
    }
}

/// (Re)register supervisor specs from the current config so start/stop/
/// restart/status always act on what is configured now. Registration only
/// touches the spec table, never live processes: re-registering a running
/// backend replaces the spec its NEXT spawn will use. Specs for ids no
/// longer configured are pruned — except while their process is live
/// (unregister refuses), so a removed-but-running backend stays visible
/// and stoppable until it exits.
async fn ensure_backend_specs(state: &AppState) {
    let cfg = state.config.read().await.clone();
    let mut sup = state.supervisor().await.lock().await;
    for b in &cfg.backends {
        sup.register_spec(backend_spec(&cfg, b));
    }
    let configured: std::collections::HashSet<&str> =
        cfg.backends.iter().map(|b| b.id.as_str()).collect();
    // Record which spec ids belong to backends. The Supervisor is
    // shared with the MCP subsystem, whose specs must never be pruned
    // or surfaced by the backend commands.
    {
        let mut owned = state
            .backend_spec_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for b in &cfg.backends {
            owned.insert(b.id.clone());
        }
        // Prune backend-owned specs whose ids left the config.
        // unregister_spec refuses while a process is live under the id,
        // so a removed-but-running backend keeps its spec (and stays
        // visible + stoppable via list_backends) until it exits.
        let stale: Vec<String> = owned
            .iter()
            .filter(|id| !configured.contains(id.as_str()))
            .cloned()
            .collect();
        for id in stale {
            if sup.unregister_spec(&id) {
                owned.remove(&id);
            }
        }
    }
}

fn process_state_str(s: ProcessState) -> &'static str {
    match s {
        ProcessState::Stopped => "stopped",
        ProcessState::Starting => "starting",
        ProcessState::Running => "running",
        ProcessState::Stopping => "stopping",
        ProcessState::Failed => "failed",
    }
}

/// Gates a start/restart: the backend must be configured and enabled,
/// its model must not be mid-download or mid-verification, must not
/// carry a recorded SHA-256 verification failure, and — when it has a
/// real pinned hash — must have passed verification at least once
/// (`verified_at` set). The llama-server binary and model file must
/// exist. Errors name the missing piece and the fix, so a first Start
/// after a skipped wizard step explains itself.
async fn backend_start_preflight(state: &AppState, id: &str) -> Result<(), String> {
    let cfg = state.config.read().await.clone();
    let b = cfg
        .backends
        .iter()
        .find(|b| b.id == id)
        .ok_or_else(|| err(ApiError::NotFound(format!("backend {id}"))))?;
    if !b.enabled {
        return Err(err(ApiError::InvalidInput(format!(
            "backend {id} is disabled in the configuration; enable it before starting"
        ))));
    }
    reap_downloads(state).await;
    let resolved_model = resolve_under_data_dir(&cfg.data_dir, &b.model_file);
    if let Some(m) = cfg
        .models
        .iter()
        .find(|m| resolve_under_data_dir(&cfg.data_dir, &m.gguf_path) == resolved_model)
    {
        if state.downloads.lock().await.contains_key(&m.id) {
            return Err(err(ApiError::InvalidInput(format!(
                "model {} is still downloading; a backend started now would load a truncated file",
                m.id
            ))));
        }
        if state.verifying_downloads.lock().unwrap().contains(&m.id) {
            return Err(err(ApiError::InvalidInput(format!(
                "SHA-256 verification is still running for model {}.",
                m.id
            ))));
        }
        // A recorded verification failure gates start until the model
        // is re-downloaded or passes a manual verify: the file that
        // came down did not match its pinned hash.
        if let Some(outcome) = state.download_outcomes.lock().await.get(&m.id) {
            if outcome.verification_failure {
                return Err(err(ApiError::InvalidInput(outcome.failed.clone().unwrap_or_else(
                    || {
                        format!(
                            "model {} has not passed SHA-256 verification — open Models and run Verify.",
                            m.id
                        )
                    },
                ))));
            }
        }
        // Durable gate: a model with a real pinned hash that has never
        // passed verification (verified_at unset) must not start even
        // across app restarts, where no in-memory outcome survives.
        // Wizard-downloaded models land here too: the wizard's
        // DownloadModels step verifies the hash in-step but never
        // stamps verified_at, so their first start asks for a
        // Models > Verify pass — the remediation in the message.
        if is_real_pin(&m.sha256) && m.verified_at.is_none() && resolved_model.exists() {
            return Err(err(ApiError::InvalidInput(format!(
                "model {} has not passed SHA-256 verification — open Models and run Verify.",
                m.id
            ))));
        }
    }
    let spec = backend_spec(&cfg, b);
    if !spec.program.exists() {
        return Err(err(ApiError::InvalidInput(format!(
            "llama-server is not installed (missing {}) — run the setup wizard to install llama.cpp",
            spec.program.display()
        ))));
    }
    let model_path = if b.model_file.is_absolute() {
        b.model_file.clone()
    } else {
        cfg.data_dir.join(&b.model_file)
    };
    if !model_path.exists() {
        return Err(err(ApiError::InvalidInput(format!(
            "model file is missing ({}) — download it in the Models view",
            model_path.display()
        ))));
    }
    Ok(())
}

/// All backends with their launch ports, models, and live supervisor
/// runtime state (process state, pid, restarts, uptime). Configured
/// backends carry `configured: true`; the list also appends
/// supervisor-only rows (`configured: false`, `enabled: false`) for
/// backend processes still live under ids that left the config, so they
/// stay visible and stoppable instead of silently holding their port.
/// MCP server processes share the Supervisor but are never listed here.
#[tauri::command]
async fn list_backends(state: State<'_, AppState>) -> ApiResult {
    ensure_backend_specs(&state).await;
    let cfg = state.config.read().await;
    let mut sup = state.supervisor().await.lock().await;
    // Reap children that exited since the last read so a crashed backend
    // reports "failed" instead of "running" forever.
    sup.reap_exited();
    let statuses: HashMap<String, manager_supervisor::ProcessStatus> = sup
        .status()
        .into_iter()
        .map(|s| (s.id.clone(), s))
        .collect();
    let mut backends: Vec<Value> = cfg
        .backends
        .iter()
        .map(|b| {
            let st = statuses.get(&b.id);
            json!({
                "id": b.id,
                "enabled": b.enabled,
                "port": b.port,
                "model_file": b.model_file,
                "configured": true,
                "state": st.map(|s| process_state_str(s.state)).unwrap_or("stopped"),
                "pid": st.and_then(|s| s.pid),
                "restarts": st.map(|s| s.restarts).unwrap_or(0),
                "uptime_secs": st.and_then(|s| s.uptime).map(|u| u.as_secs()),
            })
        })
        .collect();
    // Processes still live under ids that left the config (their specs
    // survive pruning while live): surface them so they stay visible and
    // stoppable instead of silently holding their port.
    let configured: std::collections::HashSet<&str> =
        cfg.backends.iter().map(|b| b.id.as_str()).collect();
    let backend_owned: std::collections::HashSet<String> = state
        .backend_spec_ids
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    for (id, st) in &statuses {
        if configured.contains(id.as_str()) || !backend_owned.contains(id) || st.pid.is_none() {
            continue;
        }
        let spec = sup.spec(id);
        let model_file = spec
            .and_then(|sp| {
                sp.args
                    .iter()
                    .position(|a| a == "-m")
                    .map(|i| sp.args[i + 1].clone())
            })
            .unwrap_or_default();
        // The spec argv carries the data-dir-resolved absolute path;
        // configured rows show the config-relative path. Strip the
        // data-dir prefix so orphan rows display the same form.
        let model_file = match std::path::Path::new(&model_file).strip_prefix(&cfg.data_dir) {
            Ok(rel) => rel.to_string_lossy().into_owned(),
            Err(_) => model_file,
        };
        backends.push(json!({
            "id": id,
            "enabled": false,
            "port": spec.and_then(|sp| sp.port).unwrap_or(0),
            "model_file": model_file,
            "configured": false,
            "state": process_state_str(st.state),
            "pid": st.pid,
            "restarts": st.restarts,
            "uptime_secs": st.uptime.map(|u| u.as_secs()),
        }));
    }
    Ok(json!({ "backends": backends }))
}

/// Start a backend (supervisor spawn). Returns a task id.
#[tauri::command]
async fn start_backend(app: AppHandle, state: State<'_, AppState>, id: String) -> ApiResult {
    ensure_backend_specs(&state).await;
    backend_start_preflight(&state, &id).await?;
    let task_id = state
        .record_task("backend_start", json!({ "backend_id": id }))
        .await;
    let mut sup = state.supervisor().await.lock().await;
    // Reap first: without this, a backend that crashed since the last
    // list_backends poll would read as AlreadyRunning for a corpse.
    sup.reap_exited();
    let outcome = sup.spawn_backend(&id).await;
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
    ensure_backend_specs(&state).await;
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
    ensure_backend_specs(&state).await;
    backend_start_preflight(&state, &id).await?;
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
///
/// The command payload is `{samples, processes, at_ms, has_polled,
/// source_error}`. `at_ms` is when the store last polled successfully
/// (epoch ms; the newest sample's timestamp as a fallback, `null` when
/// no poll has completed yet) — it describes the data, not the moment
/// this command ran. `has_polled` says whether any poll has succeeded.
/// `processes` is the latest snapshot's per-process VRAM attribution
/// (each row carries its GPU index; `backend_id` is filled in here by
/// matching the pid against the supervisor's live backend pids, `null`
/// for processes the app does not supervise). `source_error` is the last
/// poll error's message while polls are failing and `null` when the last
/// poll succeeded — a failing source must not look like a GPU-less
/// machine returning an empty snapshot.
#[tauri::command]
async fn get_gpu_telemetry(app: AppHandle, state: State<'_, AppState>) -> ApiResult {
    let store = state.telemetry().await;
    let (samples, raw_processes, source_error, last_poll_at) = {
        let guard = lock_telemetry(&store);
        (
            guard.snapshot(),
            guard.last_processes().to_vec(),
            guard.last_poll_error().map(|e| e.message.clone()),
            guard.last_successful_poll_at(),
        )
    };
    let payload = TelemetryUpdatePayload {
        samples: samples.clone(),
        at_ms: now_ms(),
    };
    let _ = app.emit(events::TELEMETRY_UPDATE, &payload);
    // pid -> backend id from the supervisor's live status (the same data
    // list_backends reads; reap first so a just-exited backend's pid is
    // not attributed to a recycled process).
    let pid_to_backend: HashMap<u32, String> = {
        let mut sup = state.supervisor().await.lock().await;
        sup.reap_exited();
        sup.status()
            .into_iter()
            .filter_map(|s| s.pid.map(|pid| (pid, s.id)))
            .collect()
    };
    let processes: Vec<Value> = raw_processes
        .iter()
        .map(|p| {
            let mut v = serde_json::to_value(p).map_err(jerr)?;
            v["backend_id"] = json!(pid_to_backend.get(&p.pid));
            Ok(v)
        })
        .collect::<Result<_, String>>()?;
    // The command return enriches each sample with the store's peak
    // temperature mark (the event payload keeps the crate shape).
    let enriched: Vec<Value> = {
        let guard = lock_telemetry(&store);
        samples
            .iter()
            .map(|s| {
                let mut v = serde_json::to_value(s).map_err(jerr)?;
                let peak = guard.peaks(s.index).and_then(|p| p.temp_c).map(|m| m.value);
                v["temp_peak_c"] = json!(peak);
                Ok(v)
            })
            .collect::<Result<_, String>>()?
    };
    // The command's at_ms stamps the DATA: the store's last successful
    // poll time (fallback: the newest sample's own timestamp; null
    // until a poll has completed). The emitted event keeps its
    // emit-time stamp.
    let at_ms: Option<i64> = last_poll_at
        .map(|t| t.timestamp_millis())
        .or_else(|| samples.iter().map(|s| s.ts.timestamp_millis()).max());
    Ok(json!({
        "samples": enriched,
        "processes": processes,
        "at_ms": at_ms,
        "has_polled": last_poll_at.is_some(),
        "source_error": source_error,
    }))
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

/// Configured models, probed against the filesystem: on_disk/size_bytes
/// reflect the completed file at the model's gguf_path right now (the
/// download worker only moves a finished transfer into place, so a
/// partial download never shows as on_disk). `partial_bytes` is the size
/// of the in-progress `.part` sibling when one exists, `downloading`
/// marks a live download handle or a running post-download SHA-256
/// verification, `verifying` marks the verification alone, `paused` a
/// parked download, and `download_error` carries the last terminal
/// failure recorded by the completion watcher. `catalog_size_bytes` is
/// the curated catalog's size estimate for the model, when it has one.
/// `fit_check_available` is always false in v0.1.x: the catalog carries
/// no VRAM-fit model yet.
#[tauri::command]
async fn list_models(state: State<'_, AppState>) -> ApiResult {
    ensure_catalog_models(&state).await;
    reap_downloads(&state).await;
    let cfg = state.config.read().await;
    let downloads = state.downloads.lock().await;
    let outcomes = state.download_outcomes.lock().await;
    let verifying = state
        .verifying_downloads
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let catalog = manager_models::curated_models();
    let catalog_sizes: HashMap<&str, u64> = catalog
        .iter()
        .map(|s| (s.id.as_str(), s.size_bytes))
        .collect();
    let models: Vec<Value> = cfg
        .models
        .iter()
        .map(|m| {
            let path = resolve_under_data_dir(&cfg.data_dir, &m.gguf_path);
            let meta = std::fs::metadata(&path).ok().filter(|md| md.is_file());
            let partial_meta = std::fs::metadata(manager_models::partial_path(&path))
                .ok()
                .filter(|md| md.is_file());
            let is_verifying = verifying.contains(&m.id);
            json!({
                "id": m.id,
                "gguf_path": m.gguf_path,
                "quant": m.quant,
                "params_b": m.params_b,
                "sha256": m.sha256,
                "verified_at": m.verified_at,
                "fit_check_available": false,
                "on_disk": meta.is_some(),
                "size_bytes": meta.map(|md| md.len()),
                "partial_bytes": partial_meta.map(|md| md.len()),
                "downloading": downloads.contains_key(&m.id) || is_verifying,
                "verifying": is_verifying,
                "paused": downloads.get(&m.id).map(|h| h.is_paused()).unwrap_or(false),
                "download_error": outcomes.get(&m.id).and_then(|o| o.failed.clone()),
                "catalog_size_bytes": catalog_sizes.get(m.id.as_str()).copied(),
            })
        })
        .collect();
    Ok(json!({ "models": models }))
}

/// Build a download spec for a configured model: the curated catalog
/// provides URL/size; the config's sha256 wins when it is a real pinned
/// hash (64 hex chars), otherwise the catalog value stands. A model
/// with no catalog entry falls back to its own configured `source_url`
/// (which must be an https URL), with the placeholder hash unless the
/// config pins a real one — verification then fails closed, exactly as
/// for unpinned catalog models.
fn download_spec_for(cfg: &AppConfig, id: &str) -> Result<manager_models::ModelSpec, String> {
    let m = cfg
        .models
        .iter()
        .find(|m| m.id == id)
        .ok_or_else(|| err(ApiError::NotFound(format!("model {id}"))))?;
    if let Some(mut spec) = manager_models::curated_models()
        .into_iter()
        .find(|s| s.id == id)
    {
        if is_real_pin(&m.sha256) {
            spec.sha256 = m.sha256.clone();
        }
        return Ok(spec);
    }
    if m.source_url.starts_with("https://") {
        return Ok(manager_models::ModelSpec {
            id: m.id.clone(),
            name: m.id.clone(),
            quant: m.quant.clone(),
            size_bytes: 0,
            sha256: if is_real_pin(&m.sha256) {
                m.sha256.clone()
            } else {
                manager_models::PLACEHOLDER_SHA256.to_string()
            },
            url: m.source_url.clone(),
        });
    }
    Err(err(ApiError::NotFound(format!(
        "no catalog entry for model {id} and no https source_url configured"
    ))))
}

/// Start a model download; progress streams via `download-progress`.
/// A completion watcher records the terminal outcome in
/// `download_outcomes` (surfaced by `list_models` as `download_error`)
/// and, when the model has a real pinned SHA-256, verifies the finished
/// file and stamps `verified_at` on success. The model is marked
/// verifying before the watcher spawns and unmarked on every watcher
/// exit path; each attempt carries a token so a superseded watcher
/// cannot overwrite a newer attempt's outcome.
#[tauri::command]
async fn download_model(app: AppHandle, state: State<'_, AppState>, id: String) -> ApiResult {
    ensure_catalog_models(&state).await;
    let (spec, dest) = {
        let cfg = state.config.read().await;
        let spec = download_spec_for(&cfg, &id)?;
        // Download to the model's configured gguf_path (resolved under
        // the data dir) — the same path list_models probes — so a
        // completed download always reports on_disk, for any config.
        let model = cfg
            .models
            .iter()
            .find(|m| m.id == id)
            .ok_or_else(|| err(ApiError::NotFound(format!("model {id}"))))?;
        let dest = resolve_under_data_dir(&cfg.data_dir, &model.gguf_path);
        (spec, dest)
    };
    // Hold the downloads lock across the check, the in-use guard, and
    // the insert: two concurrent invokes for the same model must not
    // both spawn workers onto one destination file (Range-append vs
    // truncate corrupts it, and the second insert would orphan the
    // first handle).
    let mut downloads = state.downloads.lock().await;
    downloads.retain(|_, h| !h.is_terminal());
    if downloads.contains_key(&id) {
        return Err(err(ApiError::Models(format!(
            "download already in flight for model {id}"
        ))));
    }
    // In-use guard: re-downloading a file a running backend has loaded
    // truncates it under llama-server's mmap. Refuse while any running
    // backend's model resolves to this destination.
    {
        let cfg = state.config.read().await;
        let mut sup = state.supervisor().await.lock().await;
        sup.reap_exited();
        let running: std::collections::HashSet<String> = sup
            .status()
            .into_iter()
            .filter(|st| st.state == manager_supervisor::ProcessState::Running)
            .map(|st| st.id)
            .collect();
        for b in &cfg.backends {
            if running.contains(&b.id)
                && resolve_under_data_dir(&cfg.data_dir, &b.model_file) == dest
            {
                return Err(err(ApiError::InvalidInput(format!(
                    "model {id} is loaded by running backend {}; stop it before re-downloading",
                    b.id
                ))));
            }
        }
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
    let watcher_handle = handle.clone();
    downloads.insert(id.clone(), handle);
    // Starting a new attempt clears the previous terminal outcome and
    // any earlier verification stamp: until this attempt's file passes
    // verification the model counts as unverified (the start
    // preflight's durable gate reads `verified_at`).
    state.download_outcomes.lock().await.remove(&id);
    let attempt = state.begin_download_attempt(&id).await;
    {
        let mut cfg = state.config.write().await;
        if let Some(m) = cfg.models.iter_mut().find(|m| m.id == id) {
            m.verified_at = None;
        }
    }
    // Mark verification-pending BEFORE the watcher spawns; the watcher
    // removes the mark on every exit path. While marked, list_models
    // reports the model as verifying and the start preflight refuses
    // to launch a backend on a file that may be deleted as corrupt.
    state
        .verifying_downloads
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.clone());
    // Completion watcher: `download_model` returns as soon as the
    // transfer is spawned; the watcher observes the terminal state so
    // failures are recorded instead of dropped, and a pinned hash is
    // actually verified against the finished file.
    let watcher_app = app.clone();
    let watcher_id = id.clone();
    let watcher_dest = dest.clone();
    tokio::spawn(async move {
        let outcome = watcher_handle.wait().await;
        let st = watcher_app.state::<AppState>();
        // Every path below leaves verification; clear the mark first
        // so no outcome branch can skip it.
        st.verifying_downloads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&watcher_id);
        match outcome {
            Ok(()) => {
                let pinned = {
                    let cfg = st.config.read().await;
                    cfg.models
                        .iter()
                        .find(|m| m.id == watcher_id)
                        .map(|m| m.sha256.clone())
                };
                let (failed, verification_failure) = match pinned {
                    Some(sha) if is_real_pin(&sha) => {
                        match manager_models::verify_sha256(&watcher_dest, &sha).await {
                            Ok(true) => {
                                let mut cfg = st.config.write().await;
                                if let Some(m) = cfg.models.iter_mut().find(|m| m.id == watcher_id)
                                {
                                    m.verified_at = Some(chrono::Utc::now());
                                }
                                let _ = manager_config::save(&cfg, &st.config_path);
                                (None, false)
                            }
                            Ok(false) => {
                                // Fail closed: a file that does not match
                                // the pinned hash is not kept as a model.
                                match tokio::fs::remove_file(&watcher_dest).await {
                                    Ok(()) => {
                                        let mut cfg = st.config.write().await;
                                        if let Some(m) =
                                            cfg.models.iter_mut().find(|m| m.id == watcher_id)
                                        {
                                            m.verified_at = None;
                                        }
                                        let _ = manager_config::save(&cfg, &st.config_path);
                                        (
                                            Some(
                                                "SHA-256 mismatch after download — file removed; \
                                                 the download did not match the pinned hash."
                                                    .to_string(),
                                            ),
                                            true,
                                        )
                                    }
                                    Err(e) => (
                                        Some(format!(
                                            "SHA-256 mismatch after download — the file could not be removed: {e}"
                                        )),
                                        true,
                                    ),
                                }
                            }
                            Err(e) => (Some(format!("verification failed: {e}")), true),
                        }
                    }
                    _ => (None, false),
                };
                st.record_download_outcome(
                    &watcher_id,
                    DownloadOutcome {
                        attempt,
                        failed,
                        verification_failure,
                    },
                )
                .await;
            }
            Err(manager_models::ModelError::Cancelled(_)) => {
                // Operator-initiated: neither a completion nor a
                // failure — clear the outcome instead of recording one.
                st.clear_download_outcome(&watcher_id, attempt).await;
            }
            Err(e) => {
                // Transfer failures are not verification failures and
                // do not gate backend start.
                st.record_download_outcome(
                    &watcher_id,
                    DownloadOutcome {
                        attempt,
                        failed: Some(format!("download failed: {e}")),
                        verification_failure: false,
                    },
                )
                .await;
            }
        }
    });
    Ok(json!({ "model_id": id, "dest": dest }))
}

/// Cancel an in-flight download (partial file removed by the worker).
#[tauri::command]
async fn cancel_download(state: State<'_, AppState>, id: String) -> ApiResult {
    // Reap first: cancelling a download that already finished must not
    // report success — the terminal handle is dropped here and the
    // lookup below honestly reports that nothing is in flight.
    reap_downloads(&state).await;
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

/// Query captured logs. `scope` is a process id — live, stopped, or an
/// id that also has an archived previous-run ring; querying the id
/// returns current and archived lines merged — or `"all"`, which
/// merges every process's records into one stream. `level` is an
/// optional error/warn/info/debug heuristic filter; `search` is an
/// optional case-insensitive substring. Ordering is newest-first in
/// every scope, archives included.
///
/// Returns `{lines, total_matching, truncated, scopes}`: `lines` is the
/// page capped at `limit` (default 200), each record stamped with its
/// `source` process id and an `archived` flag for previous-run records;
/// `total_matching` counts every record that matched before the cap and
/// `truncated` says whether the cap cut the page, so the UI can signal
/// omission. `scopes` lists every retained scope as `{id, previous}`,
/// where `previous` means the id also has an archived previous-run
/// ring (incl. previous run).
#[tauri::command]
async fn get_logs(
    state: State<'_, AppState>,
    scope: String,
    level: Option<String>,
    search: Option<String>,
    limit: Option<u64>,
) -> ApiResult {
    let sup = state.supervisor().await.lock().await;
    let result = sup.logs_query(
        &scope,
        level.as_deref(),
        search.as_deref(),
        limit.unwrap_or(200) as usize,
    );
    let scopes: Vec<Value> = sup
        .log_scopes()
        .into_iter()
        .map(|s| json!({ "id": s.id, "previous": s.previous }))
        .collect();
    Ok(json!({
        "lines": serde_json::to_value(&result.records).map_err(jerr)?,
        "total_matching": result.total_matching,
        "truncated": result.total_matching > result.records.len(),
        "scopes": scopes,
    }))
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

/// Merge a JSON patch into the gateway config and persist: the patch is
/// merged into a clone of the config, the clone is saved via
/// `manager_config::save` (which validates before writing), and only a
/// successful save installs the clone in memory. `persisted` is true
/// only after that save; on failure the in-memory config is unchanged
/// and the error surfaces.
#[tauri::command]
async fn update_gateway_config(state: State<'_, AppState>, patch: Value) -> ApiResult {
    let mut cfg = state.config.write().await;
    let mut new_cfg = cfg.clone();
    let mut v = serde_json::to_value(&new_cfg.gateway).map_err(jerr)?;
    merge_json(&mut v, patch);
    new_cfg.gateway = serde_json::from_value(v).map_err(jerr)?;
    manager_config::save(&new_cfg, &state.config_path)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    *cfg = new_cfg;
    Ok(json!({ "ok": true, "persisted": true }))
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

/// Validate and store a new sandbox policy. The validator returns a
/// report rather than an error for a bad policy (`valid: false` with
/// the problems listed): the write is refused in that case — an
/// invalid policy file must never reach disk, where the sandbox would
/// read it as authoritative.
#[tauri::command]
async fn update_mxc_policy(state: State<'_, AppState>, policy: Value) -> ApiResult {
    let p: Policy = serde_json::from_value(policy).map_err(jerr)?;
    let report = manager_mxc::validate(&p)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    if !report.valid {
        return Err(err(ApiError::Mxc(format!(
            "MXC policy failed validation: {}",
            report.errors.join("; ")
        ))));
    }
    let cfg = state.config.read().await;
    if let Some(parent) = cfg.mxc.path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(ApiError::from)
            .map_err(jerr)?;
    }
    // Atomic replace (the manager_config::save pattern): serialize to
    // a temp file in the same directory, then rename over the target,
    // so a crash mid-write can never leave a truncated policy behind.
    let serialized = serde_json::to_string_pretty(&p).map_err(jerr)?;
    let mut tmp_path = cfg.mxc.path.clone();
    tmp_path.as_mut_os_string().push(".tmp");
    std::fs::write(&tmp_path, serialized)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    if let Err(e) = std::fs::rename(&tmp_path, &cfg.mxc.path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err(ApiError::from(e)));
    }
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

/// Mark the setup wizard complete and persist the flag: the flag is
/// set on a clone of the config, the clone is saved via
/// `manager_config::save`, and only a successful save installs it in
/// memory — a failed save returns an error rather than an ok:true
/// payload, so the UI never reports completion that a restart would
/// silently undo.
#[tauri::command]
async fn setup_mark_complete(state: State<'_, AppState>) -> ApiResult {
    let mut cfg = state.config.write().await;
    let mut new_cfg = cfg.clone();
    new_cfg.setup_completed = true;
    manager_config::save(&new_cfg, &state.config_path)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    *cfg = new_cfg;
    Ok(json!({ "ok": true, "persisted": true }))
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
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/C", "start", "", &url]);
        // CREATE_NO_WINDOW suppresses only the transient cmd console;
        // the browser that `start` opens is a separate GUI process and
        // still appears.
        cmd.creation_flags(0x08000000);
        cmd.spawn()
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
/// `backend_apply_config`). The patch is applied to a clone of the
/// config (`apply_config_diff` validates and rolls the clone back on
/// failure), the clone is persisted via `manager_config::save`, and
/// only a successful save installs it in memory — `persisted` reports
/// that truthfully. `apply_mode` is `RestartNow | OnNextRestart` —
/// `RestartNow` records a restart task for the UI to dispatch; the
/// restart itself goes through `restart_backend`.
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
        let mut new_cfg = cfg.clone();
        let diff = manager_config::apply_config_diff(&mut new_cfg, &id, &patch)
            .map_err(ApiError::from)
            .map_err(jerr)?;
        manager_config::save(&new_cfg, &state.config_path)
            .map_err(ApiError::from)
            .map_err(jerr)?;
        *cfg = new_cfg;
        diff
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
        "persisted": true,
    }))
}

/// Exact argv the supervisor would spawn (must equal the wizard's
/// planned_commands output per spec).
#[tauri::command]
async fn backend_get_launch_command(state: State<'_, AppState>, id: String) -> ApiResult {
    ensure_backend_specs(&state).await;
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

/// Gateway liveness summary: the configured shape plus a live probe.
/// The probe is a plain TCP/HTTP check against 127.0.0.1:<port>
/// (`GET /v1/models`; any syntactically valid HTTP status line = up)
/// run on a blocking thread — the gateway is a separate LiteLLM
/// process, so config alone cannot say whether it is up.
/// `routing_log_len` is the routing decision ring's actual current
/// entry count (not its capacity). `openrouter_cred_present` reports
/// whether the cloud tier's configured credential reference exists in
/// Windows Credential Manager (presence only — the value is never
/// read); it is null when no reference is configured.
#[tauri::command]
async fn gateway_status(state: State<'_, AppState>) -> ApiResult {
    let (port, groups, routing_rules, cred_ref) = {
        let cfg = state.config.read().await;
        (
            cfg.gateway.port,
            cfg.gateway.groups.len(),
            cfg.gateway.routing_rules.len(),
            cfg.gateway.openrouter.cred_ref.clone(),
        )
    };
    let up = tokio::task::spawn_blocking(move || probe_gateway(port))
        .await
        .unwrap_or(false);
    // The cmdkey presence check is a blocking subprocess call; run it
    // off the async worker like the MCP presence checks.
    let openrouter_cred_present: Option<bool> = match cred_ref.filter(|r| !r.is_empty()) {
        Some(target) => Some(
            tokio::task::spawn_blocking(move || cmdkey_target_exists(&target))
                .await
                .unwrap_or(false),
        ),
        None => None,
    };
    let routing_log_len = state.routing_log.lock().await.len();
    Ok(json!({
        "port": port,
        "groups": groups,
        "routing_rules": routing_rules,
        "up": up,
        "routing_log_len": routing_log_len,
        "openrouter_cred_present": openrouter_cred_present,
    }))
}

/// Stop the running gateway: terminate the process recorded in
/// `<data_dir>/gateway.pid` by the start-gateway step (see
/// [`manager_wizard::stop_gateway`]). Restarting is stop + re-running
/// the start-gateway step, which is what the wizard does itself.
#[tauri::command]
async fn gateway_stop(state: State<'_, AppState>) -> ApiResult {
    let data_dir = state.data_dir.clone();
    let stopped = tokio::task::spawn_blocking(move || manager_wizard::stop_gateway(&data_dir))
        .await
        .map_err(|e| err(ApiError::Gateway(format!("gateway stop task failed: {e}"))))?
        .map_err(|e| err(ApiError::Gateway(e)))?;
    Ok(json!({ "ok": true, "stopped": stopped }))
}

/// One-shot gateway liveness probe: connect to 127.0.0.1:<port> with a
/// 500 ms timeout, `GET /v1/models` with a 1 s read timeout, and report
/// whether the reply's first line is a syntactically valid HTTP status
/// line. Any status code counts — 200, 401, and 404 all prove a live
/// HTTP listener, which is all "up" means here. Any error (refused,
/// timeout, malformed reply) is simply "not up"; runs on a blocking
/// thread and never fails the caller.
fn probe_gateway(port: u16) -> bool {
    use std::io::{BufRead, BufReader, Write};
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) =
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500))
    else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(1)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(1)));
    if stream
        .write_all(b"GET /v1/models HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    if reader.read_line(&mut status_line).is_err() {
        return false;
    }
    // "HTTP/1.1 200 OK" — a valid status line starts with "HTTP/" and
    // carries a numeric status code as its second token.
    status_line.starts_with("HTTP/")
        && status_line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse::<u16>().ok())
            .is_some()
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
/// name (`{"group": "planner"}`). Runs `pick_backend` as an explainer
/// over the current config (every group member assumed healthy, no
/// load/latency data — the reason string says so) and records the
/// decision in the request ring. This is a dry run for the UI: live
/// traffic is routed by LiteLLM under its single global strategy, not
/// by `pick_backend`.
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

/// SHA-256-verify an installed model file against the hash pinned in
/// the model's config entry. Records the outcome in `verified_at`
/// (stamped on success, cleared on mismatch) and persists the config.
/// The download-outcome gate follows the same result: success REMOVES
/// any recorded verification failure for the model (the start
/// preflight stops refusing), a mismatch records one.
#[tauri::command]
async fn model_verify_checksum(state: State<'_, AppState>, id: String) -> ApiResult {
    let (path, sha) = {
        let cfg = state.config.read().await;
        let m = cfg
            .models
            .iter()
            .find(|m| m.id == id)
            .ok_or_else(|| err(ApiError::NotFound(format!("model {id}"))))?;
        if !is_real_pin(&m.sha256) {
            return Err(err(ApiError::InvalidInput(format!(
                "no SHA-256 hash is pinned for model {id}; pin one in the app configuration to enable verification"
            ))));
        }
        (
            resolve_under_data_dir(&cfg.data_dir, &m.gguf_path),
            m.sha256.clone(),
        )
    };
    let ok = manager_models::verify_sha256(&path, &sha)
        .await
        .map_err(ApiError::from)
        .map_err(jerr)?;
    {
        let mut cfg = state.config.write().await;
        if let Some(m) = cfg.models.iter_mut().find(|m| m.id == id) {
            m.verified_at = if ok { Some(chrono::Utc::now()) } else { None };
        }
        manager_config::save(&cfg, &state.config_path).map_err(jerr)?;
    }
    if ok {
        // A passing manual verify clears the start gate recorded by an
        // earlier failed verification.
        state.download_outcomes.lock().await.remove(&id);
    } else {
        state.download_outcomes.lock().await.insert(
            id.clone(),
            DownloadOutcome {
                // Not produced by a download attempt; direct insert
                // (the attempt-token gate applies to watchers only).
                attempt: 0,
                failed: Some(
                    "SHA-256 verification failed: the file on disk does not match the pinned hash"
                        .to_string(),
                ),
                verification_failure: true,
            },
        );
    }
    Ok(json!({ "id": id, "ok": ok }))
}

/// Delete a model file (guarded): refuses while any backend references
/// the model, or while a download is in flight for it. Removes the
/// model from the config on success and persists the removal; any
/// partial download file beside it is deleted too.
#[tauri::command]
async fn model_delete(state: State<'_, AppState>, id: String) -> ApiResult {
    reap_downloads(&state).await;
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
    // Delete the file first: on failure the config entry is kept. The
    // file path is the model's gguf_path resolved under the data dir —
    // the same path download_model wrote and list_models probes.
    let file_path = resolve_under_data_dir(&cfg.data_dir, &model.gguf_path);
    let report = manager_models::delete_model_file(&model, &cfg.backends, &file_path)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    cfg.models.remove(pos);
    // Persist the removal — without the save, the deleted model
    // reappears in the config (and the Models view) on the next launch.
    manager_config::save(&cfg, &state.config_path).map_err(jerr)?;
    // Drop any partial download left beside the deleted file.
    let _ = std::fs::remove_file(manager_models::partial_path(&file_path));
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
    // Validate the split against the discovered GPU count before
    // touching the config. Done before the config write lock is taken:
    // the telemetry accessor reads the config while initializing, so
    // calling it under the write lock could deadlock. A count of 0
    // means telemetry has not polled yet — the check is skipped rather
    // than rejecting against an unknown count.
    let gpu_count = lock_telemetry(&state.telemetry().await).gpu_count();
    if gpu_count > 0 && split.len() != gpu_count {
        // Same wording as manager_models::validate_split.
        return Err(err(ApiError::InvalidInput(format!(
            "tensor split has {} entries but there are {gpu_count} GPUs",
            split.len()
        ))));
    }
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
    manager_config::save(&cfg, &state.config_path).map_err(jerr)?;
    drop(cfg);
    let task_id = state
        .record_task(
            "model_assign",
            json!({ "model_id": id, "backend_id": backend_id, "split": split }),
        )
        .await;
    // The assignment is already applied and persisted above; the task is
    // the UI's rolling-restart handle, recorded complete rather than
    // left Running forever.
    state
        .finish_task(
            &task_id,
            TaskStatus::Done,
            json!({ "model_id": id, "backend_id": backend_id }),
        )
        .await;
    Ok(json!({ "task_id": task_id, "persisted": true }))
}

// ---------------------------------------------------------------------------
// Telemetry: additional §14 commands
// ---------------------------------------------------------------------------

/// Set the telemetry poll interval. Persisted immediately via
/// `manager_config::save`; the running poller's cadence is fixed when
/// the poller starts (PollHandle exposes no re-cadence), so the new
/// interval takes effect on the next app start.
#[tauri::command]
async fn telemetry_set_interval(state: State<'_, AppState>, ms: u64) -> ApiResult {
    // Persisted immediately. The running poller's cadence is fixed when
    // the poller starts (PollHandle exposes no re-cadence), so the new
    // interval takes effect on the next app start; the GPUs view labels
    // the control accordingly.
    let mut cfg = state.config.write().await;
    cfg.telemetry.poll_interval_ms = ms;
    manager_config::save(&cfg, &state.config_path).map_err(jerr)?;
    Ok(json!({ "ok": true, "poll_interval_ms": ms }))
}

/// Reset peak trackers (spec §5: peak marks are resettable).
#[tauri::command]
async fn telemetry_reset_peaks(state: State<'_, AppState>) -> ApiResult {
    lock_telemetry(&state.telemetry().await).reset_peaks();
    Ok(json!({ "ok": true }))
}

/// Write the current snapshot to the data dir.
#[tauri::command]
async fn telemetry_save_snapshot(state: State<'_, AppState>) -> ApiResult {
    let samples: Vec<GpuSample> = lock_telemetry(&state.telemetry().await).snapshot();
    let path = state
        .data_dir
        .join(format!("telemetry-snapshot-{}.json", now_ms()));
    std::fs::write(&path, serde_json::to_string_pretty(&samples).map_err(jerr)?)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    Ok(json!({ "path": path }))
}

/// Project a tensor split onto discovered VRAM (spec §14
/// `placement_propose`). The model resolves from the curated catalog
/// first, else from the config (size = the on-disk file when downloaded,
/// else a params_b estimate — see the resolution code). Free VRAM comes
/// from the latest telemetry snapshot — the same snapshot the fits
/// verdict and the payload's per-GPU `free_mib` are computed from;
/// stale GPUs count their last-known values. The response is the
/// projection plus provenance for the UI's labels: `snapshot_age_ms`
/// (age of the telemetry snapshot the projection was computed from),
/// `gpu_indices` (the GPU index each split fraction maps to), and
/// `weights_only` — the projection covers model weights only, with no
/// KV-cache or compute allowance, so "fits" is not a load guarantee.
#[tauri::command]
async fn placement_propose(
    state: State<'_, AppState>,
    model_id: String,
    split: Vec<f32>,
) -> ApiResult {
    let spec = match manager_models::curated_models()
        .into_iter()
        .find(|s| s.id == model_id)
    {
        Some(spec) => spec,
        None => {
            // Not a catalog model: resolve the configured model. Size
            // basis, in order: the on-disk file's size when the model
            // is downloaded; else an estimate from params_b using the
            // catalog's derivation (~4.92 bits per weight plus ~5%
            // overhead — an estimate, like the catalog's own sizes).
            // The config read finishes before telemetry is touched:
            // the telemetry accessor reads the config while
            // initializing, so the two must not nest.
            let cfg = state.config.read().await;
            let m = cfg
                .models
                .iter()
                .find(|m| m.id == model_id)
                .ok_or_else(|| {
                    err(ApiError::NotFound(format!(
                        "model {model_id} is neither a catalog entry nor a configured model"
                    )))
                })?;
            let path = resolve_under_data_dir(&cfg.data_dir, &m.gguf_path);
            let size_bytes = match std::fs::metadata(&path) {
                Ok(md) if md.is_file() => md.len(),
                _ if m.params_b > 0.0 => (m.params_b as f64 * 1e9 * 4.92 / 8.0 * 1.05) as u64,
                _ => {
                    return Err(err(ApiError::InvalidInput(format!(
                        "model size unknown for {model_id}: the file is not downloaded and no params_b is configured to estimate from"
                    ))))
                }
            };
            manager_models::ModelSpec {
                id: m.id.clone(),
                name: m.id.clone(),
                quant: m.quant.clone(),
                sha256: m.sha256.clone(),
                url: m.source_url.clone(),
                size_bytes,
            }
        }
    };
    let store = state.telemetry().await;
    let guard = lock_telemetry(&store);
    let mut samples = guard.snapshot();
    samples.sort_by_key(|s| s.index);
    if samples.is_empty() {
        return Err(err(ApiError::Telemetry(
            "no telemetry yet; cannot project placement".to_string(),
        )));
    }
    // Age of the snapshot: now minus the last successful poll that fed
    // the store (falling back to the newest sample's own timestamp).
    let snapshot_age_ms = guard
        .last_successful_poll_at()
        .map(|t| t.timestamp_millis())
        .or_else(|| samples.iter().map(|s| s.ts.timestamp_millis()).max())
        .map(|then_ms| now_ms().saturating_sub(then_ms.max(0) as u64));
    let gpu_indices: Vec<u32> = samples.iter().map(|s| s.index).collect();
    let free_mib: Vec<u64> = samples
        .iter()
        .map(|s| s.vram_total_mib.saturating_sub(s.vram_used_mib))
        .collect();
    drop(guard);
    let projection = manager_models::propose_placement(&spec, &split, &free_mib)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    let mut payload = serde_json::to_value(&projection).map_err(jerr)?;
    payload["snapshot_age_ms"] = json!(snapshot_age_ms);
    payload["gpu_indices"] = json!(gpu_indices);
    payload["weights_only"] = json!(true);
    Ok(payload)
}

/// Apply a tensor split to the backends serving a model (spec §14
/// `placement_apply`). Validates the split against the telemetry
/// snapshot's GPU count (the same parity rule `placement_propose`
/// enforces), writes it into the config of every backend the model is
/// assigned to, persists via `manager_config::save`, and completes the
/// task it records. When the model has no recorded assignment, the
/// target backends are derived from the backends whose model file
/// resolves to the model's gguf_path, and the apply records them into
/// `assigned_backends` — so a model configured by hand (or installed
/// by the wizard, which never records assignments) is still placeable.
/// The split takes effect on each backend's next start. All mutation
/// happens on a clone installed in memory only after a successful
/// save, so a late failure leaves memory and disk in agreement.
#[tauri::command]
async fn placement_apply(
    state: State<'_, AppState>,
    model_id: String,
    split: Vec<f32>,
) -> ApiResult {
    // Validate the split length against the telemetry snapshot first.
    // Done before any config lock is taken: the telemetry accessor reads
    // the config while initializing, so calling it under the write lock
    // could deadlock (the same constraint model_assign documents).
    let gpu_count = lock_telemetry(&state.telemetry().await).snapshot().len();
    if gpu_count == 0 {
        return Err(err(ApiError::Telemetry(
            "no telemetry yet; cannot validate placement".to_string(),
        )));
    }
    if split.len() != gpu_count {
        // Same wording as manager_models::validate_split.
        return Err(err(ApiError::InvalidInput(format!(
            "tensor split has {} entries but there are {gpu_count} GPUs",
            split.len()
        ))));
    }
    let cfg = state.config.read().await;
    let model = cfg
        .models
        .iter()
        .find(|m| m.id == model_id)
        .ok_or_else(|| err(ApiError::NotFound(format!("model {model_id}"))))?;
    let mut backend_ids = model.assigned_backends.clone();
    if backend_ids.is_empty() {
        // No recorded assignment: derive the targets from the backends
        // whose model file resolves (under the data dir) to the model's
        // gguf_path. The apply below records them into
        // assigned_backends, so later applies take the direct path.
        let model_path = resolve_under_data_dir(&cfg.data_dir, &model.gguf_path);
        backend_ids = cfg
            .backends
            .iter()
            .filter(|b| resolve_under_data_dir(&cfg.data_dir, &b.model_file) == model_path)
            .map(|b| b.id.clone())
            .collect();
    }
    drop(cfg);
    if backend_ids.is_empty() {
        return Err(err(ApiError::NotFound(format!(
            "model {model_id} is not referenced by any backend; point a backend's model file at it first"
        ))));
    }
    let task_id = state
        .record_task(
            "placement_apply",
            json!({ "model_id": model_id, "backends": backend_ids, "split": split }),
        )
        .await;
    let mut cfg = state.config.write().await;
    let outcome: Result<(), String> = (|| {
        let mut new_cfg = cfg.clone();
        // Existence first: a missing backend must not leave earlier
        // backends mutated while the command reports failure.
        for bid in &backend_ids {
            if !new_cfg.backends.iter().any(|b| &b.id == bid) {
                return Err(err(ApiError::NotFound(format!("backend {bid}"))));
            }
        }
        for bid in &backend_ids {
            let backend = new_cfg
                .backends
                .iter_mut()
                .find(|b| &b.id == bid)
                .expect("existence checked above");
            manager_models::apply_placement(backend, &split)
                .map_err(ApiError::from)
                .map_err(jerr)?;
        }
        // Record the (possibly derived) assignment on the model.
        if let Some(m) = new_cfg.models.iter_mut().find(|m| m.id == model_id) {
            for bid in &backend_ids {
                if !m.assigned_backends.contains(bid) {
                    m.assigned_backends.push(bid.clone());
                }
            }
        }
        manager_config::validate(&new_cfg)
            .map_err(ApiError::from)
            .map_err(jerr)?;
        // Persist — without the save, the applied split silently
        // vanished on app restart while the command reported success.
        manager_config::save(&new_cfg, &state.config_path).map_err(jerr)?;
        *cfg = new_cfg;
        Ok(())
    })();
    drop(cfg);
    match outcome {
        Ok(()) => {
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Done,
                    json!({ "model_id": model_id, "backends": backend_ids }),
                )
                .await;
            Ok(json!({ "task_id": task_id, "backends": backend_ids, "persisted": true }))
        }
        Err(e) => {
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Failed,
                    json!({ "model_id": model_id, "error": e }),
                )
                .await;
            Err(e)
        }
    }
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
    let samples: Vec<GpuSample> = lock_telemetry(&state.telemetry().await).snapshot();
    let telemetry_json = serde_json::to_string_pretty(&samples).map_err(jerr)?;
    let scopes: Vec<String> = {
        let sup = state.supervisor().await.lock().await;
        // Every retained log scope, not just registered specs:
        // log_scopes() also covers scopes whose process was pruned but
        // whose ring (incl. an archived previous run) is retained.
        let mut ids: Vec<String> = sup.log_scopes().into_iter().map(|s| s.id).collect();
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
            .logs_query(&scope, None, None, 5000)
            .records;
        let lines: Vec<String> = records
            .iter()
            .rev()
            .map(|r| {
                // Archived (previous-run) records keep their marker in
                // the bundle so the distinction survives export.
                let archived = if r.archived { " [archived]" } else { "" };
                format!(
                    "[{}] [{:?}]{} {}",
                    r.ts.format("%Y-%m-%dT%H:%M:%S%.3fZ"),
                    r.stream,
                    archived,
                    r.line
                )
            })
            .collect();
        log_scopes.push((scope, lines));
    }
    let versions = vec![
        ("app".to_string(), env!("CARGO_PKG_VERSION").to_string()),
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
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new("cmdkey");
        cmd.arg(format!("/list:{target}"));
        // CREATE_NO_WINDOW: this probe runs from a GUI process.
        cmd.creation_flags(0x08000000);
        cmd.output()
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

/// Set the MXC enforcement mode and persist it: the mode is set on a
/// clone of the config, the clone is saved via `manager_config::save`,
/// and only a successful save installs it in memory. `persisted` is
/// true only after that save.
#[tauri::command]
async fn mxc_set_mode(state: State<'_, AppState>, mode: MxcMode) -> ApiResult {
    let mut cfg = state.config.write().await;
    let mut new_cfg = cfg.clone();
    new_cfg.mxc.mode = mode;
    manager_config::save(&new_cfg, &state.config_path)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    *cfg = new_cfg;
    Ok(json!({ "ok": true, "mode": mode, "persisted": true }))
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

/// Pin a component to a version and persist it: the pin is set on a
/// clone of the config, the clone is saved via `manager_config::save`,
/// and only a successful save installs it in memory. `persisted` is
/// true only after that save; on failure the in-memory config is
/// unchanged and the error surfaces.
#[tauri::command]
async fn updates_pin(
    state: State<'_, AppState>,
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
    manager_config::save(&new_cfg, &state.config_path)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    *cfg = new_cfg;
    Ok(json!({ "ok": true, "persisted": true, "component_id": component_id, "pinned_version": version }))
}

/// Toggle auto-update for a component and persist it: same
/// clone → save → install pattern as [`updates_pin`].
#[tauri::command]
async fn updates_set_auto(
    state: State<'_, AppState>,
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
    manager_config::save(&new_cfg, &state.config_path)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    *cfg = new_cfg;
    Ok(json!({ "ok": true, "persisted": true, "component_id": component_id, "auto_update": enabled }))
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
            gateway_stop,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg() -> AppConfig {
        let mut cfg = AppConfig::default_config();
        cfg.data_dir = PathBuf::from("/data");
        cfg
    }

    fn backend(cfg: &AppConfig, id: &str) -> manager_config::BackendConfig {
        cfg.backends.iter().find(|b| b.id == id).unwrap().clone()
    }

    #[test]
    fn backend_spec_uses_wizard_binary_and_resolves_model() {
        let cfg = test_cfg();
        let b = backend(&cfg, "planner");
        let spec = backend_spec(&cfg, &b);
        let expected_prog = PathBuf::from("/data")
            .join("llamacpp")
            .join(if cfg!(windows) {
                "llama-server.exe"
            } else {
                "llama-server"
            });
        assert_eq!(spec.program, expected_prog);
        assert_eq!(spec.port, Some(b.port));
        // -m carries the data_dir-resolved model path.
        let pos = spec.args.iter().position(|a| a == "-m").unwrap();
        assert_eq!(
            spec.args[pos + 1],
            PathBuf::from("/data").join(&b.model_file).to_string_lossy()
        );
        // --port carries the configured port.
        let pos = spec.args.iter().position(|a| a == "--port").unwrap();
        assert_eq!(spec.args[pos + 1], b.port.to_string());
    }

    #[test]
    fn backend_spec_split_modes() {
        let cfg = test_cfg();
        for (mode, want) in [
            (manager_config::SplitMode::None, "none"),
            (manager_config::SplitMode::Layer, "layer"),
            (manager_config::SplitMode::Row, "row"),
        ] {
            let mut b = backend(&cfg, "planner");
            b.flags.split_mode = mode;
            let spec = backend_spec(&cfg, &b);
            let pos = spec.args.iter().position(|a| a == "--split-mode").unwrap();
            assert_eq!(spec.args[pos + 1], want);
        }
    }

    #[test]
    fn backend_spec_tensor_split_joined_with_commas() {
        let cfg = test_cfg();
        let mut b = backend(&cfg, "planner");
        b.flags.tensor_split = vec![0.5, 0.3, 0.2];
        let spec = backend_spec(&cfg, &b);
        let pos = spec
            .args
            .iter()
            .position(|a| a == "--tensor-split")
            .unwrap();
        assert_eq!(spec.args[pos + 1], "0.5,0.3,0.2");
    }

    #[test]
    fn backend_spec_filters_host_from_raw_flags() {
        let cfg = test_cfg();
        let mut b = backend(&cfg, "planner");
        b.raw_flags = Some("--host 0.0.0.0 --threads 4".into());
        let spec = backend_spec(&cfg, &b);
        assert!(!spec.args.iter().any(|a| a == "--host" || a == "0.0.0.0"));
        assert!(spec.args.iter().any(|a| a == "--threads"));
        b.raw_flags = Some("--host=0.0.0.0 --threads 4".into());
        let spec = backend_spec(&cfg, &b);
        assert!(!spec.args.iter().any(|a| a.starts_with("--host")));
        assert!(spec.args.iter().any(|a| a == "--threads"));
    }

    #[test]
    fn process_state_str_covers_all_variants() {
        for (v, want) in [
            (ProcessState::Stopped, "stopped"),
            (ProcessState::Starting, "starting"),
            (ProcessState::Running, "running"),
            (ProcessState::Stopping, "stopping"),
            (ProcessState::Failed, "failed"),
        ] {
            assert_eq!(process_state_str(v), want);
        }
    }
}
