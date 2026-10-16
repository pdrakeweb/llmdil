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
    test_request, BackoffPolicy, HealthProbe, HttpHealthProbe, LogLevelFilter, LogRecord,
    LogStream, ProcessSpec, ProcessState, Supervisor,
};
use manager_telemetry::{
    FallbackSource, GpuSample, NvidiaSmiSource, NvmlSource, PollCadence, Poller, TelemetryStore,
};
use manager_wizard::{
    ops::{prev_log_path, RealSystemOps},
    steps::{continue_config_yaml, default_steps, merge_env_contents, VscodeExtensions},
    RunMode, StepOutcome, StepState, SystemOps, Wizard, WizardContext, WizardReport,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{Mutex, OnceCell, RwLock};

mod activation;
mod gateway_stats;

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
    /// Last setup_get_steps payload, refreshed whenever the install
    /// wizard's state is read or mutated under its lock. Served when
    /// the wizard lock is held by a running step, so the checklist
    /// never sits blank for the duration of a step.
    wizard_steps_cache: RwLock<Option<Value>>,
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
    /// Gateway request-statistic snapshots scraped from LiteLLM's
    /// Prometheus endpoint by the background poller (see
    /// [`gateway_stats`]); `list_backends` turns them into the
    /// Dashboard's trailing-hour request/error/throughput figures.
    gateway_stats: Mutex<gateway_stats::GatewayStatsStore>,
    /// WinML secondary-backend registry (stateful single-model rule).
    winml: OnceCell<Mutex<manager_winml::WinMlRegistry>>,
}

/// On-disk form of the install wizard's progress
/// (`<data_dir>/wizard-state.json`): the wizard's own report,
/// restored at launch so closing the app does not reset the
/// checklist. Written after every state mutation.
#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedWizard {
    version: u32,
    report: WizardReport,
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
            wizard_steps_cache: RwLock::new(None),
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
            gateway_stats: Mutex::new(gateway_stats::GatewayStatsStore::default()),
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
        // The wizard reads the same model registry as every other view;
        // make sure the first-run catalog seeding has happened before
        // the snapshot is taken, whichever view runs first. (Seeding
        // used to live only inside the Models commands, so a wizard run
        // before the first Models visit saw an empty registry and its
        // download step reported "no models configured".)
        ensure_catalog_models(self).await;
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

    /// Seed the install wizard's checklist from the machine's actual
    /// state: run the audit wizard's checks once and adopt the passing
    /// outcomes, so the checklist does not reset to all-Queued on
    /// every launch. Runs in the background at startup; best-effort —
    /// the wizard stays usable (in its default state) while it runs.
    async fn hydrate_wizard(&self) {
        let ctx = self.wizard_ctx(RunMode::Audit).await;
        let audit = self.wizard(RunMode::Audit).await;
        let report = {
            let mut guard = audit.lock().await;
            guard.run_all(&ctx).await
        };
        for s in &report.steps {
            append_wizard_log(
                &self.data_dir,
                &wizard_log_block(
                    "hydration audit",
                    &s.id,
                    &format!("{:?}", s.state).to_lowercase(),
                    &s.message,
                    s.duration_ms,
                    &s.log_tail,
                ),
            );
        }
        let install = self.wizard(RunMode::Install).await;
        let mut guard = install.lock().await;
        guard.hydrate(&report);
        *self.wizard_steps_cache.write().await = Some(wizard_steps_payload(&guard));
    }

    /// Persist the install wizard's current report to
    /// `wizard-state.json` (atomic tmp + rename). Best effort: a
    /// failed write must never fail the step that triggered it.
    async fn persist_wizard(&self) {
        let wizard = self.wizard(RunMode::Install).await;
        let report = {
            let guard = wizard.lock().await;
            guard.report()
        };
        let persisted = PersistedWizard {
            version: 1,
            report,
        };
        let Ok(text) = serde_json::to_string(&persisted) else {
            return;
        };
        let path = self.data_dir.join("wizard-state.json");
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    /// Reload the live config from `<data_dir>/config.json` after a
    /// wizard step or audit run. Steps receive a snapshot clone and
    /// persist their writes to the file (tensor-split placement,
    /// verified_at stamps); without this reload those writes never
    /// reach the in-memory config, so backend starts later in the
    /// same session would still use the pre-step values. Best
    /// effort: a missing or invalid file keeps the current config —
    /// validity was enforced when the file was saved, and a wedged
    /// file must not take the session down.
    async fn reload_config_from_disk(&self) {
        let path = self.data_dir.join("config.json");
        if let Ok(cfg) = manager_config::load(&path) {
            *self.config.write().await = cfg;
        }
    }

    /// Restore persisted wizard progress at launch, before the
    /// hydration audit fills still-Queued steps from machine checks.
    /// A missing or unreadable file leaves the wizard untouched.
    async fn restore_wizard(&self) {
        let Ok(text) = std::fs::read_to_string(self.data_dir.join("wizard-state.json")) else {
            return;
        };
        let Ok(persisted) = serde_json::from_str::<PersistedWizard>(&text) else {
            return;
        };
        let wizard = self.wizard(RunMode::Install).await;
        let mut guard = wizard.lock().await;
        guard.restore(&persisted.report);
        *self.wizard_steps_cache.write().await = Some(wizard_steps_payload(&guard));
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
///
/// Seeded models start enabled (the wizard's download step installs
/// enabled models) and pre-assigned to their default backend. A default
/// backend whose `model_file` still carries the untouched default
/// (`models/<backend-id>.gguf`) is aligned to the seeded model's path
/// and enabled, so the default fleet is ready to start once its models
/// are downloaded and verified, and config validation (an enabled
/// backend's model_file must be a registered model) passes.
///
/// Configurations written before the catalog carried real hash pins
/// and parameter counts get a one-time repair first
/// ([`manager_config::repair_catalog_defaults`], gated on
/// `AppConfig::defaults_repaired`): placeholder hashes become the real
/// pins, parameter counts are filled in, default backend assignments
/// and model_file alignment are applied, and the default fleet is
/// enabled — exactly the state the current seeding produces.
async fn ensure_catalog_models(state: &AppState) {
    let backend_for = |model_id: &str| -> Option<&'static str> {
        match model_id {
            "qwen3-27b-planner" => Some("planner"),
            "qwen3-coder-30b" => Some("coder"),
            "qwen2.5-coder-7b" => Some("coder-fast"),
            "qwen3-8b" => Some("tool-runner"),
            _ => None,
        }
    };
    let mut cfg = state.config.write().await;
    let mut dirty = false;
    if !cfg.defaults_repaired {
        let catalog: Vec<manager_config::CatalogModelDefaults> = manager_models::curated_models()
            .into_iter()
            .filter_map(|spec| {
                backend_for(&spec.id).map(|backend_id| manager_config::CatalogModelDefaults {
                    model_id: spec.id,
                    sha256: spec.sha256,
                    params_b: spec.params_b,
                    backend_id: backend_id.to_string(),
                })
            })
            .collect();
        dirty = manager_config::repair_catalog_defaults(
            &mut cfg,
            &catalog,
            manager_models::PLACEHOLDER_SHA256,
        );
        cfg.defaults_repaired = true;
    }
    // Catalog sync: when the curated catalog has gained entries
    // since this configuration last synced (revision bump), append
    // the missing models with their catalog defaults — opt-in
    // candidates arrive disabled and unassigned. Runs once per
    // revision, so a model the operator deleted is not resurrected
    // until the catalog next changes.
    if cfg.catalog_seeded && cfg.catalog_revision < manager_models::CATALOG_REVISION {
        let entries: Vec<manager_config::CatalogModelEntry> =
            manager_models::curated_models()
                .into_iter()
                .map(|spec| {
                    let file_name = spec
                        .url
                        .rsplit('/')
                        .next()
                        .unwrap_or("model.gguf")
                        .to_string();
                    manager_config::CatalogModelEntry {
                        model_id: spec.id.clone(),
                        gguf_file_name: file_name,
                        quant: spec.quant.clone(),
                        params_b: spec.params_b,
                        sha256: spec.sha256.clone(),
                        source_url: spec.url.clone(),
                        assigned_backends: backend_for(&spec.id)
                            .map(|b| vec![b.to_string()])
                            .unwrap_or_default(),
                        enabled: spec.enabled_by_default,
                    }
                })
                .collect();
        manager_config::append_missing_catalog_models(&mut cfg, &entries);
        cfg.catalog_revision = manager_models::CATALOG_REVISION;
        dirty = true;
    }
    if cfg.catalog_seeded {
        if dirty {
            let _ = manager_config::save(&cfg, &state.config_path);
        }
        return;
    }
    if !cfg.models.is_empty() {
        cfg.catalog_seeded = true;
        // The catalog sync above keys off catalog_seeded, so it
        // applies from the next launch for this config.
        if dirty {
            let _ = manager_config::save(&cfg, &state.config_path);
        }
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
            let assigned_backends = backend_for(&spec.id)
                .map(|b| vec![b.to_string()])
                .unwrap_or_default();
            manager_config::ModelConfig {
                id: spec.id,
                gguf_path: PathBuf::from("models").join(file_name),
                quant: spec.quant,
                params_b: spec.params_b,
                sha256: spec.sha256,
                verified_at: None,
                source_url: spec.url,
                assigned_backends,
                enabled: spec.enabled_by_default,
            }
        })
        .collect();
    let seeded_paths: Vec<(String, PathBuf)> = cfg
        .models
        .iter()
        .flat_map(|m| {
            m.assigned_backends
                .iter()
                .map(move |b| (b.clone(), m.gguf_path.clone()))
        })
        .collect();
    for (backend_id, path) in seeded_paths {
        if let Some(backend) = cfg.backends.iter_mut().find(|b| b.id == backend_id) {
            if backend.model_file == PathBuf::from(format!("models/{backend_id}.gguf")) {
                backend.model_file = path;
            }
            // The default fleet is enabled by default; a backend
            // whose model is not downloaded and verified yet fails
            // its start preflight rather than starting unconfigured.
            backend.enabled = true;
        }
    }
    cfg.catalog_seeded = true;
    cfg.catalog_revision = manager_models::CATALOG_REVISION;
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
    // Seeding/repair first: this command validates the patched config,
    // and a fresh install's pre-seed config (default backends not yet
    // aligned to registered models) must not fail that validation.
    ensure_catalog_models(&state).await;
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

/// Adopt llama-server processes already listening on a configured
/// backend's port that this session does not supervise. Backend
/// processes are detached and survive the app closing, so after a
/// restart the previous session's llama-servers still hold their
/// ports and GPU memory while the new supervisor reports every
/// backend stopped — and every pid-keyed stat (VRAM attribution,
/// process health) reads empty. Adoption is conservative: the port
/// owner must be a live `llama-server` image, and a backend that is
/// already supervised is never touched.
async fn adopt_orphan_backends(state: &AppState) {
    ensure_backend_specs(state).await;
    let ports: Vec<(String, u16)> = {
        let cfg = state.config.read().await;
        cfg.backends
            .iter()
            .map(|b| (b.id.clone(), b.port))
            .collect()
    };
    let mut sup = state.supervisor().await.lock().await;
    sup.reap_exited();
    let live: std::collections::HashSet<String> = sup
        .status()
        .into_iter()
        .filter(|s| s.pid.is_some())
        .map(|s| s.id)
        .collect();
    for (id, port) in ports {
        if live.contains(&id) {
            continue;
        }
        if let Some(pid) = manager_supervisor::port_owner_pid(port) {
            if manager_supervisor::process_image_is(pid, "llama-server.exe") {
                let _ = sup.adopt(&id, pid);
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
    adopt_orphan_backends(&state).await;
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
                "n_ctx": b.flags.n_ctx,
                "n_batch": b.flags.n_batch,
                "group": cfg.gateway.groups.iter()
                    .find(|g| g.members.iter().any(|m| m == &b.id))
                    .map(|g| g.name.clone()),
                "state": st.map(|s| process_state_str(s.state)).unwrap_or("stopped"),
                "pid": st.and_then(|s| s.pid),
                "adopted": st.map(|s| s.adopted).unwrap_or(false),
                "restarts": st.map(|s| s.restarts).unwrap_or(0),
                "uptime_secs": st.and_then(|s| s.uptime).map(|u| u.as_secs()),
            })
        })
        .collect();
    // Gateway request statistics (LiteLLM Prometheus snapshots from
    // the background poller): attach the trailing-hour figures where
    // the store has a window for the backend. Absent fields render
    // as "—" in the UI — no stats yet is not zero stats.
    {
        let pairs: Vec<(String, u16)> = cfg
            .backends
            .iter()
            .map(|b| (b.id.clone(), b.port))
            .collect();
        if let Some(windows) = state.gateway_stats.lock().await.window_stats(&pairs) {
            for row in backends.iter_mut() {
                let Some(id) = row["id"].as_str().map(str::to_string) else {
                    continue;
                };
                let Some(w) = windows.get(&id) else {
                    continue;
                };
                row["requests_1h"] = json!(w.requests);
                row["errors_1h"] = json!(w.errors);
                row["throughput_toks"] = json!((w.tokens as f64 / w.window_secs * 10.0).round() / 10.0);
            }
        }
    }
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
            "adopted": st.adopted,
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
    // Adoption first: the pid -> backend map below is how telemetry
    // processes get attributed, and an unadopted orphan attributes
    // to nothing.
    adopt_orphan_backends(&state).await;
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
    // try_lock, not lock: a second invocation (e.g. a stray double
    // click while a step runs) must not queue on the mutex and execute
    // a step again after the current run finishes. Like the audit, it
    // reports the running state instead of failing.
    let mut guard = match wizard.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            state
                .finish_task(&task_id, TaskStatus::Done, json!({ "busy": true }))
                .await;
            return Ok(json!({ "task_id": task_id, "busy": true, "outcome": null }));
        }
    };
    let (_done_before, total) = guard.progress();
    // A start marker lands in the wizard log before the step runs:
    // a step that hangs (the install-postgres case) then leaves a
    // timestamped "started, never finished" record instead of nothing.
    append_wizard_log(
        &state.data_dir,
        &[format!(
            "[{}] === step {step_id} (install): started ===",
            chrono::Utc::now().to_rfc3339()
        )],
    );
    let outcome: StepOutcome = guard
        .run_step(&ctx, &step_id)
        .await
        .map_err(ApiError::from)
        .map_err(jerr)?;
    append_wizard_log(
        &state.data_dir,
        &wizard_log_block(
            "install",
            &step_id,
            &format!("{:?}", outcome.state).to_lowercase(),
            &outcome.message,
            outcome.duration_ms,
            &outcome.log_tail,
        ),
    );
    let (done, _) = guard.progress();
    *state.wizard_steps_cache.write().await = Some(wizard_steps_payload(&guard));
    drop(guard);
    state.persist_wizard().await;
    state.reload_config_from_disk().await;
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
    // try_lock, not lock: a duplicate audit invocation must not queue
    // a second execution. But a concurrent audit (the launch-time
    // hydration audit, or a second click from a re-opened view) is a
    // state to report, not a failure — the running audit's results
    // land in the checklist when it finishes.
    let mut guard = match wizard.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Done,
                    json!({ "already_running": true }),
                )
                .await;
            return Ok(json!({
                "task_id": task_id,
                "already_running": true,
                "done": 0,
                "total": 0,
                "steps": [],
            }));
        }
    };
    let report: WizardReport = guard.run_all(&ctx).await;
    for s in &report.steps {
        append_wizard_log(
            &state.data_dir,
            &wizard_log_block(
                "audit",
                &s.id,
                &format!("{:?}", s.state).to_lowercase(),
                &s.message,
                s.duration_ms,
                &s.log_tail,
            ),
        );
    }
    let (done, total) = report.progress();
    // The per-step results ride along in the response: the checklist is
    // painted from the install wizard's state (setup_get_steps), which
    // an audit never changes, so without this payload the audit's
    // outcome is invisible in the UI.
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
    drop(guard);
    state.reload_config_from_disk().await;
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
    Ok(json!({ "task_id": task_id, "done": done, "total": total, "steps": steps }))
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
    let catalog_ctx: HashMap<&str, u32> = catalog
        .iter()
        .map(|s| (s.id.as_str(), s.ctx_max))
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
                "enabled": m.enabled,
                "fit_check_available": false,
                "on_disk": meta.is_some(),
                "size_bytes": meta.map(|md| md.len()),
                "partial_bytes": partial_meta.map(|md| md.len()),
                "downloading": downloads.contains_key(&m.id) || is_verifying,
                "verifying": is_verifying,
                "paused": downloads.get(&m.id).map(|h| h.is_paused()).unwrap_or(false),
                "download_error": outcomes.get(&m.id).and_then(|o| o.failed.clone()),
                "catalog_size_bytes": catalog_sizes.get(m.id.as_str()).copied(),
                "ctx_max": catalog_ctx.get(m.id.as_str()).copied(),
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
            params_b: m.params_b,
            ctx_max: 0,
            enabled_by_default: m.enabled,
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

/// The file-backed gateway log records: the current run's
/// `gateway.stdout.log` / `gateway.stderr.log` under `<data_dir>/logs`
/// plus their rotated `.prev` runs (marked archived). The capture
/// writes no per-line timestamps, so every line is stamped with its
/// file's modification time; lines are emitted newest-first within
/// each file so the merged ordering stays correct under the caller's
/// stable sort. At most the last 2,000 lines of each file are read.
fn gateway_log_records(data_dir: &std::path::Path) -> (Vec<LogRecord>, bool) {
    let logs = data_dir.join("logs");
    let mut records = Vec::new();
    let mut has_previous = false;
    for (name, stream) in [
        ("gateway.stdout.log", LogStream::Stdout),
        ("gateway.stderr.log", LogStream::Stderr),
    ] {
        for archived in [false, true] {
            let base = logs.join(name);
            let path = if archived {
                prev_log_path(&base)
            } else {
                base
            };
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            if archived {
                has_previous = true;
            }
            let ts = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .map(chrono::DateTime::<chrono::Utc>::from)
                .unwrap_or_else(|_| chrono::Utc::now());
            let text = String::from_utf8_lossy(&bytes);
            for line in text.lines().rev().take(2000) {
                records.push(LogRecord {
                    ts,
                    stream,
                    line: line.to_string(),
                    source: "gateway".to_string(),
                    archived,
                });
            }
        }
    }
    (records, has_previous)
}

/// Wizard step output, persisted to `<data_dir>/logs/wizard.log` and
/// served by `get_logs` as the `wizard` scope. Step outcomes otherwise
/// live only in the in-memory wizard report: a step that hangs or an
/// app restart loses them entirely, leaving nothing to inspect (the
/// install-postgres hang, 2026-10-15). Appends are best-effort —
/// logging must never fail the step it describes. Rotation mirrors
/// the gateway logs: past ~1 MiB the file becomes `wizard.prev.log`.
fn append_wizard_log(data_dir: &std::path::Path, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    let logs = data_dir.join("logs");
    if std::fs::create_dir_all(&logs).is_err() {
        return;
    }
    let path = logs.join("wizard.log");
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 1_048_576 {
            let _ = std::fs::rename(&path, prev_log_path(&path));
        }
    }
    let mut text = String::new();
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(text.as_bytes());
    }
}

/// One step's block in the wizard log: an outcome header plus the
/// step's own log lines, each stamped so the file stays readable on
/// its own (the reader stamps records with the file mtime, like the
/// gateway files).
fn wizard_log_block(
    label: &str,
    step_id: &str,
    state: &str,
    message: &str,
    duration_ms: u64,
    log_tail: &[String],
) -> Vec<String> {
    let ts = chrono::Utc::now().to_rfc3339();
    let mut out = vec![format!(
        "[{ts}] === step {step_id} ({label}): {state} — {message} ({duration_ms} ms) ==="
    )];
    for line in log_tail {
        out.push(format!("[{ts}] {line}"));
    }
    out
}

/// File-backed wizard records for `get_logs` (mirrors
/// `gateway_log_records`): newest last-2000 lines per file.
fn wizard_log_records(data_dir: &std::path::Path) -> (Vec<LogRecord>, bool) {
    let logs = data_dir.join("logs");
    let mut records = Vec::new();
    let mut has_previous = false;
    for archived in [true, false] {
        let base = logs.join("wizard.log");
        let path = if archived { prev_log_path(&base) } else { base };
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if archived {
            has_previous = true;
        }
        let ts = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .map(chrono::DateTime::<chrono::Utc>::from)
            .unwrap_or_else(|_| chrono::Utc::now());
        let text = String::from_utf8_lossy(&bytes);
        for line in text.lines().rev().take(2000) {
            records.push(LogRecord {
                ts,
                stream: LogStream::Stdout,
                line: line.to_string(),
                source: "wizard".to_string(),
                archived,
            });
        }
    }
    (records, has_previous)
}

/// Query captured logs. `scope` is a process id — live, stopped, or an
/// id that also has an archived previous-run ring; querying the id
/// returns current and archived lines merged — `"gateway"`, the
/// file-backed gateway log (also merged into `"all"`) — or `"all"`,
/// which merges every source's records into one stream. `level` is an
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
    let limit = limit.unwrap_or(200) as usize;
    let sup = state.supervisor().await.lock().await;
    // Unfiltered fetch: filtering, merging with the file-backed
    // gateway records, ordering, and pagination happen here so every
    // source is treated identically.
    let mut records: Vec<LogRecord> = if scope == "gateway" || scope == "wizard" {
        Vec::new()
    } else {
        sup.logs_query(&scope, None, None, usize::MAX).records
    };
    let mut scopes: Vec<Value> = sup
        .log_scopes()
        .into_iter()
        .map(|s| json!({ "id": s.id, "previous": s.previous }))
        .collect();
    let (gateway_records, gateway_previous) = gateway_log_records(&state.data_dir);
    if !gateway_records.is_empty() {
        scopes.push(json!({ "id": "gateway", "previous": gateway_previous }));
        if scope == "all" || scope == "gateway" {
            records.extend(gateway_records);
        }
    }
    let (wizard_records, wizard_previous) = wizard_log_records(&state.data_dir);
    if !wizard_records.is_empty() {
        scopes.push(json!({ "id": "wizard", "previous": wizard_previous }));
        if scope == "all" || scope == "wizard" {
            records.extend(wizard_records);
        }
    }
    if let Some(filter) = level.as_deref().and_then(LogLevelFilter::parse) {
        records.retain(|r| filter.matches(&r.line));
    }
    if let Some(search) = &search {
        let needle = search.to_lowercase();
        records.retain(|r| r.line.to_lowercase().contains(&needle));
    }
    // Newest first; the sort is stable, so records sharing a timestamp
    // keep their source order (gateway files emit newest-first).
    records.sort_by(|a, b| b.ts.cmp(&a.ts));
    let total_matching = records.len();
    records.truncate(limit);
    Ok(json!({
        "lines": serde_json::to_value(&records).map_err(jerr)?,
        "total_matching": total_matching,
        "truncated": total_matching > records.len(),
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
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    let mut v = serde_json::to_value(&new_cfg.gateway).map_err(jerr)?;
    merge_json(&mut v, patch);
    new_cfg.gateway = serde_json::from_value(v).map_err(jerr)?;
    manager_config::save(&new_cfg, &state.config_path)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    *cfg = new_cfg;
    record_gateway_change(
        &state.data_dir,
        &pre_cfg,
        &cfg,
        "gateway_config",
        "Gateway configuration changed".to_string(),
        false,
    );
    Ok(json!({ "ok": true, "persisted": true }))
}

/// Journal a saved change for the gateway activation plan when it
/// affects what the running gateway serves: the generated gateway
/// configuration differs between `pre` and `post`, or `force` is set
/// for changes the YAML does not carry (the bind address). Saves
/// that leave the gateway configuration identical (a backend's
/// context length, a tensor split) are backend-process concerns and
/// create no gateway plan.
fn record_gateway_change(
    data_dir: &std::path::Path,
    pre: &AppConfig,
    post: &AppConfig,
    kind: &str,
    summary: String,
    force: bool,
) {
    if !force {
        let before = manager_gateway::generate_config_yaml(pre).unwrap_or_default();
        let after = manager_gateway::generate_config_yaml(post).unwrap_or_default();
        if before == after {
            return;
        }
    }
    activation::record_change(data_dir, pre, post, kind, summary);
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
    state.persist_wizard().await;
    Ok(json!({ "ok": true, "step_id": step_id }))
}

/// Skip a step with a note.
#[tauri::command]
async fn setup_skip_step(state: State<'_, AppState>, step_id: String, note: String) -> ApiResult {
    let wizard = state.wizard(RunMode::Install).await;
    wizard.lock().await.skip(&step_id, note);
    state.persist_wizard().await;
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

/// The setup steps payload shared by `setup_get_steps` and the
/// last-known snapshot cached in AppState.
fn wizard_steps_payload(guard: &Wizard) -> Value {
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
    json!({ "steps": steps, "done": done, "total": total })
}

/// Live wizard step states for the Setup view: id, name, state, last
/// outcome message/log tail, plus remediation + help URL per step.
/// While a step is executing (the wizard lock is held for the whole
/// run), the last known snapshot is served with `busy: true` instead
/// of blocking — blocking left the checklist blank for the duration
/// of long steps.
#[tauri::command]
async fn setup_get_steps(state: State<'_, AppState>) -> ApiResult {
    let wizard = state.wizard(RunMode::Install).await;
    match wizard.try_lock() {
        Ok(guard) => {
            let payload = wizard_steps_payload(&guard);
            *state.wizard_steps_cache.write().await = Some(payload.clone());
            Ok(payload)
        }
        Err(_) => {
            // A step holds the wizard lock for its entire execution;
            // blocking on it here left the checklist blank for the
            // duration of the step. Serve the last known snapshot,
            // flagged busy, instead.
            match state.wizard_steps_cache.read().await.clone() {
                Some(mut payload) => {
                    payload["busy"] = json!(true);
                    Ok(payload)
                }
                None => Ok(json!({ "steps": [], "done": 0, "total": 0, "busy": true })),
            }
        }
    }
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

/// The gateway master key, for the Gateway view's client-endpoint
/// card: it is both the API key local clients present and the
/// LiteLLM admin UI password (username `admin`). Read from Windows
/// Credential Manager, generating and storing on first use — the
/// same ensure the gateway spawn and the editor-config writer use,
/// so the value shown is the value in force.
#[tauri::command]
async fn gateway_master_key() -> ApiResult {
    let key = manager_wizard::ensure_gateway_master_key(&RealSystemOps)
        .await
        .map_err(jerr)?;
    Ok(json!({ "master_key": key }))
}

/// Open a folder in Windows Explorer. `path` may name the folder
/// itself or a file inside it (the diagnostics bundle path is a
/// file; its containing folder is opened). Backs the "Open folder"
/// button shown after a diagnostics export.
#[tauri::command]
async fn open_folder(path: String) -> ApiResult {
    let p = std::path::Path::new(&path);
    let dir = if p.is_dir() {
        p.to_path_buf()
    } else if p.is_file() {
        match p.parent() {
            Some(parent) => parent.to_path_buf(),
            None => {
                return Err(err(ApiError::InvalidInput(format!(
                    "no containing folder for: {path}"
                ))))
            }
        }
    } else {
        return Err(err(ApiError::InvalidInput(format!(
            "path does not exist: {path}"
        ))));
    };
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new("explorer.exe");
        cmd.arg(&dir);
        // CREATE_NO_WINDOW: no console host for the spawn; Explorer
        // itself is a GUI process and still appears.
        cmd.creation_flags(0x08000000);
        cmd.spawn()
            .map_err(|e| err(ApiError::Io(format!("failed to open folder: {e}"))))?;
        Ok(json!({ "ok": true }))
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = dir;
        Err(err(ApiError::Unsupported(
            "open_folder is only supported on Windows".to_string(),
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
    state.persist_wizard().await;
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
        let pre_cfg = cfg.clone();
        let mut new_cfg = cfg.clone();
        let diff = manager_config::apply_config_diff(&mut new_cfg, &id, &patch)
            .map_err(ApiError::from)
            .map_err(jerr)?;
        manager_config::save(&new_cfg, &state.config_path)
            .map_err(ApiError::from)
            .map_err(jerr)?;
        *cfg = new_cfg;
        record_gateway_change(
            &state.data_dir,
            &pre_cfg,
            &cfg,
            "backend_apply_config",
            format!("Backend `{id}` settings changed"),
            false,
        );
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

/// Create a new backend and route it through the gateway (spec §14
/// `backend_add`). The backend is created with the seeded defaults
/// (32K context, a tensor split assigned from live telemetry when
/// available, the standard restart policy) and
/// the chosen registered model; it joins an existing gateway group
/// or a new one created for it. The gateway picks the route up when
/// it next (re)starts — its config is generated from this state.
#[tauri::command]
async fn backend_add(
    state: State<'_, AppState>,
    id: String,
    model_id: String,
    port: u16,
    group: String,
    create_group: bool,
    enabled: bool,
) -> ApiResult {
    let id = id.trim().to_string();
    let group = group.trim().to_string();
    let valid_slug = |s: &str| {
        !s.is_empty()
            && s.len() <= 48
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !s.starts_with('-')
            && !s.ends_with('-')
    };
    if !valid_slug(&id) {
        return Err(err(ApiError::InvalidInput(
            "backend name must be lowercase letters, digits, and hyphens".to_string(),
        )));
    }
    if !valid_slug(&group) {
        return Err(err(ApiError::InvalidInput(
            "group name must be lowercase letters, digits, and hyphens".to_string(),
        )));
    }
    if port == 0 {
        return Err(err(ApiError::InvalidInput(
            "port must be non-zero".to_string(),
        )));
    }
    // Placement for the new backend, computed before the config
    // write lock is taken (the telemetry accessor reads the config
    // while initializing, so the two must not nest). Existing enabled
    // backends are accounted for as they are; the new backend is
    // assigned a split by the same algorithm the wizard's
    // tensor-split step uses. No telemetry yet (or a single GPU)
    // leaves the split unset — the wizard step assigns it later.
    let new_backend_split: Option<(Vec<f32>, manager_config::SplitMode)> = {
        let snap = state.config.read().await.clone();
        let vrams: Vec<u64> = {
            let store = state.telemetry().await;
            let guard = lock_telemetry(&store);
            let mut samples = guard.snapshot();
            samples.sort_by_key(|s| s.index);
            samples.iter().map(|s| s.vram_total_mib).collect()
        };
        if vrams.len() < 2 {
            None
        } else {
            let size_of = |model_file: &std::path::Path| -> u64 {
                let target = resolve_under_data_dir(&snap.data_dir, &model_file.to_path_buf());
                let model = snap.models.iter().find(|m| {
                    resolve_under_data_dir(&snap.data_dir, &m.gguf_path) == target
                });
                match model {
                    Some(m) => {
                        let path = resolve_under_data_dir(&snap.data_dir, &m.gguf_path);
                        match std::fs::metadata(&path) {
                            Ok(md) if md.is_file() => md.len(),
                            _ => manager_wizard::steps::estimate_model_bytes(m.params_b as f64),
                        }
                    }
                    None => 0,
                }
            };
            let mut inputs: Vec<manager_wizard::steps::SplitInput> = snap
                .backends
                .iter()
                .filter(|b| b.enabled)
                .map(|b| manager_wizard::steps::SplitInput {
                    model_bytes: size_of(&b.model_file),
                    existing: if b.flags.tensor_split.is_empty() {
                        None
                    } else {
                        Some(b.flags.tensor_split.clone())
                    },
                    assign: false,
                })
                .collect();
            let new_model = snap.models.iter().find(|m| m.id == model_id);
            inputs.push(manager_wizard::steps::SplitInput {
                model_bytes: match new_model {
                    Some(m) => size_of(&m.gguf_path),
                    None => 0,
                },
                existing: None,
                assign: true,
            });
            manager_wizard::steps::assign_splits(&inputs, &vrams)
                .into_iter()
                .last()
                .flatten()
        }
    };
    let mut cfg = state.config.write().await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    if new_cfg.backends.iter().any(|b| b.id == id) {
        return Err(err(ApiError::InvalidInput(format!(
            "a backend with id '{id}' already exists"
        ))));
    }
    let model = new_cfg
        .models
        .iter()
        .find(|m| m.id == model_id)
        .ok_or_else(|| err(ApiError::NotFound(format!("model {model_id}"))))?
        .clone();
    if create_group {
        if new_cfg.gateway.groups.iter().any(|g| g.name == group) {
            return Err(err(ApiError::InvalidInput(format!(
                "group '{group}' already exists; join it instead of creating it"
            ))));
        }
        new_cfg.gateway.groups.push(manager_config::ModelGroup {
            name: group.clone(),
            members: vec![id.clone()],
            strategy: manager_config::RoutingStrategy::SimpleShuffle,
            fallbacks: Vec::new(),
        });
    } else {
        let g = new_cfg
            .gateway
            .groups
            .iter_mut()
            .find(|g| g.name == group)
            .ok_or_else(|| err(ApiError::NotFound(format!("group {group}"))))?;
        if !g.members.iter().any(|m| m == &id) {
            g.members.push(id.clone());
        }
    }
    let (tensor_split, split_mode) = match &new_backend_split {
        Some((split, mode)) => (split.clone(), *mode),
        None => (Vec::new(), manager_config::SplitMode::None),
    };
    new_cfg.backends.push(manager_config::BackendConfig {
        id: id.clone(),
        enabled,
        model_file: model.gguf_path.clone(),
        port,
        flags: manager_config::ServerFlags {
            n_ctx: 32768,
            n_batch: 512,
            tensor_split,
            split_mode,
        },
        raw_flags: None,
        restart_policy: manager_config::RestartPolicy {
            max_retries: 5,
            backoff_base_secs: 2,
        },
        overrides_global: false,
    });
    if let Some(m) = new_cfg.models.iter_mut().find(|m| m.id == model_id) {
        if !m.assigned_backends.contains(&id) {
            m.assigned_backends.push(id.clone());
        }
    }
    manager_config::validate(&new_cfg)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    manager_config::save(&new_cfg, &state.config_path)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    *cfg = new_cfg;
    record_gateway_change(
        &state.data_dir,
        &pre_cfg,
        &cfg,
        "backend_add",
        format!("Backend `{id}` added to group `{group}`"),
        false,
    );
    drop(cfg);
    ensure_backend_specs(&state).await;
    Ok(json!({ "backend_id": id, "group": group, "persisted": true }))
}

/// Update a backend's llama-server sizing flags: context length
/// (`-c`) and batch size (`-b`). This is the supported way to change
/// them after a backend is created — before this command the only
/// paths were hand-editing config.json or removing and re-adding the
/// backend. Takes effect on the backend's next start; a running
/// backend keeps its current values until it is restarted.
#[tauri::command]
async fn backend_update_flags(
    state: State<'_, AppState>,
    id: String,
    n_ctx: u32,
    n_batch: u32,
) -> ApiResult {
    if !(1024..=1_048_576).contains(&n_ctx) {
        return Err(err(ApiError::InvalidInput(
            "context length must be between 1024 and 1048576".to_string(),
        )));
    }
    if n_batch == 0 || n_batch > n_ctx {
        return Err(err(ApiError::InvalidInput(
            "batch size must be between 1 and the context length".to_string(),
        )));
    }
    let mut cfg = state.config.write().await;
    let mut new_cfg = cfg.clone();
    let b = new_cfg
        .backends
        .iter_mut()
        .find(|b| b.id == id)
        .ok_or_else(|| err(ApiError::NotFound(format!("backend {id}"))))?;
    b.flags.n_ctx = n_ctx;
    b.flags.n_batch = n_batch;
    manager_config::validate(&new_cfg)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    manager_config::save(&new_cfg, &state.config_path)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    *cfg = new_cfg;
    drop(cfg);
    ensure_backend_specs(&state).await;
    Ok(json!({ "backend_id": id, "persisted": true, "applies": "next start" }))
}

/// Remove a backend (spec §14 `backend_delete`). Refused while the
/// backend is running — stop it first. The id is scrubbed from every
/// group's member list (the gateway config generator rejects unknown
/// members) and from models' assignment lists. A group left with no
/// members stays in the config: it generates no gateway deployments
/// and can gain members again later.
#[tauri::command]
async fn backend_delete(state: State<'_, AppState>, id: String) -> ApiResult {
    {
        let sup = state.supervisor().await;
        let mut sup = sup.lock().await;
        sup.reap_exited();
        if let Some(st) = sup.status().iter().find(|s| s.id == id) {
            if st.pid.is_some() {
                return Err(err(ApiError::InvalidInput(format!(
                    "backend {id} is running; stop it before removing it"
                ))));
            }
        }
    }
    let mut cfg = state.config.write().await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    let pos = new_cfg
        .backends
        .iter()
        .position(|b| b.id == id)
        .ok_or_else(|| err(ApiError::NotFound(format!("backend {id}"))))?;
    new_cfg.backends.remove(pos);
    for g in new_cfg.gateway.groups.iter_mut() {
        g.members.retain(|m| m != &id);
    }
    for m in new_cfg.models.iter_mut() {
        m.assigned_backends.retain(|b| b != &id);
    }
    manager_config::validate(&new_cfg)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    manager_config::save(&new_cfg, &state.config_path)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    *cfg = new_cfg;
    record_gateway_change(
        &state.data_dir,
        &pre_cfg,
        &cfg,
        "backend_delete",
        format!("Backend `{id}` removed"),
        false,
    );
    drop(cfg);
    ensure_backend_specs(&state).await;
    Ok(json!({ "backend_id": id, "deleted": true, "persisted": true }))
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
    let (port, bind_host, probe_host, groups, routing_rules, cred_ref) = {
        let cfg = state.config.read().await;
        (
            cfg.gateway.port,
            cfg.gateway.bind_host.clone(),
            manager_wizard::steps::gateway_probe_host(&cfg),
            cfg.gateway.groups.len(),
            cfg.gateway.routing_rules.len(),
            cfg.gateway.openrouter.cred_ref.clone(),
        )
    };
    let up = tokio::task::spawn_blocking(move || probe_gateway(&probe_host, port))
        .await
        .unwrap_or(false);
    // The workstation's primary LAN address, for the client endpoint
    // shown when the gateway binds beyond loopback. A UDP connect
    // sends nothing; it selects the egress interface.
    let lan_ip = if bind_host != "127.0.0.1" {
        tokio::task::spawn_blocking(primary_lan_ip)
            .await
            .unwrap_or(None)
    } else {
        None
    };
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
    // Activation-plan state for the status surfaces: saved changes
    // the running gateway has not picked up (journal entries no
    // gateway start has resolved, or live config drift).
    let (activation_pending, activation_changes) = {
        let cfg = state.config.read().await;
        let journal = activation::load_journal(&state.data_dir);
        let unresolved = activation::unresolved_entries(&state.data_dir, &journal);
        (
            !unresolved.is_empty() || activation::config_drift(&cfg, &state.data_dir),
            unresolved.len(),
        )
    };
    Ok(json!({
        "port": port,
        "bind_host": bind_host,
        "lan_ip": lan_ip,
        "groups": groups,
        "routing_rules": routing_rules,
        "up": up,
        "routing_log_len": routing_log_len,
        "openrouter_cred_present": openrouter_cred_present,
        "activation_pending": activation_pending,
        "activation_changes": activation_changes,
    }))
}

/// The gateway activation plan: saved changes waiting on a gateway
/// restart, with the state the operator's decision depends on.
#[tauri::command]
async fn gateway_activation_plan(state: State<'_, AppState>) -> ApiResult {
    let cfg = state.config.read().await.clone();
    let journal = activation::load_journal(&state.data_dir);
    let unresolved = activation::unresolved_entries(&state.data_dir, &journal);
    let drift = activation::config_drift(&cfg, &state.data_dir);
    let pending = !unresolved.is_empty() || drift;
    let probe_host = manager_wizard::steps::gateway_probe_host(&cfg);
    let port = cfg.gateway.port;
    let up = tokio::task::spawn_blocking(move || probe_gateway(&probe_host, port))
        .await
        .unwrap_or(false);
    let changes: Vec<Value> = unresolved
        .iter()
        .map(|e| json!({ "ts": e.ts, "kind": e.kind, "summary": e.summary }))
        .collect();
    Ok(json!({
        "pending": pending,
        "gateway_up": up,
        "drift": drift,
        "applies_at_next_start": pending && !up,
        "changes": changes,
        "snapshot_available": journal.snapshot.is_some() && !unresolved.is_empty(),
    }))
}

/// Cancel the pending activation plan's changes: restore the
/// configuration snapshot taken before the first unresolved change.
/// Refused when the configuration has changed through another path
/// since (the restore would discard that change too).
#[tauri::command]
async fn gateway_activation_cancel(state: State<'_, AppState>) -> ApiResult {
    let mut cfg = state.config.write().await;
    let restored = match activation::cancel_target(&state.data_dir, &cfg) {
        Ok(Some(restored)) => restored,
        Ok(None) => return Ok(json!({ "cancelled": false, "reason": "no pending changes" })),
        Err(msg) => return Err(err(ApiError::InvalidInput(msg))),
    };
    manager_config::validate(&restored)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    manager_config::save(&restored, &state.config_path)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    *cfg = restored;
    drop(cfg);
    activation::clear_journal(&state.data_dir);
    ensure_backend_specs(&state).await;
    Ok(json!({ "cancelled": true }))
}

/// Mark the activation plan complete after a successful gateway
/// (re)start. The start-gateway step regenerates everything from
/// the live configuration, so journaled changes are live by
/// definition at that point; the plan also self-resolves through
/// the journal's mtime rule, this just clears the files promptly.
#[tauri::command]
async fn gateway_activation_complete(state: State<'_, AppState>) -> ApiResult {
    activation::clear_journal(&state.data_dir);
    Ok(json!({ "cleared": true }))
}

/// Quiesce wait: poll the gateway's in-flight request gauge until
/// it reaches zero or the timeout expires. A gateway that does not
/// answer metrics (down, or the metrics config not yet active)
/// counts as drained — nothing can be in flight through a gateway
/// that is not serving, and the gauge series only exists once
/// traffic has flowed.
#[tauri::command]
async fn gateway_quiesce_wait(state: State<'_, AppState>, timeout_secs: u64) -> ApiResult {
    let timeout = timeout_secs.clamp(5, 600);
    let (host, port) = {
        let cfg = state.config.read().await;
        (
            manager_wizard::steps::gateway_probe_host(&cfg),
            cfg.gateway.port,
        )
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
    let mut last_in_flight: Option<f64> = None;
    loop {
        let key = RealSystemOps
            .credential_read(manager_wizard::LITELLM_MASTER_KEY_CRED_TARGET)
            .await
            .unwrap_or_default();
        let body = if key.is_empty() {
            None
        } else {
            let (h, k) = (host.clone(), key);
            tokio::task::spawn_blocking(move || fetch_gateway_metrics(&h, port, &k))
                .await
                .ok()
                .flatten()
        };
        match body {
            Some(text) => match activation::parse_in_flight(&text) {
                Some(0.0) | None => {
                    return Ok(json!({ "drained": true, "in_flight": 0, "timed_out": false }))
                }
                Some(n) => last_in_flight = Some(n),
            },
            None => {
                return Ok(json!({ "drained": true, "in_flight": 0, "timed_out": false }))
            }
        }
        if std::time::Instant::now() >= deadline {
            return Ok(json!({
                "drained": false,
                "in_flight": last_in_flight.unwrap_or(0.0),
                "timed_out": true,
            }));
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// Stop the running gateway: terminate the process recorded in
/// `<data_dir>/gateway.pid` by the start-gateway step (see
/// [`manager_wizard::stop_gateway`]). Restarting is stop + re-running
/// the start-gateway step, which is what the wizard does itself.
#[tauri::command]
async fn gateway_stop(state: State<'_, AppState>) -> ApiResult {
    let data_dir = state.data_dir.clone();
    let stopped = tokio::task::spawn_blocking({
        let data_dir = data_dir.clone();
        move || manager_wizard::stop_gateway(&data_dir)
    })
        .await
        .map_err(|e| err(ApiError::Gateway(format!("gateway stop task failed: {e}"))))?
        .map_err(|e| err(ApiError::Gateway(e)))?;
    // The app-managed PostgreSQL server exists to serve the
    // gateway's database; stop it after the gateway so nothing is
    // left holding pooled connections to a stopping server.
    // Best-effort: the gateway stop already succeeded, so a
    // PostgreSQL stop problem is reported in the payload, not as a
    // command failure.
    let postgres_stopped =
        manager_wizard::steps::stop_postgres_server(&RealSystemOps, &data_dir)
            .await
            .unwrap_or(false);
    Ok(json!({ "ok": true, "stopped": stopped, "postgres_stopped": postgres_stopped }))
}

/// One-shot gateway liveness probe: connect to <host>:<port> with a
/// 500 ms timeout, `GET /v1/models` with a 1 s read timeout, and report
/// whether the reply's first line is a syntactically valid HTTP status
/// line. Any status code counts — 200, 401, and 404 all prove a live
/// HTTP listener, which is all "up" means here. Any error (refused,
/// timeout, malformed reply) is simply "not up"; runs on a blocking
/// thread and never fails the caller. The host is the app's probe
/// host for the configured bind address (loopback whenever the bind
/// covers it).
fn probe_gateway(host: &str, port: u16) -> bool {
    use std::io::{BufRead, BufReader, Write};
    let Ok(ip) = host.parse::<std::net::IpAddr>() else {
        return false;
    };
    let addr = std::net::SocketAddr::new(ip, port);
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

/// The workstation's primary LAN IPv4 address, or None when it
/// cannot be determined. A UDP "connect" sends no traffic; it makes
/// the OS select the egress interface, whose address is the one LAN
/// clients should use.
fn primary_lan_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    if ip.is_loopback() || ip.is_unspecified() {
        None
    } else {
        Some(ip.to_string())
    }
}

/// Set the gateway bind address (spec: Pete's decision 2026-10-15 —
/// 0.0.0.0 by default so LAN clients can use the gateway, 127.0.0.1
/// for this computer only; any other value must be an IP address on
/// this machine). Applies at the next gateway start; the firewall
/// rule is reconciled by the start-gateway step at that point.
#[tauri::command]
async fn gateway_set_bind_host(state: State<'_, AppState>, host: String) -> ApiResult {
    if host.parse::<std::net::IpAddr>().is_err() {
        return Err(err(ApiError::InvalidInput(format!(
            "bind host {host:?} is not an IP address"
        ))));
    }
    let mut cfg = state.config.write().await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    new_cfg.gateway.bind_host = host.clone();
    manager_config::validate(&new_cfg)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    manager_config::save(&new_cfg, &state.config_path)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    *cfg = new_cfg;
    record_gateway_change(
        &state.data_dir,
        &pre_cfg,
        &cfg,
        "gateway_bind",
        format!("Gateway network access changed to {host}"),
        true,
    );
    Ok(json!({ "bind_host": host, "restart_required": true }))
}

/// Fetch the gateway's Prometheus exposition (`GET /metrics`, the
/// master key as Bearer). Returns the body only on an HTTP 200;
/// anything else — gateway down, the metrics callback not active
/// (config predates v0.1.18 until the next gateway start), an auth
/// failure — is None and the poll cycle is skipped.
fn fetch_gateway_metrics(host: &str, port: u16, master_key: &str) -> Option<String> {
    use std::io::{Read, Write};
    let ip: std::net::IpAddr = host.parse().ok()?;
    let addr = std::net::SocketAddr::new(ip, port);
    let mut stream =
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(800))
            .ok()?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(2)));
    let req = format!(
        "GET /metrics HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {master_key}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut raw: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > 8 * 1024 * 1024 {
                    return None;
                }
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&raw);
    let mut parts = text.splitn(2, "\r\n\r\n");
    let head = parts.next()?;
    if !head.starts_with("HTTP/1.1 200") && !head.starts_with("HTTP/1.0 200") {
        return None;
    }
    // A chunked body would corrupt the exposition parse. LiteLLM's
    // /metrics answers with a content-length body, but skip the cycle
    // rather than misread if that ever changes.
    if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        return None;
    }
    Some(parts.next()?.to_string())
}

/// One gateway-metrics poll cycle: resolve the probe host and port,
/// read the stored master key (never generate one here — polling
/// must not create credentials), fetch and record a snapshot. Any
/// missing piece skips the cycle silently.
async fn poll_gateway_metrics_once(state: &AppState) {
    let (host, port) = {
        let cfg = state.config.read().await;
        (
            manager_wizard::steps::gateway_probe_host(&cfg),
            cfg.gateway.port,
        )
    };
    let Some(key) = RealSystemOps
        .credential_read(manager_wizard::LITELLM_MASTER_KEY_CRED_TARGET)
        .await
    else {
        return;
    };
    if key.is_empty() {
        return;
    }
    let body = tokio::task::spawn_blocking(move || fetch_gateway_metrics(&host, port, &key))
        .await
        .ok()
        .flatten();
    if let Some(body) = body {
        let snap = gateway_stats::parse_metrics(&body, std::time::Instant::now());
        state.gateway_stats.lock().await.record(snap);
    }
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

/// Enable or disable a model for wizard downloads. Disabling keeps the
/// model's file and configuration; the setup wizard's download step
/// skips disabled models.
#[tauri::command]
async fn model_set_enabled(state: State<'_, AppState>, id: String, enabled: bool) -> ApiResult {
    let mut cfg = state.config.write().await;
    let model = cfg
        .models
        .iter_mut()
        .find(|m| m.id == id)
        .ok_or_else(|| err(ApiError::NotFound(format!("model {id}"))))?;
    model.enabled = enabled;
    manager_config::save(&cfg, &state.config_path).map_err(jerr)?;
    drop(cfg);
    Ok(json!({ "model_id": id, "enabled": enabled }))
}

/// Add a model to the registry: an id, the https URL of its GGUF file,
/// and optionally an explicit file name, quant label, and parameter
/// count. The model starts enabled with no pinned SHA-256 — its
/// downloads run unverified (the Models view shows "hash not pinned")
/// until a hash is recorded in the configuration.
#[tauri::command]
async fn model_add(
    state: State<'_, AppState>,
    id: String,
    source_url: String,
    file_name: Option<String>,
    quant: Option<String>,
    params_b: Option<f32>,
) -> ApiResult {
    let id = id.trim().to_string();
    if id.is_empty() {
        return Err(err(ApiError::InvalidInput(
            "model id is required".to_string(),
        )));
    }
    if !source_url.starts_with("https://") {
        return Err(err(ApiError::InvalidInput(
            "source URL must be an https:// URL for a GGUF file".to_string(),
        )));
    }
    let file_name = file_name
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
        .unwrap_or_else(|| {
            source_url
                .rsplit('/')
                .next()
                .unwrap_or("model.gguf")
                .to_string()
        });
    if !file_name.to_lowercase().ends_with(".gguf")
        || file_name.contains('/')
        || file_name.contains('\\')
    {
        return Err(err(ApiError::InvalidInput(
            "file name must be a plain .gguf file name".to_string(),
        )));
    }
    let mut cfg = state.config.write().await;
    if cfg.models.iter().any(|m| m.id == id) {
        return Err(err(ApiError::InvalidInput(format!(
            "a model with id '{id}' already exists"
        ))));
    }
    cfg.models.push(manager_config::ModelConfig {
        gguf_path: PathBuf::from("models").join(&file_name),
        id: id.clone(),
        quant: quant
            .map(|q| q.trim().to_string())
            .filter(|q| !q.is_empty())
            .unwrap_or_else(|| "unknown".to_string()),
        params_b: params_b.filter(|p| *p > 0.0).unwrap_or(0.0),
        sha256: String::new(),
        verified_at: None,
        source_url,
        assigned_backends: Vec::new(),
        enabled: true,
    });
    manager_config::save(&cfg, &state.config_path).map_err(jerr)?;
    drop(cfg);
    Ok(json!({ "model_id": id }))
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
                params_b: m.params_b,
                ctx_max: 0,
                enabled_by_default: m.enabled,
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
    // A down GPU (absent from recent polls, retained by the store
    // with last-known readings) has no usable capacity: project it as
    // zero free VRAM so no split fraction is planned onto it, and
    // name it in the payload so the UI can say why it does not fit.
    let down_indices: Vec<u32> = samples
        .iter()
        .filter(|s| s.down)
        .map(|s| s.index)
        .collect();
    let free_mib: Vec<u64> = samples
        .iter()
        .map(|s| {
            if s.down {
                0
            } else {
                s.vram_total_mib.saturating_sub(s.vram_used_mib)
            }
        })
        .collect();
    drop(guard);
    let projection = manager_models::propose_placement(&spec, &split, &free_mib)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    let mut payload = serde_json::to_value(&projection).map_err(jerr)?;
    payload["snapshot_age_ms"] = json!(snapshot_age_ms);
    payload["gpu_indices"] = json!(gpu_indices);
    payload["down_indices"] = json!(down_indices);
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
    // The telemetry poller starts lazily on first use; in a session
    // that never opened a telemetry view, this call starts it and an
    // immediate snapshot is still empty. Give the first poll a short
    // window to land so the bundle carries the machine's GPU state
    // instead of an empty array.
    let store = state.telemetry().await;
    let mut samples: Vec<GpuSample> = lock_telemetry(&store).snapshot();
    for _ in 0..10 {
        if !samples.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        samples = lock_telemetry(&store).snapshot();
    }
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
    // The file-backed gateway log joins the bundle as the "gateway"
    // scope: it is the record of the most recent gateway failures
    // (startup errors included) and the supervisor hub never sees it.
    let (gateway_records, _) = gateway_log_records(&state.data_dir);
    if !gateway_records.is_empty() {
        let lines: Vec<String> = gateway_records
            .iter()
            .rev()
            .map(|r| {
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
        log_scopes.push(("gateway".to_string(), lines));
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
/// `vscode_write_config`): the `config.yaml` + `.env` pair current
/// Continue loads (the legacy `config.json` write was retired in
/// v0.1.14 — with a config.yaml present the JSON is inert, and its
/// stale literal key is a hazard if the YAML is removed). When
/// `backup` is true, an existing `~/.continue/config.yaml` is first
/// renamed to a timestamped backup (a legacy `config.json` is backed
/// up alongside it), and the `.env` key line is merged in place; when
/// false, the files are only staged under the data dir and the
/// canonical files are left untouched. Returns old/new model lists
/// so the UI can show the diff before apply.
#[tauri::command]
async fn vscode_write_config(state: State<'_, AppState>, backup: bool) -> ApiResult {
    let cfg = state.config.read().await;
    // The editor client authenticates with the gateway master key;
    // ensure (generate + store on first use) so the written config
    // works against the key-guarded gateway. The key itself goes to
    // ~/.continue/.env — config.yaml references it as
    // ${{ secrets.LITELLM_API_KEY }} and carries no literal secret.
    let master_key = manager_wizard::ensure_gateway_master_key(&RealSystemOps)
        .await
        .map_err(jerr)?;
    let new_yaml = continue_config_yaml(&cfg);
    let new_models: Value = serde_yaml::from_str::<Value>(&new_yaml)
        .map_err(|e| e.to_string())?
        .get("models")
        .cloned()
        .unwrap_or(Value::Null);
    drop(cfg);

    let staged_dir = state.data_dir.join("vscode");
    std::fs::create_dir_all(&staged_dir)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    let staged = staged_dir.join("continue-config.yaml");
    std::fs::write(&staged, new_yaml.as_bytes())
        .map_err(ApiError::from)
        .map_err(jerr)?;
    std::fs::write(
        staged_dir.join("continue.env"),
        format!("LITELLM_API_KEY={master_key}\n"),
    )
    .map_err(ApiError::from)
    .map_err(jerr)?;

    let mut out = json!({
        "staged": staged,
        "canonical": Value::Null,
        "backup_path": Value::Null,
        "legacy_backup_path": Value::Null,
        "old_models": Value::Null,
        "new_models": new_models,
    });
    if !backup {
        return Ok(out);
    }
    let Some(home) = home_dir() else {
        return Ok(out);
    };
    let dir = home.join(".continue");
    std::fs::create_dir_all(&dir)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    let canonical = dir.join("config.yaml");
    let legacy = dir.join("config.json");
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut old_models = Value::Null;
    let mut backup_path = Value::Null;
    if canonical.exists() {
        let existing = std::fs::read_to_string(&canonical)
            .map_err(ApiError::from)
            .map_err(jerr)?;
        old_models = serde_yaml::from_str::<Value>(&existing)
            .ok()
            .and_then(|v| v.get("models").cloned())
            .unwrap_or(Value::Null);
        let bak = dir.join(format!("config.yaml.bak-{ts_ms}"));
        std::fs::rename(&canonical, &bak)
            .map_err(ApiError::from)
            .map_err(jerr)?;
        backup_path = serde_json::to_value(&bak).map_err(jerr)?;
    } else if legacy.exists() {
        // No YAML yet: the diff's old side is the legacy JSON config.
        let existing = std::fs::read_to_string(&legacy)
            .map_err(ApiError::from)
            .map_err(jerr)?;
        old_models = serde_json::from_str::<Value>(&existing)
            .ok()
            .and_then(|v| v.get("models").cloned())
            .unwrap_or(Value::Null);
    }
    std::fs::write(&canonical, new_yaml.as_bytes())
        .map_err(ApiError::from)
        .map_err(jerr)?;
    // The .env secret: merged by key, never backed up or replaced.
    let env_path = dir.join(".env");
    let existing_env = std::fs::read_to_string(&env_path).unwrap_or_default();
    std::fs::write(
        &env_path,
        merge_env_contents(&existing_env, "LITELLM_API_KEY", &master_key),
    )
    .map_err(ApiError::from)
    .map_err(jerr)?;
    // A legacy config.json is inert once config.yaml exists, but if
    // the YAML is ever removed the JSON — with its stale literal
    // key — becomes live again. Back it aside with the same backup.
    let mut legacy_backup_path = Value::Null;
    if legacy.exists() {
        let bak = dir.join(format!("config.json.bak-{ts_ms}"));
        std::fs::rename(&legacy, &bak)
            .map_err(ApiError::from)
            .map_err(jerr)?;
        legacy_backup_path = serde_json::to_value(&bak).map_err(jerr)?;
    }
    out["canonical"] = serde_json::to_value(&canonical).map_err(jerr)?;
    out["backup_path"] = backup_path;
    out["legacy_backup_path"] = legacy_backup_path;
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
            // Hydrate the wizard checklist from the machine's actual
            // state in the background (audit checks adopted into the
            // install wizard), so a relaunched app does not present
            // setup as untouched when most of it is already in place.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<AppState>();
                state.restore_wizard().await;
                state.hydrate_wizard().await;
                state.persist_wizard().await;
            });
            // Gateway metrics poller: scrape LiteLLM's Prometheus
            // endpoint every 15 s into the stats store that feeds
            // the Dashboard's request/error/throughput figures.
            // Cycles skip silently while the gateway is down.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<AppState>();
                loop {
                    poll_gateway_metrics_once(&state).await;
                    tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                }
            });
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
            backend_add,
            backend_update_flags,
            backend_delete,
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
            open_folder,
            gateway_master_key,
            gateway_set_bind_host,
            // Models
            list_models,
            download_model,
            cancel_download,
            model_pause_download,
            model_resume_download,
            model_verify_checksum,
            model_delete,
            model_assign,
            model_add,
            model_set_enabled,
            // Logs
            get_logs,
            // Gateway
            get_gateway_config,
            update_gateway_config,
            gateway_status,
            gateway_stop,
            gateway_activation_plan,
            gateway_activation_cancel,
            gateway_activation_complete,
            gateway_quiesce_wait,
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
