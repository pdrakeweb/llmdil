//! Service core for the Local LLM Service Manager (spec §14).
//!
//! Extracted from the Tauri shell (admin API plan, phase 0): this crate
//! holds AppState and every operation implementation. The Tauri command
//! bridge (src-tauri) and the admin REST API (manager-api) are both thin
//! adapters over the operations registered in [`registry`].
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
    test_request, test_tool_call, BackoffPolicy, HealthProbe, HttpHealthProbe, LogLevelFilter, LogRecord,
    LogStream, ProcessSpec, ProcessState, Supervisor,
};
use manager_telemetry::{GpuInfo, GpuSample, NvidiaSmiSource, PollCadence, Poller, TelemetryStore};
use manager_wizard::{
    ops::{prev_log_path, RealSystemOps},
    steps::{continue_config_yaml, default_steps, merge_env_contents, VscodeExtensions},
    RunMode, StepOutcome, StepState, SystemOps, Wizard, WizardContext, WizardReport,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::{Mutex, OnceCell, RwLock};

pub mod activation;
pub mod activation_history;
pub mod app_update;
pub mod gateway_stats;
pub mod gpu_history;
pub mod notify;
pub mod optimize;
pub mod optimize_eval;
pub mod optimize_experiments;

pub use optimize::{
    op_measurement_delete, op_measurement_lookup, op_optimize_cancel_consult,
    op_optimize_cancel_job, op_optimize_consult, op_optimize_discard_plan, op_optimize_discover,
    op_optimize_estimate_backend, op_optimize_get_state, op_optimize_measure_backend,
    op_optimize_save_purpose, op_optimize_set_item_outcome, op_optimize_set_item_state,
    op_optimize_validate_plan,
};
pub use gpu_history::op_get_gpu_history;
pub use app_update::{
    op_app_update_check, op_app_update_download, op_app_update_restart, op_app_update_set_auto,
    op_app_update_set_checking, op_app_update_status,
};
pub use optimize_eval::op_optimize_run_baseline;
pub use optimize_experiments::{
    op_optimize_estimate_experiments, op_optimize_run_experiments, op_optimize_set_experiment_state,
};
pub mod registry;

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
// Event sink
// ---------------------------------------------------------------------------

/// The channel operations use to emit UI events. The desktop shell
/// supplies a Tauri-backed sink; the REST API supplies one that
/// forwards to its event stream; tests use [`NoopSink`]. Payloads are
/// the same JSON values the desktop events carried before the
/// extraction, so the frontend event contract is unchanged.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: &str, payload: Value);
    /// Show an OS toast notification (the desktop shell renders it
    /// through the notification plugin; headless sinks discard it).
    /// The notification module gates on the operator's settings
    /// before calling this; the error is for the Settings test
    /// action, which reports delivery failure.
    fn toast(&self, _title: &str, _body: &str) -> Result<(), String> {
        Ok(())
    }
}

/// An [`EventSink`] that discards everything.
pub struct NoopSink;

impl EventSink for NoopSink {
    fn emit(&self, _event: &str, _payload: Value) {}
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
    Services(String),
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
            ApiError::Services(e) => write!(f, "service error: {e}"),
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

pub type ApiResult = Result<Value, ApiError>;

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
    /// Persistent per-GPU telemetry history (see [`gpu_history`]):
    /// loaded from the data dir on first use, appended by the
    /// recorder loop, read by the history query op.
    gpu_history: OnceCell<std::sync::Mutex<gpu_history::GpuHistoryCell>>,
    wizard_install: OnceCell<Mutex<Wizard>>,
    wizard_audit: OnceCell<Mutex<Wizard>>,
    /// Last setup_get_steps payload, refreshed whenever the install
    /// wizard's state is read or mutated under its lock. Served when
    /// the wizard lock is held by a running step, so the checklist
    /// never sits blank for the duration of a step.
    wizard_steps_cache: RwLock<Option<Value>>,
    /// The install-wizard step currently executing (id + start
    /// instant), set by `run_wizard_step` just before the step runs
    /// and cleared when it returns. A caller that finds the wizard
    /// lock held reads this to report *what* is running and for how
    /// long instead of a bare "busy" — and the rejection is written
    /// to the wizard log, so the state is diagnosable afterwards.
    /// std Mutex: only synchronous set/clear/read.
    wizard_step_running: std::sync::Mutex<Option<(String, std::time::Instant)>>,
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
    /// Shared cancellation slot for the Optimize consultation (the
    /// design's one-consult-at-a-time rule): starting a consult
    /// stores its sender; firing it aborts the in-flight LLM call.
    optimize_abort: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    /// When the running consultation started (Unix ms), recorded
    /// beside the abort slot so the state payload can report a
    /// true elapsed time to any page instance — the page's own
    /// clock only knows about consultations it started itself.
    optimize_consult_started_unix_ms: Mutex<Option<i64>>,
    /// The last consultation's failure (message + Unix ms),
    /// recorded when a run ends in failure and cleared when a
    /// new run starts or one succeeds: a consultation that ran
    /// while the operator was on another view must still surface
    /// its failure on the Optimize page, not silently leave the
    /// previous plan looking current.
    optimize_consult_error: Mutex<Option<(String, i64)>>,
    /// Shared cancellation slot for GPU-bound Optimize jobs
    /// (Phase 5: measurements and experiment batches): one such job
    /// at a time, separate from the consult slot because a
    /// consultation and a measurement are different jobs with
    /// different lifetimes.
    optimize_job_abort: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    /// Transition-watch state for the toast notification monitor
    /// (see [`notify`]): backend states, gateway liveness
    /// expectation, and activation-plan pending state, so
    /// state-driven toasts fire on transitions only. std Mutex:
    /// synchronous diff/update only.
    notify_watch: std::sync::Mutex<notify::NotifyWatch>,
    /// Serializes app self-update work (check / stage / apply): a
    /// scheduled check, a manual check, and a Settings download can
    /// race on the same state file and staging directory otherwise.
    /// Held across the operation; the network calls involved are
    /// bounded (release list is one small JSON document, the
    /// installer ~8 MB).
    app_update_guard: Mutex<()>,
    /// Latest offload-probe state per backend id (see the probe run
    /// after a start that activates a saved offload change). std
    /// Mutex: synchronous insert/read only.
    offload_probes: std::sync::Mutex<std::collections::HashMap<String, OffloadProbe>>,
}

/// One backend's offload-probe state: a start that activated a saved
/// offload change runs a long-prompt probe against the new position
/// and records the outcome here for the backends list to surface.
#[derive(Debug, Clone)]
pub struct OffloadProbe {
    /// `probing` | `passed` | `failed`.
    state: String,
    /// Offload slider position (MoE layers on system RAM) probed.
    layers: u32,
    detail: Option<String>,
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

/// The Credential Manager target names whose credential is
/// PRESENT, across the Gateway-exposure MCP servers of the given
/// configurations — the arming predicate input for every drift
/// and journaling comparison (MCP design §4.2: presence, not
/// values; the accepted gap is documented on
/// [`manager_wizard::armed_gateway_mcp_servers`]). Callers
/// comparing a pre/post pair pass both configurations, so a
/// server removed by the change still resolves its arming state
/// for the pre-change side.
async fn mcp_credential_presence(
    cfgs: &[&AppConfig],
) -> std::collections::HashSet<String> {
    let ops = RealSystemOps::default();
    let mut present: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut targets: Vec<&str> = Vec::new();
    for cfg in cfgs {
        for server in cfg
            .mcp
            .servers
            .iter()
            .filter(|s| s.exposure == manager_config::McpExposure::Gateway)
        {
            if let Some(target) = server.cred_ref.as_deref().filter(|r| !r.is_empty()) {
                if !targets.contains(&target) {
                    targets.push(target);
                }
            }
        }
    }
    for target in targets {
        if ops.credential_exists(target).await.unwrap_or(false) {
            present.insert(target.to_string());
        }
    }
    present
}

impl AppState {
    /// The app's data directory (config, logs, and the API's
    /// stores live under it). Exposed for the desktop shell's
    /// server-lifecycle commands.
    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    pub fn new(config: AppConfig, config_path: PathBuf, data_dir: PathBuf) -> Self {
        Self {
            config: RwLock::new(config),
            config_path,
            data_dir,
            supervisor: OnceCell::new(),
            telemetry: OnceCell::new(),
            telemetry_poller: OnceCell::new(),
            gpu_history: OnceCell::new(),
            wizard_install: OnceCell::new(),
            wizard_audit: OnceCell::new(),
            wizard_steps_cache: RwLock::new(None),
            wizard_step_running: std::sync::Mutex::new(None),
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
            optimize_abort: Mutex::new(None),
            optimize_consult_started_unix_ms: Mutex::new(None),
            optimize_consult_error: Mutex::new(None),
            optimize_job_abort: Mutex::new(None),
            notify_watch: std::sync::Mutex::new(notify::NotifyWatch::default()),
            app_update_guard: Mutex::new(()),
            offload_probes: std::sync::Mutex::new(std::collections::HashMap::new()),
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
        // Live step progress goes straight to wizard.log: the step
        // outcome's own log is only persisted when the step returns,
        // so a multi-minute step (a cold package install, a database
        // wait) otherwise leaves the file silent between "started"
        // and the outcome block and reads as a hang.
        let log_data_dir = self.data_dir.clone();
        let log_sink: manager_wizard::StepLogSink = Arc::new(move |line: String| {
            append_wizard_log(
                &log_data_dir,
                &[format!("[{}] {line}", chrono::Utc::now().to_rfc3339())],
            );
        });
        WizardContext {
            config: self.config.read().await.clone(),
            data_dir: self.data_dir.clone(),
            mode,
            ops: Arc::new(RealSystemOps::default()),
            log_sink: Some(log_sink),
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
                let source = manager_telemetry::default_telemetry_stack();
                let (poller, store) = Poller::new(source, cadence);
                let handle = poller.start();
                let _ = self.telemetry_poller.set(std::sync::Mutex::new(handle));
                // Static GPU inventory (index, UUID, driver model)
                // for launch construction: CUDA_VISIBLE_DEVICES is
                // expressed in GPU UUIDs (see backend_spec). The
                // query is BOUNDED and retried: it previously ran
                // unbounded inside this OnceCell init, so a hung
                // nvidia-smi (a wedged driver call is exactly when
                // inventory matters) stalled every telemetry caller
                // for the whole session, and a transient failure
                // cached an empty inventory forever — launches then
                // stayed unrestricted with no signal anywhere
                // (code-review 4.2). Spec building treats an empty
                // inventory as "cannot determine" and leaves
                // launches unrestricted, so a late-arriving
                // inventory only improves later launches.
                let smi = NvidiaSmiSource::default();
                let attempt = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    smi.run_query(),
                )
                .await;
                let loaded = match attempt {
                    Ok(Ok(xml)) => match NvidiaSmiSource::parse_inventory(&xml) {
                        Ok(inventory) if !inventory.is_empty() => {
                            lock_telemetry(&store).set_inventory(inventory);
                            true
                        }
                        Ok(_) => {
                            eprintln!("gpu inventory: nvidia-smi reported no GPUs");
                            false
                        }
                        Err(e) => {
                            eprintln!("gpu inventory: could not parse nvidia-smi output: {e}");
                            false
                        }
                    },
                    Ok(Err(e)) => {
                        eprintln!("gpu inventory: nvidia-smi query failed: {e}");
                        false
                    }
                    Err(_) => {
                        eprintln!("gpu inventory: nvidia-smi query timed out after 10s");
                        false
                    }
                };
                if !loaded {
                    let store_retry = store.clone();
                    tokio::spawn(async move {
                        for delay_secs in [5u64, 15, 45] {
                            tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
                            let smi = NvidiaSmiSource::default();
                            let attempt = tokio::time::timeout(
                                std::time::Duration::from_secs(10),
                                smi.run_query(),
                            )
                            .await;
                            if let Ok(Ok(xml)) = attempt {
                                if let Ok(inventory) = NvidiaSmiSource::parse_inventory(&xml) {
                                    if !inventory.is_empty() {
                                        lock_telemetry(&store_retry)
                                            .set_inventory(inventory);
                                        eprintln!(
                                            "gpu inventory: loaded on retry after {delay_secs}s"
                                        );
                                        return;
                                    }
                                }
                            }
                            eprintln!(
                                "gpu inventory: retry after {delay_secs}s did not load an inventory"
                            );
                        }
                        eprintln!(
                            "gpu inventory: giving up after retries; launches stay unrestricted until the next app start"
                        );
                    });
                }
                store
            })
            .await
            .clone()
    }

    /// The persistent GPU history cell. The first call loads the
    /// history file from the data dir (missing/corrupt = empty);
    /// the recorder appends and the history op reads through the
    /// same cell, so the file's in-memory image is single-owned.
    async fn gpu_history(&self) -> &std::sync::Mutex<gpu_history::GpuHistoryCell> {
        self.gpu_history
            .get_or_init(|| async {
                std::sync::Mutex::new(gpu_history::GpuHistoryCell {
                    store: gpu_history::load_history(&self.data_dir),
                    last_saved_ms: 0,
                })
            })
            .await
    }

    /// Transport setup can only fail on local client misconfiguration;
    /// the error is surfaced to the caller instead of panicking.
    async fn winml(&self) -> Result<&Mutex<manager_winml::WinMlRegistry>, ApiError> {
        if let Some(r) = self.winml.get() {
            return Ok(r);
        }
        let transport = manager_winml::ReqwestProbeTransport::new(Duration::from_secs(10))
            .map_err(|e| ApiError::WinMl(format!("winml transport setup failed: {e:?}")))?;
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

    /// The full gateway drift check for hydration/restore: the
    /// on-disk YAML matches the current configuration AND the
    /// recorded spawn fingerprint matches the current spawn inputs
    /// (port, bind, PostgreSQL presence, credential hashes —
    /// code-review 2.3). The fingerprint is computed from the
    /// credentials as read NOW; a credential that cannot be read
    /// hashes as absent, which cannot match a recorded spawn that
    /// had one — unreadable therefore reads as drifted, the safe
    /// direction.
    pub async fn gateway_config_current(&self, cfg: &AppConfig) -> bool {
        let ops = RealSystemOps::default();
        let postgres = manager_wizard::steps::postgres_installed(&ops, &self.data_dir).await;
        let master = ops
            .credential_read(manager_wizard::LITELLM_MASTER_KEY_CRED_TARGET)
            .await;
        let tier = &cfg.gateway.openrouter;
        let openrouter = if tier.enabled {
            match tier.cred_ref.as_deref().filter(|r| !r.is_empty()) {
                Some(r) => ops.credential_read(r).await,
                None => None,
            }
        } else {
            None
        };
        // MCP arming for the drift path (MCP design §4.2, Phase 2):
        // resolved by credential PRESENCE (credential_exists), not
        // by reading values — the one arming rule in manager-wizard
        // documents this accepted gap (the fingerprint hash covers
        // the cred_ref name, never the value, so presence determines
        // set membership unless a credential exists but is
        // unreadable). Presence results are gathered async here and
        // handed to the sync helpers as a lookup set.
        let mcp_present = mcp_credential_presence(&[cfg]).await;
        let mcp_armed = manager_wizard::armed_gateway_mcp_servers(cfg, |t| {
            mcp_present.contains(t)
        });
        let fingerprint = manager_wizard::gateway_spawn_fingerprint(
            cfg,
            postgres,
            master.as_deref(),
            openrouter.as_deref(),
            &mcp_armed,
        );
        manager_wizard::gateway_config_is_current(cfg, &self.data_dir, &fingerprint, |t| {
            mcp_present.contains(t)
        })
    }

    /// The configured supervision mode (process | service).
    async fn supervision_mode(&self) -> manager_config::SupervisionMode {
        self.config.read().await.supervision.mode
    }

    /// Whether lifecycle routing for `service_name` goes to the SCM:
    /// the global mode is Service AND the service is registered.
    /// Registration is the per-service opt-in, so an unregistered
    /// backend in service mode keeps process supervision (mixed mode;
    /// see docs/windows-services-design.md). SCM queries are
    /// blocking; they run on the blocking pool.
    async fn service_route_active(&self, service_name: &str) -> bool {
        if self.supervision_mode().await != manager_config::SupervisionMode::Service {
            return false;
        }
        let name = service_name.to_string();
        tokio::task::spawn_blocking(move || {
            use manager_services::ServiceControl;
            manager_services::scm::platform_control()
                .is_registered(&name)
                .unwrap_or(false)
        })
        .await
        .unwrap_or(false)
    }

    /// Regenerate a backend's service definition from the supervisor's
    /// registered spawn spec — the same spec object process mode
    /// spawns — and write it to `<data_dir>/services/`. Returns the
    /// definition path.
    /// Regenerate a backend's service definition from the supervisor's
    /// registered spawn spec — the same spec object process mode
    /// spawns — and write it to `<data_dir>/services/`. Returns the
    /// definition path.
    async fn write_backend_service_definition(
        &self,
        backend_id: &str,
    ) -> Result<std::path::PathBuf, String> {
        ensure_backend_specs(self).await;
        let spec = {
            let sup = self.supervisor().await;
            let sup = sup.lock().await;
            sup.spec(backend_id).cloned()
        };
        let spec = spec.ok_or_else(|| format!("backend {backend_id} has no spawn spec"))?;
        let def = manager_services::backend_definition(&spec, &self.data_dir.join("logs"));
        manager_services::write_definition(&self.data_dir, &def).map_err(|e| e.to_string())
    }

    /// Definition sync for the config-save paths: regenerate the
    /// backend's service definition when service mode is on. A sync
    /// failure is written to the wizard log, never silently dropped —
    /// and the start path regenerates as well and fails visibly there,
    /// so a stale definition cannot quietly reach a service start.
    async fn sync_backend_service_definition(&self, backend_id: &str) {
        if self.supervision_mode().await != manager_config::SupervisionMode::Service {
            return;
        }
        if let Err(e) = self.write_backend_service_definition(backend_id).await {
            append_wizard_log(
                &self.data_dir,
                &[format!(
                    "[{}] service definition sync for backend {backend_id} failed: {e}",
                    chrono::Utc::now().to_rfc3339()
                )],
            );
        }
    }

    /// Reconcile the persisted `setup_completed` flag with the
    /// install wizard's actual state. The flag was historically set
    /// only by the wizard view's Finish button (`setup_mark_complete`);
    /// every other completion path — a full "Run setup" run, per-step
    /// runs, skip / mark-manual, and launch-time hydration from live
    /// machine checks — left it false, so a user with a functionally
    /// complete setup was routed back to the wizard on every launch
    /// (field-confirmed 2026-10-17: every step passing, flag false).
    /// When the wizard's report shows every step Done, Manual, or
    /// Skipped — the Finish dialog's own completeness predicate — the
    /// flag is set and persisted, so launch routing (the UI reads the
    /// flag via `setup_status`) sends the user to the Dashboard.
    /// Best effort: a failed save keeps the in-memory value and the
    /// next trigger retries.
    pub async fn reconcile_setup_completed(&self) {
        if self.config.read().await.setup_completed {
            return;
        }
        let complete = {
            let wizard = self.wizard(RunMode::Install).await;
            let guard = wizard.lock().await;
            let report = guard.report();
            !report.steps.is_empty()
                && report.steps.iter().all(|s| {
                    matches!(
                        s.state,
                        manager_wizard::StepState::Done
                            | manager_wizard::StepState::Manual
                            | manager_wizard::StepState::Skipped
                    )
                })
        };
        if !complete {
            return;
        }
        let mut cfg = self.config.write().await;
        if cfg.setup_completed {
            return;
        }
        let mut new_cfg = cfg.clone();
        new_cfg.setup_completed = true;
        match manager_config::save(&new_cfg, &self.config_path) {
            Ok(()) => {
                *cfg = new_cfg;
                append_wizard_log(
                    &self.data_dir,
                    &[format!(
                        "[{}] setup_completed reconciled: every wizard step is done/manual/skipped, so the flag was set without a Finish click",
                        chrono::Utc::now().to_rfc3339()
                    )],
                );
            }
            Err(e) => {
                append_wizard_log(
                    &self.data_dir,
                    &[format!(
                        "[{}] setup_completed reconcile could not persist: {e}",
                        chrono::Utc::now().to_rfc3339()
                    )],
                );
            }
        }
    }

    /// The persisted MCP registry (`AppConfig.mcp`) — the source of
    /// truth since MCP Phase 1 (design §3/§10.1). The session-only
    /// `mcp_modes` overlay is deleted; exposure is persisted.
    async fn mcp_registry(&self) -> Vec<manager_config::McpServerConfig> {
        self.config.read().await.mcp.servers.clone()
    }


    /// Seed the install wizard's checklist from the machine's actual
    /// state: run the audit wizard's checks once and adopt the passing
    /// outcomes, so the checklist does not reset to all-Queued on
    /// every launch. Runs in the background at startup; best-effort —
    /// the wizard stays usable (in its default state) while it runs.
    pub async fn hydrate_wizard(&self) {
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
        // The audit's start-gateway pass only proves a gateway
        // answers; it is adopted as the install step's completion
        // only while the on-disk gateway config still matches the
        // current configuration (see Wizard::hydrate).
        let gateway_current = self.gateway_config_current(&ctx.config).await;
        guard.hydrate(&report, gateway_current);
        *self.wizard_steps_cache.write().await = Some(wizard_steps_payload(&guard));
    }

    /// Persist the install wizard's current report to
    /// `wizard-state.json` (atomic tmp + rename). Best effort: a
    /// failed write must never fail the step that triggered it.
    pub async fn persist_wizard(&self) {
        let wizard = self.wizard(RunMode::Install).await;
        let report = {
            let guard = wizard.lock().await;
            guard.report()
        };
        let persisted = PersistedWizard { version: 1, report };
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
    pub async fn restore_wizard(&self) {
        let Ok(text) = std::fs::read_to_string(self.data_dir.join("wizard-state.json")) else {
            return;
        };
        let Ok(persisted) = serde_json::from_str::<PersistedWizard>(&text) else {
            return;
        };
        // A step persisted as Running was in flight when the app
        // closed: its outcome was never written (outcomes land on
        // completion). Mark the interruption in the wizard log so
        // the log shows why there is a "started" with no outcome —
        // the gap that made the 2026-10-16 half-repaired gateway
        // undiagnosable from the bundle alone.
        for s in &persisted.report.steps {
            if s.state == StepState::Running {
                append_wizard_log(
                    &self.data_dir,
                    &[format!(
                        "[{}] === step {} (install): interrupted — the app closed while this step was running; re-run it to complete ===",
                        chrono::Utc::now().to_rfc3339(),
                        s.id
                    )],
                );
            }
        }
        let wizard = self.wizard(RunMode::Install).await;
        let mut guard = wizard.lock().await;
        // Same drift rule as hydration: a persisted start-gateway
        // Done is adopted only while the gateway config on disk
        // still matches the current configuration.
        let cfg = self.config.read().await.clone();
        let gateway_current = self.gateway_config_current(&cfg).await;
        guard.restore(&persisted.report, gateway_current);
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

/// Resolve a config path against the data dir (absolute paths pass
/// through). Config model paths are relative (`models/{id}.gguf`); every
/// filesystem operation must go through this so it agrees with the path
/// list_models probes and download_model writes.
fn resolve_under_data_dir(data_dir: &std::path::Path, path: &std::path::Path) -> PathBuf {
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
        let entries: Vec<manager_config::CatalogModelEntry> = manager_models::curated_models()
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
        // Definition replacements ride the same revision gate:
        // when a default model's catalog pin changed, the stored
        // definition is refreshed and the serving change that goes
        // with it is applied (see
        // manager_config::refresh_catalog_definition). Revision 2:
        // the planner's Qwen3.8-27B is GPU-resident, so the split
        // the Qwen3-32B needed is cleared; the coder's
        // Qwen3-Coder-Next is served by expert offload instead of
        // a tensor split.
        for entry in &entries {
            let Some(backend_id) = backend_for(&entry.model_id) else {
                continue;
            };
            let (clear_split, raw_flags) = match entry.model_id.as_str() {
                "qwen3-27b-planner" => (true, None),
                // The coder's riding serving change is its catalog
                // default offload (all experts) in canonical form.
                "qwen3-coder-30b" => (true, catalog_default_raw_flags(&entry.model_id)),
                _ => (false, None),
            };
            if !clear_split && raw_flags.is_none() {
                continue;
            }
            manager_config::refresh_catalog_definition(
                &mut cfg,
                &manager_config::CatalogDefinitionRefresh {
                    entry: entry.clone(),
                    backend_id: backend_id.to_string(),
                    clear_split,
                    raw_flags,
                },
            );
        }
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
                verified_sha256: None,
                source_url: spec.url,
                assigned_backends,
                enabled: spec.enabled_by_default,
                display_name: None,
                moe_layers: None,
                default_offload_layers: None,
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
            if backend.model_file == format!("models/{backend_id}.gguf") {
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
pub async fn op_get_status(state: &Arc<AppState>, _sink: &Arc<dyn EventSink>) -> ApiResult {
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
    manager_config::save(&new_cfg, &state.config_path)
        .map_err(ApiError::from)?;
    *cfg = new_cfg;
    Ok(json!({ "ok": true, "persisted": true }))
}


// ---------------------------------------------------------------------------
// Backends
// ---------------------------------------------------------------------------

/// GPU visibility for one backend launch.
///
/// llama.cpp's CUDA backend creates a context on **every visible GPU**
/// at startup (~256 MiB each on Windows), so a backend placed on one
/// GPU permanently wastes that much VRAM on every other GPU — VRAM
/// the placed models cannot use (Pete's machines: GPU memory is the
/// scarce resource, system RAM is not). Launches are therefore
/// restricted with `CUDA_VISIBLE_DEVICES` to exactly the placement's
/// GPUs, expressed as UUIDs (stable across index shifts).
#[derive(Debug, Clone, PartialEq, Eq)]
enum GpuVisibility {
    /// Raw flags pin GPU layers to 0: the backend loads nothing on
    /// any GPU. `CUDA_VISIBLE_DEVICES` is set empty, so no context
    /// is created anywhere.
    CpuOnly,
    /// Restrict to these GPU indices (the placement's GPUs, in order).
    Gpus(Vec<u32>),
    /// Placement cannot be determined from the config (a configured
    /// split with no positive entry — possible only in a hand-edited
    /// config, validation requires sum 1.0). The launch is left
    /// exactly as it was: no env restriction, no flag remapping.
    Undetermined,
}

/// The device set a backend's raw flags select, when they carry
/// device selection at all (`--main-gpu`, `--device`/`-dev`,
/// `--tensor-split`/`-ts`, in either spelling). `Unparseable`
/// covers a selection the parser cannot fully resolve (a device
/// entry that is not `CUDA<n>`, a non-numeric main GPU, a split
/// entry that is not a number): the selection exists but its set
/// cannot be determined.
enum RawDeviceSelection {
    None,
    Set(std::collections::BTreeSet<u32>),
    Unparseable,
}

fn raw_device_selection(raw: Option<&str>) -> RawDeviceSelection {
    let Some(raw) = raw else {
        return RawDeviceSelection::None;
    };
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    let mut set = std::collections::BTreeSet::new();
    let mut any = false;
    let mut i = 0;
    while i < tokens.len() {
        let (flag, inline) = match tokens[i].split_once('=') {
            Some((f, v)) => (f, Some(v)),
            None => (tokens[i], None),
        };
        let value = inline.or_else(|| tokens.get(i + 1).copied());
        match flag {
            "--main-gpu" | "-mg" => {
                any = true;
                match value.and_then(|v| v.parse::<u32>().ok()) {
                    Some(n) => {
                        set.insert(n);
                    }
                    None => return RawDeviceSelection::Unparseable,
                }
            }
            "--device" | "-dev" => {
                any = true;
                let Some(v) = value else {
                    return RawDeviceSelection::Unparseable;
                };
                for entry in v.split(',') {
                    match entry
                        .trim()
                        .strip_prefix("CUDA")
                        .and_then(|n| n.parse::<u32>().ok())
                    {
                        Some(n) => {
                            set.insert(n);
                        }
                        None => return RawDeviceSelection::Unparseable,
                    }
                }
            }
            "--tensor-split" | "-ts" => {
                any = true;
                let Some(v) = value else {
                    return RawDeviceSelection::Unparseable;
                };
                for (idx, entry) in v.split(',').enumerate() {
                    match entry.trim().parse::<f32>() {
                        Ok(f) if f > 0.0 => {
                            set.insert(idx as u32);
                        }
                        Ok(_) => {}
                        Err(_) => return RawDeviceSelection::Unparseable,
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    if any {
        RawDeviceSelection::Set(set)
    } else {
        RawDeviceSelection::None
    }
}

/// The backend's GPU placement, from its configuration alone: the
/// configured tensor split's positive entries (placement_apply is
/// what writes splits, so the split IS the placement record); an
/// unsplit backend loads on the main GPU (index 0), llama.cpp's
/// default; raw flags pinning GPU layers to 0 mean no GPU placement.
fn backend_gpu_visibility(b: &manager_config::BackendConfig) -> GpuVisibility {
    if raw_flags_pin_cpu_only(b.raw_flags.as_deref()) {
        return GpuVisibility::CpuOnly;
    }
    if b.flags.tensor_split.is_empty() {
        // The GPU 0 default is an assumption about an unsplit
        // launch — void when the operator's raw flags carry their
        // own device selection. Remapping such a selection into
        // the assumed {0} silently moved the backend to GPU 0
        // (the raw --main-gpu/--device was dropped and visibility
        // restricted to GPU 0's UUID), so a selection that is not
        // exactly {0} makes the placement indeterminable and the
        // launch goes out unrestricted, raw flags verbatim
        // (code-review 4.4). A structured split, when present,
        // remains the source of truth and raw selection yields to
        // it, as before.
        match raw_device_selection(b.raw_flags.as_deref()) {
            RawDeviceSelection::None => {}
            RawDeviceSelection::Set(set) if set.len() == 1 && set.contains(&0) => {}
            _ => return GpuVisibility::Undetermined,
        }
        return GpuVisibility::Gpus(vec![0]);
    }
    let placed: Vec<u32> = b
        .flags
        .tensor_split
        .iter()
        .enumerate()
        .filter(|(_, f)| **f > 0.0)
        .map(|(i, _)| i as u32)
        .collect();
    if placed.is_empty() {
        GpuVisibility::Undetermined
    } else {
        GpuVisibility::Gpus(placed)
    }
}

/// True when raw flags set GPU layers to 0 (`-ngl 0`,
/// `--gpu-layers 0`, `--n-gpu-layers=0`, ...): the backend is
/// CPU-only. Expert offload (`-ot exps=CPU`) is NOT this — the
/// non-expert weights still load on the GPU.
///
/// llama.cpp applies the LAST occurrence of a repeated flag, so
/// the last `-ngl`-family value decides: `-ngl 99 -ngl 0` is
/// CPU-only and `-ngl 0 -ngl 99` is not. The first-match reading
/// this replaces classified the second form CPU-only and launched
/// it with an empty CUDA_VISIBLE_DEVICES (code-review 4.7).
fn raw_flags_pin_cpu_only(raw: Option<&str>) -> bool {
    let Some(raw) = raw else { return false };
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    let mut last: Option<&str> = None;
    let mut i = 0;
    while i < tokens.len() {
        let (flag, inline) = match tokens[i].split_once('=') {
            Some((f, v)) => (f, Some(v)),
            None => (tokens[i], None),
        };
        if matches!(flag, "-ngl" | "--gpu-layers" | "--n-gpu-layers") {
            if let Some(value) = inline.or_else(|| tokens.get(i + 1).copied()) {
                last = Some(value);
            }
        }
        i += 1;
    }
    last == Some("0")
}

/// Position of global GPU index `v` within the visible list, if the
/// GPU is placed there.
fn visible_position(v: &str, placed: &[u32]) -> Option<usize> {
    let idx: u32 = v.parse().ok()?;
    placed.iter().position(|p| *p == idx)
}

/// Remap a llama.cpp `--device` list (`CUDA0,CUDA2`) onto the visible
/// subset: entries naming placed GPUs become their visible positions;
/// entries naming other GPUs are dropped; entries that do not parse
/// as `CUDA<n>` pass through unchanged. `None` when nothing survives.
fn remap_device_list(v: &str, placed: &[u32]) -> Option<String> {
    let mut out: Vec<String> = Vec::new();
    for entry in v.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        match entry
            .strip_prefix("CUDA")
            .and_then(|n| n.parse::<u32>().ok())
        {
            Some(num) => {
                if let Some(pos) = placed.iter().position(|p| *p == num) {
                    out.push(format!("CUDA{pos}"));
                }
            }
            None => out.push(entry.to_string()),
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out.join(","))
    }
}

/// Build the supervisor process spec for one configured backend: the
/// wizard-installed llama-server binary, the configured model (relative
/// paths resolve under the data dir), port, and structured flags.
///
/// The launch is GPU-restricted (see [`GpuVisibility`]): when the
/// placement resolves and every placed GPU's UUID is known from
/// `gpus` (the telemetry inventory), `CUDA_VISIBLE_DEVICES` carries
/// exactly the placed GPUs' UUIDs and the device-selection flags are
/// remapped onto the visible subset — the structured `--tensor-split`
/// keeps only the placed GPUs' proportions in placement order, and
/// operator-supplied `--tensor-split` / `--main-gpu` / `--device` in
/// raw flags are remapped or dropped against the same placement
/// (placement is the source of truth; placement_apply writes splits).
/// When a placed GPU's UUID is unknown (GPU absent from the
/// inventory), the launch is left unrestricted rather than guessing.
fn backend_spec(
    cfg: &AppConfig,
    b: &manager_config::BackendConfig,
    gpus: &[GpuInfo],
) -> ProcessSpec {
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
        // --jinja is explicit for every backend (MCP design §7.1):
        // structured tool calls depend on the Jinja chat template,
        // and the pinned llama.cpp build enables it by default —
        // restating it makes the precondition audit-visible and
        // pin-independent instead of relying on an upstream default.
        "--jinja".into(),
    ];

    // The visible GPU indices (empty for CPU-only) when the launch is
    // restricted; None when it is not (undetermined placement, or a
    // placed GPU missing from the inventory).
    let visible: Option<Vec<u32>> = match backend_gpu_visibility(b) {
        GpuVisibility::CpuOnly => Some(Vec::new()),
        GpuVisibility::Gpus(indices)
            if indices
                .iter()
                .all(|i| gpus.iter().any(|g| g.index == *i && !g.uuid.is_empty())) =>
        {
            Some(indices)
        }
        _ => None,
    };
    let mut env: Vec<(String, String)> = Vec::new();
    if let Some(placed) = &visible {
        let uuids = placed
            .iter()
            .map(|i| {
                gpus.iter()
                    .find(|g| g.index == *i)
                    .map(|g| g.uuid.clone())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join(",");
        env.push(("CUDA_VISIBLE_DEVICES".into(), uuids));
    }

    // Structured tensor split: remapped onto the visible subset when
    // restricted (placed GPUs' proportions only, in placement order);
    // dropped for a CPU-only launch (a split over no visible devices
    // is meaningless); verbatim when unrestricted.
    let split_arg: Option<String> = if b.flags.tensor_split.is_empty() {
        None
    } else {
        match &visible {
            Some(placed) if !placed.is_empty() => Some(
                placed
                    .iter()
                    .map(|i| b.flags.tensor_split[*i as usize].to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            Some(_) => None,
            None => Some(
                b.flags
                    .tensor_split
                    .iter()
                    .map(|f| f.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        }
    };
    if let Some(split) = split_arg {
        args.push("--tensor-split".into());
        args.push(split);
    }
    args.push("--split-mode".into());
    args.push(split_mode_str(b.flags.split_mode).into());
    if let Some(raw) = &b.raw_flags {
        // raw_flags are operator-supplied llama-server arguments, with
        // three reservations:
        // - --host is filtered out (both "--host X" and "--host=X").
        //   Backends bind loopback; the LiteLLM gateway is the only
        //   network entry point, and an unauthenticated llama-server
        //   bound to the LAN by a stray flag would bypass it.
        // - --no-jinja is filtered out (both spellings, bare tokens
        //   with no value, plus the "--jinja=false" form). --jinja is
        //   app-owned (see the structured args above): it is the
        //   precondition for structured tool calling (MCP design
        //   §7.1), so an operator flag must not silently disable it.
        // - When the launch is GPU-restricted, device-selection flags
        //   are remapped onto the visible subset (placement is the
        //   source of truth — see the fn docs).
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
            if t == "--no-jinja" || t == "--no_jinja" || t == "--jinja=false" {
                i += 1; // bare token, no value to skip
                continue;
            }
            if let Some(placed) = &visible {
                if t == "--tensor-split" {
                    i += 2; // the structured split above is authoritative
                    continue;
                }
                if t.starts_with("--tensor-split=") {
                    i += 1;
                    continue;
                }
                if t == "--main-gpu" {
                    if let Some(pos) = tokens.get(i + 1).and_then(|v| visible_position(v, placed)) {
                        args.push("--main-gpu".into());
                        args.push(pos.to_string());
                    }
                    i += 2;
                    continue;
                }
                if let Some(v) = t.strip_prefix("--main-gpu=") {
                    if let Some(pos) = visible_position(v, placed) {
                        args.push("--main-gpu".into());
                        args.push(pos.to_string());
                    }
                    i += 1;
                    continue;
                }
                if t == "--device" || t == "-dev" {
                    if let Some(mapped) =
                        tokens.get(i + 1).and_then(|v| remap_device_list(v, placed))
                    {
                        args.push(t.to_string());
                        args.push(mapped);
                    }
                    i += 2;
                    continue;
                }
                if let Some((flag, v)) = t
                    .split_once('=')
                    .filter(|(f, _)| *f == "--device" || *f == "-dev")
                {
                    if let Some(mapped) = remap_device_list(v, placed) {
                        args.push(flag.to_string());
                        args.push(mapped);
                    }
                    i += 1;
                    continue;
                }
            }
            args.push(t.to_string());
            i += 1;
        }
    }
    ProcessSpec {
        id: b.id.clone(),
        program,
        args,
        env,
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
    // GPU inventory (index -> UUID) for launch construction. The
    // config read above has completed and dropped its guard, so
    // touching the telemetry accessor here cannot nest (it reads the
    // config while initializing) — the same rule placement_propose
    // follows. Inventory is populated once at telemetry init.
    let store = state.telemetry().await;
    let gpus = lock_telemetry(&store).inventory();
    let mut sup = state.supervisor().await.lock().await;
    for b in &cfg.backends {
        sup.register_spec(backend_spec(&cfg, b, &gpus));
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
async fn backend_start_preflight(state: &AppState, id: &str) -> Result<(), ApiError> {
    let cfg = state.config.read().await.clone();
    let b = cfg
        .backends
        .iter()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    backend_start_preflight_for(state, &cfg, b).await
}

/// The preflight itself, against an explicit config + backend
/// rather than a lookup in the saved configuration. The Optimize
/// experiment runner gates its temporary backends through this:
/// a temp backend never enters the saved config, but its spawn
/// must pass the same gate as any app-mediated start
/// (code-review B-2) — most importantly the offload guardrail,
/// evaluated against the cloned config that carries the temp
/// backend.
pub(crate) async fn backend_start_preflight_for(
    state: &AppState,
    cfg: &AppConfig,
    b: &manager_config::BackendConfig,
) -> Result<(), ApiError> {
    if !b.enabled {
        return Err(ApiError::InvalidInput(format!(
            "backend {} is disabled in the configuration; enable it before starting",
            b.id
        )));
    }
    reap_downloads(state).await;
    let resolved_model = resolve_under_data_dir(&cfg.data_dir, &b.model_file);
    if let Some(m) = cfg
        .models
        .iter()
        .find(|m| resolve_under_data_dir(&cfg.data_dir, &m.gguf_path) == resolved_model)
    {
        if state.downloads.lock().await.contains_key(&m.id) {
            return Err(ApiError::InvalidInput(format!(
                "model {} is still downloading; a backend started now would load a truncated file",
                m.id
            )));
        }
        if state.verifying_downloads.lock().unwrap().contains(&m.id) {
            return Err(ApiError::InvalidInput(format!(
                "SHA-256 verification is still running for model {}.",
                m.id
            )));
        }
        // A recorded verification failure gates start until the model
        // is re-downloaded or passes a manual verify: the file that
        // came down did not match its pinned hash.
        if let Some(outcome) = state.download_outcomes.lock().await.get(&m.id) {
            if outcome.verification_failure {
                return Err(ApiError::InvalidInput(outcome.failed.clone().unwrap_or_else(
                    || {
                        format!(
                            "model {} has not passed SHA-256 verification — open Models and run Verify.",
                            m.id
                        )
                    },
                )));
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
            return Err(ApiError::InvalidInput(format!(
                "model {} has not passed SHA-256 verification — open Models and run Verify.",
                m.id
            )));
        }
    }
    // Offload guardrail (tracker #37): a saved position a placed
    // GPU's restriction blocks refuses the start unless a journaled
    // override covers exactly that position on exactly that
    // placement. Callers pass a cloned config and hold no config
    // guard, so the telemetry accessor (which reads the config
    // while initializing) is safe to call.
    {
        let store = state.telemetry().await;
        let gpus = lock_telemetry(&store).inventory();
        drop(store);
        let journal = activation::load_journal(&state.data_dir);
        if let Some(eval) = evaluate_backend_offload(cfg, b, &gpus, &journal) {
            if let Some(reason) = offload_block_reason(
                &eval.restrictions,
                eval.layers,
                eval.moe_layers,
                &eval.placed_ccs,
                eval.cpu_only,
                eval.overridden,
            ) {
                return Err(ApiError::InvalidInput(reason));
            }
        }
    }
    // Only the program path is consumed here, so the spec is built
    // without GPU inventory (its env/args are discarded).
    let spec = backend_spec(cfg, b, &[]);
    if !spec.program.exists() {
        return Err(ApiError::InvalidInput(format!(
            "llama-server is not installed (missing {}) — run the setup wizard to install llama.cpp",
            spec.program.display()
        )));
    }
    let model_path = if b.model_file.is_absolute() {
        b.model_file.clone()
    } else {
        cfg.data_dir.join(&b.model_file)
    };
    if !model_path.exists() {
        return Err(ApiError::InvalidInput(format!(
            "model file is missing ({}) — download it in the Models view",
            model_path.display()
        )));
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
pub async fn op_list_backends(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    adopt_orphan_backends(state).await;
    // Clone and drop the config guard here: the offload guardrail
    // block below needs the telemetry inventory and the activation
    // journal, and the telemetry accessor reads the config while
    // initializing — it must never be called while this command
    // holds the config guard (the rule ensure_backend_specs follows).
    let cfg = state.config.read().await.clone();
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
                "tensor_split": b.flags.tensor_split,
                // The saved raw flags and the advanced flags mode
                // (tracker #60): the card renders the string itself
                // in the mode, and the flags panel pre-fills its
                // editor from it.
                "raw_flags": b.raw_flags,
                "flags_advanced": b.flags_advanced,
                "group": cfg.gateway.groups.iter()
                    .find(|g| g.members.iter().any(|m| m == &b.id))
                    .map(|g| g.name.clone()),
                "state": st.map(|s| process_state_str(s.state)).unwrap_or("stopped"),
                "pid": st.and_then(|s| s.pid),
                "adopted": st.map(|s| s.adopted).unwrap_or(false),
                "restarts": st.map(|s| s.restarts).unwrap_or(0),
                "uptime_secs": st.and_then(|s| s.uptime).map(|u| u.as_secs()),
                // The persisted tool-call probe (MCP design §7.2):
                // {outcome: verified|unparsed|failed, at_ms} or null
                // when the backend has never been probed — the UI
                // renders null as "not measured", never as a state.
                "tool_call_probe": serde_json::to_value(b.tool_call_probe).unwrap_or(Value::Null),
            })
        })
        .collect();
    // Memory requirements per configured backend (GPU-resident
    // weights, system-RAM weights under expert offload, KV at the
    // configured context, and the derivation basis). Absent when
    // the backend's model cannot be resolved.
    {
        let memory: HashMap<String, manager_models::MemoryRequirement> = cfg
            .backends
            .iter()
            .filter_map(|b| backend_memory_requirement(&cfg, b).map(|r| (b.id.clone(), r)))
            .collect();
        for row in backends.iter_mut() {
            let Some(id) = row["id"].as_str() else {
                continue;
            };
            let Some(req) = memory.get(id) else { continue };
            row["gpu_ram_mib"] = json!(req.gpu_weights_mib);
            row["sys_ram_mib"] = json!(req.sys_weights_mib);
            row["kv_ram_mib"] = json!(req.kv_mib);
            row["memory_basis"] = serde_json::to_value(req.basis).unwrap_or(Value::Null);
        }
    }
    // Expert-offload slider state per configured backend whose
    // model is offload-capable (see backend_offload_info). Absent
    // for every other backend — the Edit Flags form shows the
    // slider only when this is present.
    {
        let offload: HashMap<String, OffloadInfo> = cfg
            .backends
            .iter()
            .filter_map(|b| backend_offload_info(&cfg, b).map(|o| (b.id.clone(), o)))
            .collect();
        // Guardrail evaluations for the same backends. The config
        // guard is NOT held here (cfg is a clone, dropped above), so
        // the telemetry inventory can be fetched directly.
        let gpus: Vec<GpuInfo> = {
            let store = state.telemetry().await;
            let inventory = lock_telemetry(&store).inventory();
            inventory
        };
        let journal = activation::load_journal(&state.data_dir);
        let evaluations: HashMap<String, OffloadEvaluation> = cfg
            .backends
            .iter()
            .filter_map(|b| {
                evaluate_backend_offload(&cfg, b, &gpus, &journal).map(|e| (b.id.clone(), e))
            })
            .collect();
        let probes: std::collections::HashMap<String, OffloadProbe> = state
            .offload_probes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for row in backends.iter_mut() {
            let Some(id) = row["id"].as_str().map(str::to_string) else {
                continue;
            };
            // The probe state attaches to every configured row,
            // offload-capable or not (a probe only ever exists for
            // an offload change, but the field's shape is uniform).
            row["offload_probe"] = match probes.get(&id) {
                Some(p) => json!({
                    "state": p.state,
                    "layers": p.layers,
                    "detail": p.detail,
                }),
                None => Value::Null,
            };
            let Some(o) = offload.get(&id) else { continue };
            row["offload"] = offload_row_json(o, evaluations.get(&id));
        }
    }
    // MTP draft control state per configured backend whose model
    // is MTP-capable (see backend_mtp_info). Absent for every
    // other backend — the flags form shows the control only when
    // this is present.
    {
        let mtp: HashMap<String, MtpInfo> = cfg
            .backends
            .iter()
            .filter_map(|b| backend_mtp_info(&cfg, b).map(|m| (b.id.clone(), m)))
            .collect();
        for row in backends.iter_mut() {
            let Some(id) = row["id"].as_str() else {
                continue;
            };
            let Some(m) = mtp.get(id) else { continue };
            row["mtp"] = json!({
                "enabled": m.enabled,
                "n_max": m.n_max,
                "parallel": m.parallel,
                "custom": m.custom,
            });
        }
    }
    // Parallel slots state per configured backend (tracker #61):
    // the flags form's Parallel slots field renders for every
    // backend, so — unlike the capability-gated offload/MTP
    // states above — this attaches to every configured row.
    {
        let parallel: HashMap<String, ParallelInfo> = cfg
            .backends
            .iter()
            .map(|b| (b.id.clone(), backend_parallel_info(b)))
            .collect();
        for row in backends.iter_mut() {
            let Some(id) = row["id"].as_str() else {
                continue;
            };
            let Some(p) = parallel.get(id) else { continue };
            row["parallel"] = json!({
                "slots": p.slots,
                "custom": p.custom,
            });
        }
    }
    // Predicted throughput per configured backend
    // (predicted-throughput design §7): the exact-match
    // measurement for the backend's SAVED configuration,
    // resolved through the same fingerprint path the Measure
    // action uses and the store's fingerprint-only lookup.
    // Absent when no exact measurement exists — never
    // null-as-zero — and independent of run state.
    {
        let gpu_pairs: Vec<(u32, String)> = {
            let store = state.telemetry().await;
            let guard = lock_telemetry(&store);
            guard
                .snapshot()
                .iter()
                .map(|s| (s.index, s.name.clone()))
                .collect()
        };
        let store = manager_optimize::MeasurementStore::load(&optimize::measurements_path(state));
        let predicted: HashMap<String, Value> = cfg
            .backends
            .iter()
            .filter_map(|b| {
                let inputs = optimize::fingerprint_inputs(&cfg, b, &gpu_pairs);
                let fp = manager_optimize::measurement_fingerprint(&inputs);
                store
                    .lookup(&fp)
                    .map(|e| (b.id.clone(), optimize::predicted_json(e)))
            })
            .collect();
        for row in backends.iter_mut() {
            let Some(id) = row["id"].as_str() else {
                continue;
            };
            let Some(p) = predicted.get(id) else { continue };
            row["predicted"] = p.clone();
        }
    }
    // Placement in short form (Groups page, predicted-throughput
    // design §8.2): where the saved configuration loads, derived
    // from config alone by the same visibility resolution the
    // measurement fingerprint uses. Absent when the placement
    // cannot be determined from the config — unknown placement
    // renders as nothing, never as a default (principle 12).
    {
        for row in backends.iter_mut() {
            let Some(id) = row["id"].as_str() else {
                continue;
            };
            let Some(b) = cfg.backends.iter().find(|b| b.id == id) else {
                continue;
            };
            let placement = match backend_gpu_visibility(b) {
                GpuVisibility::CpuOnly => Some("CPU".to_string()),
                GpuVisibility::Gpus(idxs) => Some(format!(
                    "GPU {}",
                    idxs.iter().map(|i| i.to_string()).collect::<Vec<_>>().join("+")
                )),
                GpuVisibility::Undetermined => None,
            };
            if let Some(p) = placement {
                row["placement"] = json!(p);
            }
        }
    }
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
        let stats = state.gateway_stats.lock().await;
        let windows = stats.window_stats(&pairs);
        let series = stats.series_stats(&pairs);
        for row in backends.iter_mut() {
            let Some(id) = row["id"].as_str().map(str::to_string) else {
                continue;
            };
            if let Some(w) = windows.as_ref().and_then(|m| m.get(&id)) {
                row["requests_1h"] = json!(w.requests);
                row["errors_1h"] = json!(w.errors);
                row["throughput_toks"] =
                    json!((w.tokens as f64 / w.window_secs * 10.0).round() / 10.0);
                // Health is the trailing-hour success share — the
                // one per-backend percentage the app actually
                // measures (the gateway card's ring uses the same
                // derivation). Absent when the window holds no
                // requests: no traffic is no basis for a score.
                if w.requests > 0 {
                    let share = (w.requests - w.errors) as f64 / w.requests as f64;
                    row["health"] = json!((share * 1000.0).round() / 10.0);
                }
            }
            if let Some(s) = series.as_ref().and_then(|m| m.get(&id)) {
                // Dashboard sparklines: tok/s for backend rows,
                // requests/min for the gateway row the UI sums
                // these into.
                row["spark"] = json!(s.toks_per_sec);
                row["req_spark"] = json!(s.req_per_min);
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
    // Service supervision: in service mode, a registered service's
    // SCM state is the truth for its row — the supervisor never
    // spawned it, so its view is stale by construction. `pid` is the
    // service host's pid (the llama-server child is the host's to
    // manage); rows gain `service_managed: true`.
    if cfg.supervision.mode == manager_config::SupervisionMode::Service {
        let names: Vec<(String, String)> = backends
            .iter()
            .filter_map(|row| {
                row["id"]
                    .as_str()
                    .map(|id| (id.to_string(), manager_services::backend_service_name(id)))
            })
            .collect();
        let queried = tokio::task::spawn_blocking(move || {
            use manager_services::ServiceControl;
            let control = manager_services::scm::platform_control();
            names
                .into_iter()
                .map(|(id, name)| (id, control.query(&name).ok()))
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        for (id, status) in queried {
            let Some(status) = status else { continue };
            if matches!(status.state, manager_services::ServiceState::NotInstalled) {
                continue;
            }
            if let Some(row) = backends.iter_mut().find(|r| r["id"] == json!(id)) {
                row["state"] = json!(process_state_str(status.process_state()));
                row["pid"] = json!(status.pid);
                row["adopted"] = json!(false);
                row["service_managed"] = json!(true);
            }
        }
    }
    Ok(json!({ "backends": backends }))
}


/// Start a backend (supervisor spawn, or SCM start in service mode
/// when the backend is registered as a service). Returns a task id.
pub async fn op_start_backend(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    ensure_backend_specs(state).await;
    backend_start_preflight(state, &id).await?;
    // Offload-probe capture: pending change + saved position,
    // before the spawn (a refused start never reaches here).
    let probe_ctx = offload_probe_context(state, &id).await;
    let task_id = state
        .record_task("backend_start", json!({ "backend_id": id }))
        .await;
    let outcome: Result<(), ApiError> = if state
        .service_route_active(&manager_services::backend_service_name(&id))
        .await
    {
        scm_start_backend(state, &id).await
    } else {
        let mut sup = state.supervisor().await.lock().await;
        // Reap first: without this, a backend that crashed since the
        // last list_backends poll would read as AlreadyRunning for a
        // corpse.
        sup.reap_exited();
        sup.spawn_backend(&id).await.map_err(ApiError::from)
    };
    match outcome {
        Ok(()) => {
            state
                .finish_task(&task_id, TaskStatus::Done, json!({ "backend_id": id }))
                .await;
            if let Some((layers, moe_layers, port)) = probe_ctx {
                spawn_offload_probe(sink, state, &id, layers, moe_layers, port);
            }
            sink.emit(
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
                    json!({ "backend_id": id, "error": e.to_string() }),
                )
                .await;
            Err(e)
        }
    }
}


/// Stop a backend, gracefully (drain) or immediately.
pub async fn op_stop_backend(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    id: String,
    graceful: bool,
) -> ApiResult {
    ensure_backend_specs(state).await;
    let task_id = state
        .record_task(
            "backend_stop",
            json!({ "backend_id": id, "graceful": graceful }),
        )
        .await;
    let outcome: Result<(), ApiError> = if state
        .service_route_active(&manager_services::backend_service_name(&id))
        .await
    {
        // The graceful flag has no service-mode equivalent in
        // increment 1 (see scm_stop_backend).
        let _ = graceful;
        scm_stop_backend(&id).await
    } else {
        state
            .supervisor()
            .await
            .lock()
            .await
            .stop(&id, graceful)
            .await
            .map_err(ApiError::from)
    };
    match outcome {
        Ok(()) => {
            state
                .finish_task(&task_id, TaskStatus::Done, json!({ "backend_id": id }))
                .await;
            sink.emit(
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
                    json!({ "error": e.to_string() }),
                )
                .await;
            Err(e)
        }
    }
}


/// Restart a backend (stop + start under the supervisor).
pub async fn op_restart_backend(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    id: String,
    graceful: bool,
) -> ApiResult {
    ensure_backend_specs(state).await;
    backend_start_preflight(state, &id).await?;
    // Offload-probe capture: pending change + saved position,
    // before the restart's spawn.
    let probe_ctx = offload_probe_context(state, &id).await;
    let task_id = state
        .record_task(
            "backend_restart",
            json!({ "backend_id": id, "graceful": graceful }),
        )
        .await;
    let outcome: Result<(), ApiError> = if state
        .service_route_active(&manager_services::backend_service_name(&id))
        .await
    {
        // SCM stop (waits for the stopped state) + SCM start on a
        // freshly regenerated definition.
        let _ = graceful;
        match scm_stop_backend(&id).await {
            Ok(()) => scm_start_backend(state, &id).await,
            Err(e) => Err(e),
        }
    } else {
        state
            .supervisor()
            .await
            .lock()
            .await
            .restart(&id, graceful)
            .await
            .map_err(ApiError::from)
    };
    match outcome {
        Ok(()) => {
            state
                .finish_task(&task_id, TaskStatus::Done, json!({ "backend_id": id }))
                .await;
            if let Some((layers, moe_layers, port)) = probe_ctx {
                spawn_offload_probe(sink, state, &id, layers, moe_layers, port);
            }
            sink.emit(
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
                    json!({ "error": e.to_string() }),
                )
                .await;
            // A restart the pending activation plan depends on just
            // failed: that is the plan failing to apply. A restart
            // failure with no plan entry for this backend is a plain
            // command error the UI already shows — no toast.
            let (unresolved, _) = activation_unresolved(state).await;
            if unresolved
                .iter()
                .any(|en| en.scope == "backend" && en.targets.iter().any(|t| t == &id))
            {
                notify::notify(
                    sink.as_ref(),
                    state,
                    notify::Category::Activation,
                    "Activation plan failed",
                    &format!("Backend {id} could not be restarted: {e}"),
                )
                .await;
            }
            Err(e)
        }
    }
}


/// One raw prompt against a backend; raw JSON result, no history (spec §1.3).
/// Minimal inference test request against a backend's
/// `/v1/chat/completions` (spec §14 `backend_test_request`).
pub async fn op_test_backend(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    prompt: String,
) -> ApiResult {
    let cfg = state.config.read().await;
    let backend = cfg
        .backends
        .iter()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    let base_url = format!("http://127.0.0.1:{}", backend.port);
    let result = test_request(&base_url, &prompt)
        .await
        .map_err(ApiError::from)?;
    Ok(serde_json::to_value(&result)?)
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
pub async fn op_get_gpu_telemetry(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
) -> ApiResult {
    // Adoption first: the pid -> backend map below is how telemetry
    // processes get attributed, and an unadopted orphan attributes
    // to nothing.
    adopt_orphan_backends(state).await;
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
    sink.emit(
        events::TELEMETRY_UPDATE,
        serde_json::to_value(&payload).unwrap_or(Value::Null),
    );
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
            let mut v = serde_json::to_value(p)?;
            v["backend_id"] = json!(pid_to_backend.get(&p.pid));
            Ok(v)
        })
        .collect::<Result<_, ApiError>>()?;
    // The command return enriches each sample with the store's peak
    // temperature mark (the event payload keeps the crate shape) and
    // the store's per-GPU history as sparkline arrays — the history
    // rings exist for exactly this (dashboard Trends), and without
    // this wiring the live payload never carried a series.
    let enriched: Vec<Value> = {
        let guard = lock_telemetry(&store);
        samples
            .iter()
            .map(|s| {
                let mut v = serde_json::to_value(s)?;
                let peak = guard.peaks(s.index).and_then(|p| p.temp_c).map(|m| m.value);
                v["temp_peak_c"] = json!(peak);
                let history = guard.history(s.index);
                if history.len() >= 2 {
                    v["util_spark"] =
                        json!(history.iter().map(|h| h.utilization_pct).collect::<Vec<_>>());
                    v["temp_spark"] =
                        json!(history.iter().map(|h| h.temp_c).collect::<Vec<_>>());
                    let power: Vec<f32> =
                        history.iter().filter_map(|h| h.power_w).collect();
                    if power.len() >= 2 {
                        v["power_spark"] = json!(power);
                    }
                }
                Ok(v)
            })
            .collect::<Result<_, ApiError>>()?
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
pub async fn op_run_wizard_step(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    step_id: String,
) -> ApiResult {
    let task_id = state
        .record_task("wizard_step", json!({ "step_id": step_id }))
        .await;
    let ctx = state.wizard_ctx(RunMode::Install).await;
    let wizard = state.wizard(RunMode::Install).await;
    // try_lock, not lock: a second invocation (e.g. a stray double
    // click while a step runs) must not queue on the mutex and execute
    // a step again after the current run finishes. Like the audit, it
    // reports the running state instead of failing. The report names
    // the in-flight step and how long it has been running, and the
    // rejection is written to the wizard log: a bare "busy" with no
    // record was undiagnosable when a step wedged the lock
    // (2026-10-16).
    let mut guard = match wizard.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            let running = state
                .wizard_step_running
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|(id, since)| (id.clone(), since.elapsed().as_secs()));
            append_wizard_log(
                &state.data_dir,
                &[format!(
                    "[{}] === step {step_id} (install): rejected — {} ===",
                    chrono::Utc::now().to_rfc3339(),
                    match &running {
                        Some((id, secs)) =>
                            format!("step {id} is already running (started {secs}s ago)"),
                        None => "another setup step is already running".to_string(),
                    }
                )],
            );
            let mut detail = json!({ "busy": true });
            let mut payload = json!({ "task_id": task_id, "busy": true, "outcome": null });
            if let Some((id, secs)) = &running {
                detail["running_step"] = json!(id);
                detail["elapsed_secs"] = json!(secs);
                payload["running_step"] = json!(id);
                payload["elapsed_secs"] = json!(secs);
            }
            state.finish_task(&task_id, TaskStatus::Done, detail).await;
            return Ok(payload);
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
    // Track the in-flight step for the busy arm above; cleared on
    // every path out of the run, including a step error.
    *state
        .wizard_step_running
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some((step_id.clone(), std::time::Instant::now()));
    let outcome_result = guard.run_step(&ctx, &step_id).await;
    *state
        .wizard_step_running
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    let outcome: StepOutcome = outcome_result.map_err(ApiError::from)?;
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
    // The step's display name, for the notification below (the
    // report is dropped with the guard).
    let step_name = guard
        .report()
        .steps
        .iter()
        .find(|s| s.id == step_id)
        .map(|s| s.name.clone())
        .unwrap_or_else(|| step_id.clone());
    drop(guard);
    state.persist_wizard().await;
    state.reload_config_from_disk().await;
    // A step run can complete the checklist (this is also the path
    // the wizard's "Run setup" full run takes, once per step).
    state.reconcile_setup_completed().await;
    let payload = WizardProgressPayload {
        task_id: task_id.clone(),
        step_id: Some(step_id.clone()),
        state: outcome.state,
        done,
        total,
        message: outcome.message.clone(),
    };
    sink.emit(
        events::WIZARD_PROGRESS,
        serde_json::to_value(&payload).unwrap_or(Value::Null),
    );
    state
        .finish_task(
            &task_id,
            TaskStatus::Done,
            serde_json::to_value(&outcome).unwrap_or(Value::Null),
        )
        .await;
    // Toasts: a step run is a discrete, operator-visible event (one
    // toast per run, at its terminal state). A failed start-gateway
    // while an activation plan is pending is the plan failing — one
    // toast, the Activation one, not both categories.
    let step_body = if outcome.message.trim().is_empty() {
        step_name.clone()
    } else {
        format!("{step_name}: {}", outcome.message)
    };
    match outcome.state {
        StepState::Done => {
            notify::notify(
                sink.as_ref(),
                state,
                notify::Category::Operations,
                "Setup step finished",
                &step_body,
            )
            .await;
        }
        StepState::Failed => {
            if step_id == "start-gateway" && activation_plan_pending(state).await {
                notify::notify(
                    sink.as_ref(),
                    state,
                    notify::Category::Activation,
                    "Activation plan failed",
                    &format!(
                        "The gateway could not be started to apply the saved changes: {}",
                        outcome.message
                    ),
                )
                .await;
            } else {
                notify::notify(
                    sink.as_ref(),
                    state,
                    notify::Category::Failures,
                    "Setup step failed",
                    &step_body,
                )
                .await;
            }
        }
        StepState::Manual => {
            notify::notify(
                sink.as_ref(),
                state,
                notify::Category::Failures,
                "Setup step needs manual action",
                &step_body,
            )
            .await;
        }
        _ => {}
    }
    Ok(json!({ "task_id": task_id, "outcome": outcome }))
}


/// Re-run audit: re-executes check-type steps without reinstalling (spec §4.1).
pub async fn op_run_audit(state: &Arc<AppState>, sink: &Arc<dyn EventSink>) -> ApiResult {
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
                "manual_command": s.manual_command,
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
    sink.emit(
        events::WIZARD_PROGRESS,
        serde_json::to_value(&payload).unwrap_or(Value::Null),
    );
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
pub async fn op_list_models(state: &Arc<AppState>, _sink: &Arc<dyn EventSink>) -> ApiResult {
    ensure_catalog_models(state).await;
    reap_downloads(state).await;
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
    let catalog_ctx: HashMap<&str, u32> =
        catalog.iter().map(|s| (s.id.as_str(), s.ctx_max)).collect();
    let catalog_names: HashMap<&str, &str> = catalog
        .iter()
        .map(|s| (s.id.as_str(), s.name.as_str()))
        .collect();
    let catalog_offload: HashMap<&str, (Option<u32>, Option<u32>)> = catalog
        .iter()
        .map(|s| (s.id.as_str(), (s.moe_layers, s.default_offload_layers)))
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
                "name": m.display_name.as_deref().map(str::trim).filter(|s| !s.is_empty())
                    .or_else(|| catalog_names.get(m.id.as_str()).copied())
                    .unwrap_or(m.id.as_str()),
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
                // The operator's MoE declaration on the model
                // record, and the catalog's pins for the same
                // figures: the Models view's offload editor reads
                // the pair (catalog wins where it pins a value).
                "moe_layers": m.moe_layers,
                "default_offload_layers": m.default_offload_layers,
                "catalog_moe_layers": catalog_offload.get(m.id.as_str()).and_then(|p| p.0),
                "catalog_default_offload_layers": catalog_offload.get(m.id.as_str()).and_then(|p| p.1),
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
fn download_spec_for(cfg: &AppConfig, id: &str) -> Result<manager_models::ModelSpec, ApiError> {
    let m = cfg
        .models
        .iter()
        .find(|m| m.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("model {id}")))?;
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
            expert_bytes: None,
            moe_layers: m.moe_layers,
            default_offload_layers: m.default_offload_layers,
            offload_restrictions: Vec::new(),
            mtp: false,
        });
    }
    Err(ApiError::NotFound(format!(
        "no catalog entry for model {id} and no https source_url configured"
    )))
}

/// Start a model download; progress streams via `download-progress`.
/// A completion watcher records the terminal outcome in
/// `download_outcomes` (surfaced by `list_models` as `download_error`)
/// and, when the model has a real pinned SHA-256, verifies the finished
/// file and stamps `verified_at` on success. The model is marked
/// verifying before the watcher spawns and unmarked on every watcher
/// exit path; each attempt carries a token so a superseded watcher
/// cannot overwrite a newer attempt's outcome.
pub async fn op_download_model(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    ensure_catalog_models(state).await;
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
            .ok_or_else(|| ApiError::NotFound(format!("model {id}")))?;
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
        return Err(ApiError::Models(format!(
            "download already in flight for model {id}"
        )));
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
                return Err(ApiError::InvalidInput(format!(
                    "model {id} is loaded by running backend {}; stop it before re-downloading",
                    b.id
                )));
            }
        }
    }
    let progress_sink = Arc::clone(sink);
    let model_id = id.clone();
    let handle = manager_models::download(&spec, &dest, move |progress| {
        let payload = DownloadProgressPayload {
            model_id: model_id.clone(),
            downloaded_bytes: progress.downloaded_bytes,
            total_bytes: Some(progress.total_bytes),
            fraction: Some(progress.fraction()),
        };
        progress_sink.emit(
            events::DOWNLOAD_PROGRESS,
            serde_json::to_value(&payload).unwrap_or(Value::Null),
        );
    })
    .await
    .map_err(ApiError::from)?;
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
            m.verified_sha256 = None;
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
    let watcher_sink = Arc::clone(sink);
    let watcher_state = Arc::clone(state);
    let watcher_id = id.clone();
    let watcher_dest = dest.clone();
    tokio::spawn(async move {
        let outcome = watcher_handle.wait().await;
        let st = watcher_state;
        // Display name for the completion/failure toasts: the
        // operator-set name, else the curated catalog name, else the
        // model id (same precedence as `list_models`).
        let watcher_name = {
            let cfg = st.config.read().await;
            cfg.models
                .iter()
                .find(|m| m.id == watcher_id)
                .and_then(|m| {
                    m.display_name
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                })
                .or_else(|| {
                    manager_models::curated_models()
                        .iter()
                        .find(|s| s.id == watcher_id)
                        .map(|s| s.name.clone())
                })
                .unwrap_or_else(|| watcher_id.clone())
        };
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
                let mut verified = false;
                let (failed, verification_failure) = match pinned {
                    Some(sha) if is_real_pin(&sha) => {
                        match manager_models::verify_sha256(&watcher_dest, &sha).await {
                            Ok(true) => {
                                verified = true;
                                let mut cfg = st.config.write().await;
                                if let Some(m) = cfg.models.iter_mut().find(|m| m.id == watcher_id)
                                {
                                    m.verified_at = Some(chrono::Utc::now());
                                    m.verified_sha256 = Some(sha.clone());
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
                                            m.verified_sha256 = None;
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
                // Toasts: a finished download is the Operations
                // event; a recorded failure in this arm is always a
                // verification failure (transfer errors land in the
                // Err arm below). The operator-cancelled arm stays
                // silent — the operator already knows.
                let toast = match &failed {
                    None => Some((
                        notify::Category::Operations,
                        "Model download finished",
                        if verified {
                            format!(
                                "{watcher_name} downloaded and verified against its pinned SHA-256."
                            )
                        } else {
                            format!("{watcher_name} downloaded.")
                        },
                    )),
                    Some(msg) => Some((
                        notify::Category::Failures,
                        "Model download failed verification",
                        msg.clone(),
                    )),
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
                if let Some((category, title, body)) = toast {
                    notify::notify(watcher_sink.as_ref(), &st, category, title, &body).await;
                }
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
                notify::notify(
                    watcher_sink.as_ref(),
                    &st,
                    notify::Category::Failures,
                    "Model download failed",
                    &format!("{watcher_name} — download failed: {e}"),
                )
                .await;
            }
        }
    });
    Ok(json!({ "model_id": id, "dest": dest }))
}


/// Cancel an in-flight download (partial file removed by the worker).
pub async fn op_cancel_download(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    // Reap first: cancelling a download that already finished must not
    // report success — the terminal handle is dropped here and the
    // lookup below honestly reports that nothing is in flight.
    reap_downloads(state).await;
    let handle = state
        .downloads
        .lock()
        .await
        .remove(&id)
        .ok_or_else(|| ApiError::NotFound(format!("no download in flight for {id}")))?;
    handle.cancel().await.map_err(ApiError::from)?;
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
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
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

/// Optimize pipeline output, persisted to
/// `<data_dir>/logs/optimize.log` and served by `get_logs` as the
/// `optimize` scope (tracker #52). The Optimize pipeline's
/// decisions otherwise live only in the stored plan and the
/// transient consultation response: when a plan comes back wrong
/// (tracker #51's dead-end items) there was nothing to inspect.
/// The optimize modules write very verbose, timestamped records
/// of every consultation, parse, validation verdict, apply
/// outcome, and experiment event through this one append.
/// Appends are best-effort — logging must never fail the
/// operation it describes. Rotation mirrors the wizard log: past
/// ~1 MiB the file rotates to its `.prev` sibling
/// (`optimize.log.prev`, via `prev_log_path`). Secret-free by
/// construction: the only inputs are the operator's directive,
/// the allowlist-built OptimizeContext document (sentinel-tested
/// in manager-optimize), the consulted model's response to that
/// document, and verdicts computed from them — no call site reads
/// credential stores, environment values, or config fields
/// outside the context document.
pub(crate) fn append_optimize_log(data_dir: &std::path::Path, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    let logs = data_dir.join("logs");
    if std::fs::create_dir_all(&logs).is_err() {
        return;
    }
    let path = logs.join("optimize.log");
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
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(text.as_bytes());
    }
}

/// File-backed optimize records for `get_logs` (mirrors
/// `wizard_log_records`): newest last-2000 lines per file.
fn optimize_log_records(data_dir: &std::path::Path) -> (Vec<LogRecord>, bool) {
    let logs = data_dir.join("logs");
    let mut records = Vec::new();
    let mut has_previous = false;
    for archived in [true, false] {
        let base = logs.join("optimize.log");
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
                source: "optimize".to_string(),
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
/// optional case-insensitive substring over line text; `query` is an
/// optional case-insensitive substring over line text and source.
/// `since_ms`/`until_ms` optionally bound record timestamps (epoch
/// milliseconds, both inclusive). All filters apply before counting
/// and paging, so the reported total is the filtered total.
///
/// Ordering is by timestamp: newest-first (`order` absent or
/// `"desc"`, the default in every scope, archives included) or the
/// exact mirror, oldest-first (`order = "asc"`). `offset` skips that
/// many filtered, ordered records before the page.
///
/// Returns `{lines, total_matching, total, truncated, scopes}`:
/// `lines` is the page (`limit` records from `offset`, default limit
/// 200), each record stamped with its `source` process id and an
/// `archived` flag for previous-run records; `total_matching` — and
/// its alias `total` — counts every record that matched before
/// paging, and `truncated` says whether paging cut records on either
/// side of the page. `scopes` lists every retained scope as
/// `{id, previous}`, where `previous` means the id also has an
/// archived previous-run ring (incl. previous run).
#[allow(clippy::too_many_arguments)] // argument list is the registry contract
pub async fn op_get_logs(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    scope: String,
    level: Option<String>,
    search: Option<String>,
    query: Option<String>,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
    order: Option<String>,
    offset: Option<u64>,
    limit: Option<u64>,
) -> ApiResult {
    let limit = limit.unwrap_or(200) as usize;
    let sup = state.supervisor().await.lock().await;
    // Unfiltered fetch: filtering, merging with the file-backed
    // gateway records, ordering, and pagination happen here so every
    // source is treated identically.
    let mut records: Vec<LogRecord> = if scope == "gateway" || scope == "wizard" || scope == "optimize"
    {
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
    let (optimize_records, optimize_previous) = optimize_log_records(&state.data_dir);
    if !optimize_records.is_empty() {
        scopes.push(json!({ "id": "optimize", "previous": optimize_previous }));
        if scope == "all" || scope == "optimize" {
            records.extend(optimize_records);
        }
    }
    if let Some(filter) = level.as_deref().and_then(LogLevelFilter::parse) {
        records.retain(|r| filter.matches(&r.line));
    }
    if let Some(search) = &search {
        let needle = search.to_lowercase();
        records.retain(|r| r.line.to_lowercase().contains(&needle));
    }
    if let Some(query) = &query {
        let needle = query.to_lowercase();
        records.retain(|r| {
            r.line.to_lowercase().contains(&needle) || r.source.to_lowercase().contains(&needle)
        });
    }
    // Time window, both bounds inclusive. Every record carries a
    // capture timestamp (file-backed gateway/wizard records are
    // stamped with their file's mtime), so the comparison is direct.
    if since_ms.is_some() || until_ms.is_some() {
        records.retain(|r| {
            let ms = r.ts.timestamp_millis();
            since_ms.is_none_or(|s| ms >= s) && until_ms.is_none_or(|u| ms <= u)
        });
    }
    // Newest first; the sort is stable, so records sharing a timestamp
    // keep their source order (gateway files emit newest-first).
    // "asc" is the exact mirror — the sorted vec reversed — so
    // same-timestamp groups (file-backed lines share one mtime) keep
    // their source order in the oldest-first view too, instead of
    // flipping within the group, and page boundaries mirror between
    // the two orders.
    records.sort_by_key(|r| std::cmp::Reverse(r.ts));
    // `order` is strict: an unrecognized value is a caller bug and
    // is refused, not silently treated as the default (code-review
    // 4.17).
    match order.as_deref() {
        None | Some("desc") => {}
        Some("asc") => records.reverse(),
        Some(other) => {
            return Err(ApiError::InvalidInput(format!(
                "unknown log order {other:?}; expected \"asc\" or \"desc\""
            )))
        }
    }
    let total_matching = records.len();
    let offset = offset.unwrap_or(0) as usize;
    let page: Vec<LogRecord> = records.into_iter().skip(offset).take(limit).collect();
    Ok(json!({
        "lines": serde_json::to_value(&page)?,
        "total_matching": total_matching,
        "total": total_matching,
        // Paging cut records when any matched record sits outside
        // the page — before it (a nonzero offset into a non-empty
        // result) or after it (code-review 4.9: the offset side was
        // previously not reported).
        "truncated": (offset > 0 && total_matching > 0) || total_matching > offset + page.len(),
        "scopes": scopes,
    }))
}

// ---------------------------------------------------------------------------
// Gateway
// ---------------------------------------------------------------------------

/// Current gateway configuration (from AppConfig). Each group object
/// additionally carries its derived enabled state — `enabled` and
/// `enabled_members`, computed by manager-gateway (the one place the
/// rule lives): a group is enabled exactly when at least one member
/// backend is enabled, and only enabled groups reach the generated
/// gateway config. The state is derived, never stored, so the stored
/// `GatewayConfig` serialization is unchanged.
pub async fn op_get_gateway_config(state: &Arc<AppState>, _sink: &Arc<dyn EventSink>) -> ApiResult {
    let cfg = state.config.read().await;
    let gw: &GatewayConfig = &cfg.gateway;
    let mut v = serde_json::to_value(gw)?;
    if let Some(groups) = v.get_mut("groups").and_then(Value::as_array_mut) {
        for (group, group_json) in cfg.gateway.groups.iter().zip(groups.iter_mut()) {
            let enabled_members = manager_gateway::group_enabled_member_count(group, &cfg);
            group_json["enabled"] = json!(enabled_members > 0);
            group_json["enabled_members"] = json!(enabled_members);
        }
    }
    Ok(v)
}

/// Merge a JSON patch into the gateway config and persist: the patch is
/// merged into a clone of the config, the clone is saved via
/// `manager_config::save` (which validates before writing), and only a
/// successful save installs the clone in memory. `persisted` is true
/// only after that save; on failure the in-memory config is unchanged
/// and the error surfaces.
pub async fn op_update_gateway_config(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    patch: Value,
) -> ApiResult {
    let mut cfg = state.config.write().await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    let mut v = serde_json::to_value(&new_cfg.gateway)?;
    merge_json(&mut v, patch);
    new_cfg.gateway = serde_json::from_value(v)?;
    record_gateway_change(
        state,
        &pre_cfg,
        &new_cfg,
        "gateway_config",
        "Gateway configuration changed".to_string(),
        false,
    )
    .await?;
    manager_config::save(&new_cfg, &state.config_path).map_err(ApiError::from)?;
    *cfg = new_cfg;
    Ok(json!({ "ok": true, "persisted": true }))
}

/// Journal a change for the gateway activation plan when it
/// affects what the running gateway serves: the gateway
/// configuration a start would generate differs between `pre` and
/// `post` (the armed-only MCP view on both sides — see
/// [`activation::config_drift`] for why the raw registry is the
/// wrong comparison), or `force` is set for changes the YAML does
/// not carry (the bind address). Saves that leave the gateway
/// configuration identical (a backend's context length, a tensor
/// split) are backend-process concerns and create no gateway plan.
///
/// Callers invoke this BEFORE persisting the configuration and
/// propagate the error: a journal write failure then aborts the
/// change with the configuration untouched, instead of stranding a
/// saved change no plan will ever show (code-review 4.3). The
/// journal's own resolution uses the running backends' real start
/// times, so already-live backend-scope entries prune correctly
/// (4.1).
async fn record_gateway_change(
    state: &AppState,
    pre: &AppConfig,
    post: &AppConfig,
    kind: &str,
    summary: String,
    force: bool,
) -> Result<(), ApiError> {
    if !force {
        // Compare what a gateway start would WRITE, before and
        // after: the armed-only MCP view on both sides (presence
        // gathered across the pair, so a removed server still
        // resolves). The raw registry is the wrong input — an edit
        // to an unarmed server's stored fields changes the raw
        // YAML but not the started one, and journaling it would
        // demand a gateway restart that changes nothing (the same
        // defect as the drift comparison's, tracker #42).
        let presence = mcp_credential_presence(&[pre, post]).await;
        let before_cfg =
            manager_wizard::config_with_armed_gateway_mcp_servers(pre, |t| presence.contains(t));
        let after_cfg =
            manager_wizard::config_with_armed_gateway_mcp_servers(post, |t| presence.contains(t));
        let before = manager_gateway::generate_config_yaml(&before_cfg).unwrap_or_default();
        let after = manager_gateway::generate_config_yaml(&after_cfg).unwrap_or_default();
        if before == after {
            // The YAML does not carry the routing rule table: a
            // callback-only change (a window, the output reserve,
            // or the advisory toggle on an already-enabled rule)
            // leaves the YAML identical but must still journal, or
            // the activation plan would never offer the restart
            // that regenerates llmdil_routing.py (design §6.3/§7).
            let before_py = manager_gateway::generate_routing_callback(&before_cfg);
            let after_py = manager_gateway::generate_routing_callback(&after_cfg);
            if before_py == after_py {
                return Ok(());
            }
        }
    }
    let started_at = backend_start_times(state).await;
    let running = backend_running_ids(state).await;
    let configured: std::collections::HashSet<String> =
        post.backends.iter().map(|b| b.id.clone()).collect();
    activation::record_change(
        &state.data_dir,
        pre,
        post,
        kind,
        summary,
        "gateway",
        Vec::new(),
        &started_at,
        &configured,
        &running,
    )
    .map_err(ApiError::Io)
}

/// Journal a change that only the target backend's next
/// (re)start activates — its flags, tensor split, or model
/// assignment. These never appear in the gateway YAML, so the
/// gateway rule in [`record_gateway_change`] cannot see them; the
/// activation plan carries them as backend-scope entries that
/// resolve when the backend (re)starts (or is deleted).
/// Same record-before-save contract as [`record_gateway_change`].
async fn record_backend_change(
    state: &AppState,
    pre: &AppConfig,
    post: &AppConfig,
    kind: &str,
    summary: String,
    backend_id: &str,
) -> Result<(), ApiError> {
    let started_at = backend_start_times(state).await;
    let running = backend_running_ids(state).await;
    let configured: std::collections::HashSet<String> =
        post.backends.iter().map(|b| b.id.clone()).collect();
    activation::record_change(
        &state.data_dir,
        pre,
        post,
        kind,
        summary,
        "backend",
        vec![backend_id.to_string()],
        &started_at,
        &configured,
        &running,
    )
    .map_err(ApiError::Io)
}

/// The advanced-flags-mode refusal (tracker #60): while a backend
/// is in advanced mode, its raw flags are operator-owned text and
/// the single source of truth for its launch flags, so every
/// structured write path for the backend is refused with this
/// error. The UI disables its controls in the mode; this is the
/// enforcement REST callers and the Optimize apply executor meet.
fn advanced_flags_refusal(backend_id: &str) -> ApiError {
    ApiError::InvalidInput(format!(
        "backend {backend_id} is in advanced flags mode: its flags are \
         edited as raw text (backend_set_raw_flags) and structured flags \
         writes are refused until advanced mode is turned off \
         (backend_set_flags_mode)"
    ))
}

/// Refuse a structured flags write for a backend in advanced
/// flags mode (tracker #60); a no-op otherwise.
fn ensure_backend_not_advanced(b: &manager_config::BackendConfig) -> Result<(), ApiError> {
    if b.flags_advanced {
        Err(advanced_flags_refusal(&b.id))
    } else {
        Ok(())
    }
}

/// Running backends' start instants (now minus supervisor uptime),
/// for activation-plan resolution: a backend-scope entry resolves
/// once every target has (re)started at or after it. Adopted
/// processes report no uptime and are absent here — their entries
/// stay unresolved and the plan classifies them from live state.
async fn backend_start_times(
    state: &AppState,
) -> std::collections::HashMap<String, chrono::DateTime<chrono::Utc>> {
    let sup = state.supervisor().await.lock().await;
    let now = chrono::Utc::now();
    sup.status()
        .iter()
        .filter_map(|s| {
            s.uptime.map(|u| {
                (
                    s.id.clone(),
                    now - chrono::Duration::from_std(u).unwrap_or_default(),
                )
            })
        })
        .collect()
}

/// Ids of backends whose supervisor process is running or starting.
async fn backend_running_ids(state: &AppState) -> std::collections::HashSet<String> {
    let sup = state.supervisor().await.lock().await;
    sup.status()
        .iter()
        .filter(|s| matches!(s.state, ProcessState::Running | ProcessState::Starting))
        .map(|s| s.id.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// MXC
// ---------------------------------------------------------------------------

/// Current MXC policy: the configured policy file, else the built-in
/// default. Same source rule as `get_mxc_policy`.
fn current_mxc_policy(cfg: &AppConfig) -> Result<Policy, ApiError> {
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

// ---------------------------------------------------------------------------
// Setup wizard: additional §14 commands
// ---------------------------------------------------------------------------

/// Prerequisite verification matrix: per-step prerequisite lists from
/// [`manager_wizard::steps::default_steps`] (each step's `prerequisites()`
/// are the step ids that must be Done or Manual before it may run).
pub async fn op_setup_get_prerequisites(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
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
pub async fn op_setup_retry_step(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    step_id: String,
) -> ApiResult {
    let wizard = state.wizard(RunMode::Install).await;
    wizard.lock().await.retry(&step_id);
    state.persist_wizard().await;
    Ok(json!({ "ok": true, "step_id": step_id }))
}

/// Skip a step with a note.
pub async fn op_setup_skip_step(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    step_id: String,
    note: String,
) -> ApiResult {
    let wizard = state.wizard(RunMode::Install).await;
    wizard.lock().await.skip(&step_id, note);
    state.persist_wizard().await;
    state.reconcile_setup_completed().await;
    Ok(json!({ "ok": true, "step_id": step_id }))
}


/// Exact commands a step will run ("Show commands" UI). Must equal the argv
/// actually spawned by the step.
pub async fn op_setup_get_commands(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    step_id: String,
) -> ApiResult {
    let ctx = state.wizard_ctx(RunMode::Install).await;
    let wizard = state.wizard(RunMode::Install).await;
    let guard = wizard.lock().await;
    let step = guard
        .get_step(&step_id)
        .ok_or_else(|| ApiError::NotFound(format!("wizard step {step_id}")))?;
    let commands = step.planned_commands(&ctx);
    Ok(serde_json::to_value(&commands)?)
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
                "manual_command": s.manual_command,
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
pub async fn op_setup_get_steps(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let wizard = state.wizard(RunMode::Install).await;
    match wizard.try_lock() {
        Ok(guard) => {
            let payload = wizard_steps_payload(&guard);
            *state.wizard_steps_cache.write().await = Some(payload.clone());
            drop(guard);
            // Reading the steps is also a healing trigger for the
            // launch-routing flag (the wizard view polls this while
            // the user works through setup in any order).
            state.reconcile_setup_completed().await;
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
pub async fn op_setup_status(state: &Arc<AppState>, _sink: &Arc<dyn EventSink>) -> ApiResult {
    let cfg = state.config.read().await;
    Ok(json!({ "setup_completed": cfg.setup_completed }))
}

/// Mark the setup wizard complete and persist the flag: the flag is
/// set on a clone of the config, the clone is saved via
/// `manager_config::save`, and only a successful save installs it in
/// memory — a failed save returns an error rather than an ok:true
/// payload, so the UI never reports completion that a restart would
/// silently undo.
pub async fn op_setup_mark_complete(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let mut cfg = state.config.write().await;
    let mut new_cfg = cfg.clone();
    new_cfg.setup_completed = true;
    manager_config::save(&new_cfg, &state.config_path).map_err(ApiError::from)?;
    *cfg = new_cfg;
    Ok(json!({ "ok": true, "persisted": true }))
}

/// Help URL a wizard step offers alongside its remediation hint
/// (e.g. the NVIDIA driver download page on the hardware-detect step).
/// Returns `{"url": string|null, "step_id": string}`.
pub async fn op_setup_step_help_url(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    step_id: String,
) -> ApiResult {
    let wizard = state.wizard(RunMode::Install).await;
    let url = wizard.lock().await.help_url(&step_id).map(String::from);
    Ok(json!({ "url": url, "step_id": step_id }))
}

/// Open an external URL in the system browser. Only http/https URLs are
/// accepted; anything else is rejected before spawning.
pub async fn op_open_url(
    _state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    url: String,
) -> ApiResult {
    let ok = url.starts_with("http://") || url.starts_with("https://");
    if !ok {
        return Err(ApiError::InvalidInput(
            "open_url accepts http/https URLs only".to_string(),
        ));
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
            .map_err(|e| ApiError::Io(format!("failed to open URL: {e}")))?;
        Ok(json!({ "ok": true }))
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err(ApiError::Unsupported(
            "open_url is only supported on Windows".to_string(),
        ))
    }
}

/// The gateway master key, for the Gateway view's client-endpoint
/// card: it is both the API key local clients present and the
/// LiteLLM admin UI password (username `admin`). Read from Windows
/// Credential Manager, generating and storing on first use — the
/// same ensure the gateway spawn and the editor-config writer use,
/// so the value shown is the value in force.
pub async fn op_gateway_master_key(
    _state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let key = manager_wizard::ensure_gateway_master_key(&RealSystemOps::default()).await?;
    Ok(json!({ "master_key": key }))
}

/// Open a folder in Windows Explorer. `path` may name the folder
/// itself or a file inside it (the diagnostics bundle path is a
/// file; its containing folder is opened). Backs the "Open folder"
/// button shown after a diagnostics export.
pub async fn op_open_folder(
    _state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    path: String,
) -> ApiResult {
    let p = std::path::Path::new(&path);
    let dir = if p.is_dir() {
        p.to_path_buf()
    } else if p.is_file() {
        match p.parent() {
            Some(parent) => parent.to_path_buf(),
            None => {
                return Err(ApiError::InvalidInput(format!(
                    "no containing folder for: {path}"
                )))
            }
        }
    } else {
        return Err(ApiError::InvalidInput(format!(
            "path does not exist: {path}"
        )));
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
            .map_err(|e| ApiError::Io(format!("failed to open folder: {e}")))?;
        Ok(json!({ "ok": true }))
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = dir;
        Err(ApiError::Unsupported(
            "open_folder is only supported on Windows".to_string(),
        ))
    }
}

/// Mark a step as manually completed by the user.
pub async fn op_setup_mark_manual(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    step_id: String,
    path: String,
    version: String,
) -> ApiResult {
    let wizard = state.wizard(RunMode::Install).await;
    wizard.lock().await.mark_manual(&step_id, path, version);
    state.persist_wizard().await;
    state.reconcile_setup_completed().await;
    Ok(json!({ "ok": true, "step_id": step_id }))
}


// ---------------------------------------------------------------------------
// Backends: additional §14 commands
// ---------------------------------------------------------------------------

/// Backend configuration by id.
pub async fn op_backend_get_config(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    let cfg = state.config.read().await;
    let b = cfg
        .backends
        .iter()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    Ok(serde_json::to_value(b)?)
}

/// Preview a config diff before applying (spec §14 `backend_preview_diff`).
pub async fn op_backend_preview_diff(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    patch: Value,
) -> ApiResult {
    let patch: BackendPatch = serde_json::from_value(patch)?;
    let cfg = state.config.read().await;
    let backend = cfg
        .backends
        .iter()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    let diff: ConfigDiff = manager_config::diff_config(&id, backend, &patch);
    Ok(serde_json::to_value(&diff)?)
}

/// Apply a config patch with restart semantics (spec §14
/// `backend_apply_config`). The patch is applied to a clone of the
/// config (`apply_config_diff` validates and rolls the clone back on
/// failure), the clone is persisted via `manager_config::save`, and
/// only a successful save installs it in memory — `persisted` reports
/// that truthfully. `apply_mode` is `RestartNow | OnNextRestart` —
/// `RestartNow` records a restart task for the UI to dispatch; the
/// restart itself goes through `restart_backend`.
pub async fn op_backend_apply_config(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    patch: Value,
    apply_mode: String,
) -> ApiResult {
    let patch: BackendPatch = serde_json::from_value(patch)?;
    let apply_mode = match apply_mode.as_str() {
        "RestartNow" => "RestartNow",
        "OnNextRestart" => "OnNextRestart",
        other => {
            return Err(ApiError::Config(format!(
                "unknown apply_mode {other:?}; expected RestartNow | OnNextRestart"
            )))
        }
    };
    // Advanced flags mode (tracker #60): a patch touching the
    // flags form's fields (context/batch, split, raw flags) is a
    // structured flags write and is refused for an advanced
    // backend; the patch's other fields (enabled, port, model
    // file, restart policy) are not flags state and still apply.
    let touches_flags = patch.n_ctx.is_some()
        || patch.n_batch.is_some()
        || patch.tensor_split.is_some()
        || patch.split_mode.is_some()
        || patch.raw_flags.is_some();
    let diff: ConfigDiff = {
        let mut cfg = state.config.write().await;
        let pre_cfg = cfg.clone();
        let mut new_cfg = cfg.clone();
        if touches_flags {
            if let Some(b) = new_cfg.backends.iter().find(|b| b.id == id) {
                ensure_backend_not_advanced(b)?;
            }
        }
        let diff = manager_config::apply_config_diff(&mut new_cfg, &id, &patch)
            .map_err(ApiError::from)?;
        record_gateway_change(
            state,
            &pre_cfg,
            &new_cfg,
            "backend_apply_config",
            format!("Backend `{id}` settings changed"),
            false,
        )
        .await
        ?;
        // The same save is also a backend activation: whatever the
        // gateway does with it, the backend process itself only
        // picks its settings up at its next (re)start.
        if !diff.changes.is_empty() {
            record_backend_change(
                state,
                &pre_cfg,
                &new_cfg,
                "backend_apply_config",
                format!("Backend `{id}` settings changed"),
                &id,
            )
            .await
            ?;
        }
        manager_config::save(&new_cfg, &state.config_path)
            .map_err(ApiError::from)?;
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
    state.sync_backend_service_definition(&id).await;
    Ok(json!({
        "backend_id": id,
        "diff": diff,
        "apply_mode": apply_mode,
        "restart_task_id": restart_task,
        "persisted": true,
    }))
}



/// The tensor split a newly created backend for `model_id` is
/// assigned from live telemetry: existing enabled backends are
/// accounted for as they are, and the new backend is assigned a
/// split by the same algorithm the wizard's tensor-split step
/// uses. Computed before any config write lock is taken (the
/// telemetry accessor reads the config while initializing, so the
/// two must not nest). No telemetry yet (or a single GPU) yields
/// None — the split stays unset and the wizard step assigns it
/// later.
async fn propose_new_backend_split(
    state: &Arc<AppState>,
    model_id: &str,
) -> Option<(Vec<f32>, manager_config::SplitMode)> {
    let snap = state.config.read().await.clone();
    let vrams: Vec<u64> = {
        let store = state.telemetry().await;
        let guard = lock_telemetry(&store);
        let mut samples = guard.snapshot();
        samples.sort_by_key(|s| s.index);
        samples.iter().map(|s| s.vram_total_mib).collect()
    };
    if vrams.len() < 2 {
        return None;
    }
    let size_of = |model_file: &std::path::Path| -> u64 {
        let target = resolve_under_data_dir(&snap.data_dir, model_file);
        let model = snap
            .models
            .iter()
            .find(|m| resolve_under_data_dir(&snap.data_dir, &m.gguf_path) == target);
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
            // Account for what the backend actually puts on
            // GPUs: with expert offload, only the resident
            // share occupies VRAM.
            model_bytes: backend_memory_requirement(&snap, b)
                .map(|r| r.gpu_weights_mib * manager_models::MIB)
                .unwrap_or_else(|| size_of(&b.model_file)),
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

/// The tensor split for an explicit GPU selection (backend_add's
/// `gpu_indices`, the add form's Placement chips): an even split
/// across the selected GPUs, positions in telemetry index order —
/// the composition the Placement form applies when no configured
/// split covers the selection. Validation mirrors placement_apply
/// (telemetry must exist, the selection must name inventory GPUs,
/// and at least one GPU must be selected). The mode is Layer, as
/// every assigned split carries (config validation requires a mode
/// whenever a split is set).
fn split_for_gpu_selection(
    gpu_indices: &[u32],
    selected: &[u32],
) -> Result<(Vec<f32>, manager_config::SplitMode), ApiError> {
    if gpu_indices.is_empty() {
        return Err(ApiError::Telemetry(
            "no telemetry yet; cannot validate placement".to_string(),
        ));
    }
    if selected.is_empty() {
        return Err(ApiError::InvalidInput(
            "select at least one GPU for the split".to_string(),
        ));
    }
    for s in selected {
        if !gpu_indices.contains(s) {
            return Err(ApiError::InvalidInput(format!(
                "GPU {s} is not in the telemetry inventory"
            )));
        }
    }
    let share = 1.0 / selected.len() as f32;
    let split: Vec<f32> = gpu_indices
        .iter()
        .map(|i| if selected.contains(i) { share } else { 0.0 })
        .collect();
    Ok((split, manager_config::SplitMode::Layer))
}

/// The context/batch validation shared by backend_update_flags
/// and backend_add (the add form offers the same fields under the
/// same rules): context within the accepted range, batch between
/// 1 and the context length.
fn validate_ctx_batch(n_ctx: u32, n_batch: u32) -> Result<(), ApiError> {
    if !(1024..=1_048_576).contains(&n_ctx) {
        return Err(ApiError::InvalidInput(
            "context length must be between 1024 and 1048576".to_string(),
        ));
    }
    if n_batch == 0 || n_batch > n_ctx {
        return Err(ApiError::InvalidInput(
            "batch size must be between 1 and the context length".to_string(),
        ));
    }
    Ok(())
}

/// The parallel-slots validation shared by backend_set_parallel
/// and backend_add (the add form offers the same field under the
/// same rules): the Parallel slots control's range — at least one
/// slot, at most the app's ceiling (see
/// [`manager_models::parallel`]).
fn validate_parallel_slots(slots: u32) -> Result<(), ApiError> {
    use manager_models::parallel::{PARALLEL_SLOTS_MAX, PARALLEL_SLOTS_MIN};
    if !(PARALLEL_SLOTS_MIN..=PARALLEL_SLOTS_MAX).contains(&slots) {
        return Err(ApiError::InvalidInput(format!(
            "parallel slots must be between {PARALLEL_SLOTS_MIN} and {PARALLEL_SLOTS_MAX}"
        )));
    }
    Ok(())
}

/// The resolved offload for a backend being created: the raw flags
/// it starts with, and whether an override must be journaled for
/// the position once the creation save lands.
struct NewBackendOffload {
    raw_flags: Option<String>,
    record_override: bool,
    placed_ccs: Vec<(u32, u32)>,
}

/// Validate a chosen expert-offload position for a backend being
/// created and compute its starting raw flags (tracker #49). The
/// position goes through the same capability and guardrail
/// evaluation backend_set_offload applies to an existing backend:
/// the model must be offload-capable, the position must fit its
/// MoE layer count, and a guardrail-blocked position is refused
/// unless the operator confirmed the override in the add form. The
/// refusal wording is the creation path's own (there is no Edit
/// Flags form for a backend that does not exist yet).
fn resolve_new_backend_offload(
    cfg: &AppConfig,
    backend: &manager_config::BackendConfig,
    gpus: &[GpuInfo],
    journal: &activation::ActivationJournal,
    layers: u32,
    allow_override: bool,
) -> Result<NewBackendOffload, ApiError> {
    let info = backend_offload_info(cfg, backend).ok_or_else(|| {
        ApiError::InvalidInput(format!(
            "backend {}'s model does not support expert offload",
            backend.id
        ))
    })?;
    if layers > info.moe_layers {
        return Err(ApiError::InvalidInput(format!(
            "offload of {layers} layers exceeds the model's {} MoE layers",
            info.moe_layers
        )));
    }
    let (placed_ccs, cpu_only) = placed_offload_placement(backend, gpus);
    let eval = evaluate_offload_position(
        &info.restrictions,
        &backend.id,
        layers,
        info.moe_layers,
        placed_ccs,
        cpu_only,
        journal,
    );
    let blocked = eval.status == manager_models::OffloadStatus::Blocked;
    if blocked && !allow_override {
        // Blocked implies a placed CC matches a restriction (the
        // status function blocks only on a placed match); the
        // restricting CC names the architecture in the message.
        let cc = eval.restricting_cc.unwrap_or((0, 0));
        return Err(ApiError::InvalidInput(format!(
            "offload of {layers} of {} layers is outside the verified range for this GPU (compute capability {}.{}) — choose {} of {} layers, or confirm the position with an explicit override",
            info.moe_layers, cc.0, cc.1, eval.nearest_verified_layers, info.moe_layers
        )));
    }
    let target = manager_models::offload_for_layers(layers, info.moe_layers);
    let raw_flags = manager_models::set_expert_offload(backend.raw_flags.as_deref(), target);
    Ok(NewBackendOffload {
        raw_flags,
        record_override: blocked,
        placed_ccs: eval.placed_ccs,
    })
}

/// The `--split-mode` spelling for a structured split mode.
fn split_mode_str(mode: manager_config::SplitMode) -> &'static str {
    match mode {
        manager_config::SplitMode::None => "none",
        manager_config::SplitMode::Layer => "layer",
        manager_config::SplitMode::Row => "row",
    }
}

/// Create a new backend and route it through the gateway (spec §14
/// `backend_add`). The backend is created with the chosen registered
/// model and the standard restart policy; it joins an existing
/// gateway group or a new one created for it. The gateway picks the
/// route up when it next (re)starts — its config is generated from
/// this state.
///
/// The launch parameters the add form offers (tracker #49) ride the
/// optional arguments; each `None` keeps the pre-#49 creation
/// default exactly: 32K context, 512 batch, a tensor split assigned
/// from live telemetry when available, and the model's catalog
/// default expert offload. A chosen offload position is validated
/// against the new backend's placement guardrails exactly as
/// `backend_set_offload` validates it for an existing backend —
/// a guardrail-blocked position is refused unless
/// `offload_override` is set, in which case the override is
/// journaled once the creation save lands. `gpu_indices` replaces
/// the telemetry assignment with an even split across the selected
/// GPUs (the Placement form's composition for an explicit
/// selection). A chosen parallel slot count (tracker #61) is
/// written into the created backend's raw flags as part of the
/// same single creation save; `None` writes no `--parallel` token,
/// leaving the new backend at llama.cpp's default exactly as
/// before.
#[allow(clippy::too_many_arguments)] // argument list is the registry contract
/// The `opt-exp-*` backend namespace is reserved for Optimize
/// experiment temp backends (code review B-3): experiment
/// cleanup stops and unregisters any backend carrying such an
/// id, and the startup sweep deletes it from the config, so a
/// user backend created with one would be destroyed by
/// machinery that assumes it is temporary. User-facing creation
/// paths refuse the prefix with this error, which names the
/// reservation.
fn reserved_temp_backend_id_error(id: &str) -> ApiError {
    ApiError::InvalidInput(format!(
        "backend id {id:?} is reserved: ids beginning with {:?} are reserved \
         for Optimize experiment temp backends",
        manager_optimize::TEMP_BACKEND_ID_PREFIX
    ))
}

#[allow(clippy::too_many_arguments)] // argument list is the registry contract
pub async fn op_backend_add(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    model_id: String,
    port: u16,
    group: String,
    create_group: bool,
    enabled: bool,
    n_ctx: Option<u32>,
    n_batch: Option<u32>,
    offload_layers: Option<u32>,
    offload_override: Option<bool>,
    gpu_indices: Option<Vec<u32>>,
    parallel: Option<u32>,
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
        return Err(ApiError::InvalidInput(
            "backend name must be lowercase letters, digits, and hyphens".to_string(),
        ));
    }
    if manager_optimize::is_temp_backend_id(&id) {
        return Err(reserved_temp_backend_id_error(&id));
    }
    if !valid_slug(&group) {
        return Err(ApiError::InvalidInput(
            "group name must be lowercase letters, digits, and hyphens".to_string(),
        ));
    }
    if port == 0 {
        return Err(ApiError::InvalidInput(
            "port must be non-zero".to_string(),
        ));
    }
    // Context/batch: the add form's values when chosen, the seeded
    // defaults otherwise; validated by the same rules
    // backend_update_flags applies (the batch bound reads the
    // EFFECTIVE context, so an oversized batch against a small
    // chosen context is refused here, not at first start).
    let eff_ctx = n_ctx.unwrap_or(32768);
    let eff_batch = n_batch.unwrap_or(512);
    validate_ctx_batch(eff_ctx, eff_batch)?;
    // Parallel slots: validated by the same rules
    // backend_set_parallel applies, when the add form chose a
    // count.
    if let Some(slots) = parallel {
        validate_parallel_slots(slots)?;
    }
    // GPU inventory, fetched before the config write lock when the
    // request needs it (an explicit GPU selection to validate, or
    // an offload position to evaluate against the placed GPUs'
    // guardrails). The telemetry accessor reads the config while
    // initializing, so it must never be called under the lock.
    let gpu_inventory: Vec<GpuInfo> = if gpu_indices.is_some() || offload_layers.is_some() {
        let store = state.telemetry().await;
        let inventory = lock_telemetry(&store).inventory();
        inventory
    } else {
        Vec::new()
    };
    // Placement for the new backend, likewise resolved before the
    // write lock: the operator's GPU selection when one was made,
    // else the telemetry assignment.
    let (tensor_split, split_mode) = match &gpu_indices {
        Some(selected) => {
            let indices: Vec<u32> = gpu_inventory.iter().map(|g| g.index).collect();
            split_for_gpu_selection(&indices, selected)?
        }
        None => match propose_new_backend_split(state, &model_id).await {
            Some((split, mode)) => (split, mode),
            None => (Vec::new(), manager_config::SplitMode::None),
        },
    };
    let mut cfg = state.config.write().await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    if new_cfg.backends.iter().any(|b| b.id == id) {
        return Err(ApiError::InvalidInput(format!(
            "a backend with id '{id}' already exists"
        )));
    }
    let model = new_cfg
        .models
        .iter()
        .find(|m| m.id == model_id)
        .ok_or_else(|| ApiError::NotFound(format!("model {model_id}")))?
        .clone();
    if create_group {
        if new_cfg.gateway.groups.iter().any(|g| g.name == group) {
            return Err(ApiError::InvalidInput(format!(
                "group '{group}' already exists; join it instead of creating it"
            )));
        }
        new_cfg.gateway.groups.push(manager_config::ModelGroup {
            name: group.clone(),
            members: vec![id.clone()],
            strategy: manager_config::RoutingStrategy::SimpleShuffle,
            fallbacks: Vec::new(),
            purpose: String::new(),
        });
    } else {
        let g = new_cfg
            .gateway
            .groups
            .iter_mut()
            .find(|g| g.name == group)
            .ok_or_else(|| ApiError::NotFound(format!("group {group}")))?;
        if !g.members.iter().any(|m| m == &id) {
            g.members.push(id.clone());
        }
    }
    let mut backend = manager_config::BackendConfig {
        id: id.clone(),
        enabled,
        model_file: model.gguf_path.clone(),
        port,
        flags: manager_config::ServerFlags {
            n_ctx: eff_ctx,
            n_batch: eff_batch,
            tensor_split,
            split_mode,
        },
        // A new backend starts with its model's default expert
        // offload (the green limit), when one is pinned — by the
        // catalog, or by the operator's declaration on the model
        // record for a model the catalog does not carry — e.g. a
        // backend created for Nemotron starts at --n-cpu-moe 16
        // rather than with no offload at all. A position chosen
        // in the add form replaces it below.
        raw_flags: model_default_raw_flags(&model),
        // A created backend starts in structured flags mode; the
        // operator opts into advanced mode per backend afterward.
        flags_advanced: false,
        restart_policy: manager_config::RestartPolicy {
            max_retries: 5,
            backoff_base_secs: 2,
        },
        overrides_global: false,
        tool_call_probe: None,
        purpose: String::new(),
    };
    // A chosen offload position is resolved against the constructed
    // backend (its final placement drives the guardrail evaluation)
    // before anything is journaled or saved, so a refused position
    // leaves no trace. The override, when the operator confirmed a
    // blocked position, is journaled only after the creation save
    // lands — mirroring backend_set_offload, no override ever
    // covers a change that did not persist.
    let mut offload_override_to_record: Option<(u32, Vec<(u32, u32)>)> = None;
    if let Some(layers) = offload_layers {
        let journal = activation::load_journal(&state.data_dir);
        let resolved = resolve_new_backend_offload(
            &new_cfg,
            &backend,
            &gpu_inventory,
            &journal,
            layers,
            offload_override == Some(true),
        )?;
        backend.raw_flags = resolved.raw_flags;
        if resolved.record_override {
            offload_override_to_record = Some((layers, resolved.placed_ccs));
        }
    }
    // A chosen parallel slot count is written into the constructed
    // backend's raw flags — after the offload resolution above,
    // which rewrites the same string — so the creation save below
    // carries it with everything else. An absent choice writes no
    // token: creation stays byte-identical to the pre-#61 shape.
    if let Some(slots) = parallel {
        backend.raw_flags = manager_models::set_parallel_slots(
            backend.raw_flags.as_deref(),
            Some(i64::from(slots)),
        );
    }
    new_cfg.backends.push(backend);
    if let Some(m) = new_cfg.models.iter_mut().find(|m| m.id == model_id) {
        if !m.assigned_backends.contains(&id) {
            m.assigned_backends.push(id.clone());
        }
    }
    manager_config::validate(&new_cfg)
        .map_err(ApiError::from)?;
    record_gateway_change(
        state,
        &pre_cfg,
        &new_cfg,
        "backend_add",
        format!("Backend `{id}` added to group `{group}`"),
        false,
    )
    .await
    ?;
    manager_config::save(&new_cfg, &state.config_path)
        .map_err(ApiError::from)?;
    *cfg = new_cfg;
    drop(cfg);
    if let Some((layers, placed_ccs)) = offload_override_to_record {
        let _ = activation::record_offload_override(&state.data_dir, &id, layers, placed_ccs);
    }
    ensure_backend_specs(state).await;
    state.sync_backend_service_definition(&id).await;
    Ok(json!({ "backend_id": id, "group": group, "persisted": true }))
}


/// Preview what creating a backend for `model_id` would carry
/// (tracker #49): the expert-offload slider state for the model —
/// the same `offload` object list_backends rows carry, computed
/// over a transient backend holding the creation defaults (the
/// catalog default offload position, the placement creation would
/// use) — plus that placement itself (`tensor_split` /
/// `split_mode`). The add form renders its offload control and
/// Placement chips from this, so the model-dependent behavior
/// (slider range, guardrail states) is the edit form's, computed by
/// the same code. `gpu_indices` previews an explicit GPU selection
/// instead of the telemetry assignment. Nothing is written.
pub async fn op_backend_add_preview(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    model_id: String,
    gpu_indices: Option<Vec<u32>>,
) -> ApiResult {
    let cfg = state.config.read().await.clone();
    let model = cfg
        .models
        .iter()
        .find(|m| m.id == model_id)
        .ok_or_else(|| ApiError::NotFound(format!("model {model_id}")))?
        .clone();
    // The GPU inventory serves both call sites below (selection
    // validation, guardrail evaluation). cfg is a clone, so no
    // config lock is held while telemetry is touched.
    let gpu_inventory: Vec<GpuInfo> = {
        let store = state.telemetry().await;
        let inventory = lock_telemetry(&store).inventory();
        inventory
    };
    let (tensor_split, split_mode) = match &gpu_indices {
        Some(selected) => {
            let indices: Vec<u32> = gpu_inventory.iter().map(|g| g.index).collect();
            split_for_gpu_selection(&indices, selected)?
        }
        None => match propose_new_backend_split(state, &model_id).await {
            Some((split, mode)) => (split, mode),
            None => (Vec::new(), manager_config::SplitMode::None),
        },
    };
    // The transient backend creation would build, at its defaults.
    let backend = manager_config::BackendConfig {
        id: String::new(),
        enabled: true,
        model_file: model.gguf_path.clone(),
        port: 0,
        flags: manager_config::ServerFlags {
            n_ctx: 32768,
            n_batch: 512,
            tensor_split: tensor_split.clone(),
            split_mode,
        },
        raw_flags: model_default_raw_flags(&model),
        flags_advanced: false,
        restart_policy: manager_config::RestartPolicy {
            max_retries: 5,
            backoff_base_secs: 2,
        },
        overrides_global: false,
        tool_call_probe: None,
        purpose: String::new(),
    };
    let offload = backend_offload_info(&cfg, &backend).map(|info| {
        let journal = activation::load_journal(&state.data_dir);
        let eval = evaluate_backend_offload(&cfg, &backend, &gpu_inventory, &journal);
        offload_row_json(&info, eval.as_ref())
    });
    Ok(json!({
        "model_id": model_id,
        "offload": offload,
        "tensor_split": tensor_split,
        "split_mode": split_mode_str(split_mode),
    }))
}

/// Update a backend's llama-server sizing flags: context length
/// (`-c`) and batch size (`-b`). This is the supported way to change
/// them after a backend is created — before this command the only
/// paths were hand-editing config.json or removing and re-adding the
/// backend. Takes effect on the backend's next start; a running
/// backend keeps its current values until it is restarted.
pub async fn op_backend_update_flags(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    n_ctx: u32,
    n_batch: u32,
) -> ApiResult {
    validate_ctx_batch(n_ctx, n_batch)?;
    let mut cfg = state.config.write().await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    let b = new_cfg
        .backends
        .iter_mut()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    // Advanced flags mode (tracker #60): context/batch are the
    // flags form's fields; the form is inert in the mode.
    ensure_backend_not_advanced(b)?;
    let changed = b.flags.n_ctx != n_ctx || b.flags.n_batch != n_batch;
    b.flags.n_ctx = n_ctx;
    b.flags.n_batch = n_batch;
    manager_config::validate(&new_cfg)
        .map_err(ApiError::from)?;
    if changed {
        record_backend_change(
            state,
            &pre_cfg,
            &new_cfg,
            "backend_flags",
            format!("Backend `{id}` context/batch changed"),
            &id,
        )
        .await
        ?;
    }
    manager_config::save(&new_cfg, &state.config_path)
        .map_err(ApiError::from)?;
    *cfg = new_cfg;
    drop(cfg);
    ensure_backend_specs(state).await;
    state.sync_backend_service_definition(&id).await;
    Ok(json!({ "backend_id": id, "persisted": true, "applies": "next start" }))
}


/// The raw flags a backend serving `model_id` starts with: the
/// model's catalog default expert offload (its green limit — see
/// [`manager_models::catalog_default_offload`]) in canonical form,
/// when the catalog pins one. Used where a backend gains a model
/// without operator flags of its own: backend creation, model
/// assignment, and the catalog definition refresh.
fn catalog_default_raw_flags(model_id: &str) -> Option<String> {
    manager_models::curated_models()
        .into_iter()
        .find(|s| s.id == model_id)
        .and_then(|s| {
            manager_models::offload_flag_string(manager_models::catalog_default_offload(&s))
        })
}

/// The raw flags a backend serving `model` starts with, resolved
/// against the model record as well as the catalog: the catalog
/// default when the catalog pins one for the model's id (see
/// [`catalog_default_raw_flags`]); otherwise the operator's
/// declared default on the model record
/// ([`ModelConfig::moe_layers`] /
/// [`ModelConfig::default_offload_layers`]) in canonical form —
/// the path that gives a user-added MoE model the same creation
/// default a catalog model gets. `None` when neither source pins
/// a default that writes tokens.
fn model_default_raw_flags(model: &manager_config::ModelConfig) -> Option<String> {
    if let Some(flags) = catalog_default_raw_flags(&model.id) {
        return Some(flags);
    }
    let layers = model.moe_layers?;
    let default = model.default_offload_layers?;
    if layers == 0 {
        return None;
    }
    manager_models::offload_flag_string(manager_models::offload_for_layers(
        default.min(layers),
        layers,
    ))
}

/// Set a backend's expert offload (the Edit Flags form's offload
/// slider): rewrite only the offload tokens in the backend's raw
/// flags to the canonical form for `layers` MoE layers on system
/// RAM — 0 is no offload, the full MoE layer count is the
/// `-ot exps=CPU` form, anything between is `--n-cpu-moe N` —
/// leaving every other flag untouched. The model must be
/// offload-capable (see [`backend_offload_info`]). Saved through
/// the same clone-validate-save path as the other backend
/// mutations and journaled as a backend-scope activation change;
/// applies on the backend's next start.
///
/// Guardrails (tracker #37): when the requested position is
/// blocked for the model's pinned restriction on the GPUs the
/// backend currently runs on, the save is refused unless
/// `allow_override` is set — in which case a durable override is
/// journaled (see `activation::OffloadOverride`). A non-blocked
/// save clears any standing override for the backend.
pub async fn op_backend_set_offload(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    layers: u32,
    allow_override: Option<bool>,
) -> ApiResult {
    // Capability is resolved before the write lock (resolution can
    // parse a GGUF header); the layer count is validated against
    // it here and the flags are rewritten under the lock.
    let moe_layers = {
        let cfg = state.config.read().await;
        let b = cfg
            .backends
            .iter()
            .find(|b| b.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
        // Advanced flags mode (tracker #60): the slider rewrites
        // raw-flag tokens the operator owns as text in the mode.
        ensure_backend_not_advanced(b)?;
        backend_offload_info(&cfg, b)
            .map(|o| o.moe_layers)
            .ok_or_else(|| {
                ApiError::InvalidInput(format!(
                    "backend {id}'s model does not support expert offload"
                ))
            })?
    };
    if layers > moe_layers {
        return Err(ApiError::InvalidInput(format!(
            "offload of {layers} layers exceeds the model's {moe_layers} MoE layers"
        )));
    }
    // Guardrail inputs fetched between the config reads: the GPU
    // inventory (telemetry access must never hold the config lock —
    // see ensure_backend_specs) and the activation journal (the
    // standing override records live there).
    let gpus = {
        let store = state.telemetry().await;
        let inventory = lock_telemetry(&store).inventory();
        inventory
    };
    let journal = activation::load_journal(&state.data_dir);
    let mut cfg = state.config.write().await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    // Re-resolve and re-validate UNDER the write lock (code-review
    // 4.8): the pre-lock resolution can be stale — a concurrent
    // model assignment changes which model this backend serves,
    // and with it the MoE layer count the request was validated
    // against. Computed before the mutable borrow is taken, and it
    // shadows the pre-lock value: everything below uses the
    // under-lock count.
    let eval = {
        let b_ref = new_cfg
            .backends
            .iter()
            .find(|x| x.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
        // Re-checked under the write lock (tracker #60): the mode
        // can have been entered since the pre-lock check.
        ensure_backend_not_advanced(b_ref)?;
        let info = backend_offload_info(&new_cfg, b_ref).ok_or_else(|| {
            ApiError::InvalidInput(format!(
                "backend {id}'s model does not support expert offload"
            ))
        })?;
        let (placed_ccs, cpu_only) = placed_offload_placement(b_ref, &gpus);
        evaluate_offload_position(
            &info.restrictions,
            &id,
            layers,
            info.moe_layers,
            placed_ccs,
            cpu_only,
            &journal,
        )
    };
    let moe_layers = eval.moe_layers;
    // Guardrail enforcement (tracker #37), still before any
    // mutation: a blocked requested position refuses the save
    // unless the operator explicitly overrides it. An existing
    // override does NOT grandfather a fresh save — the override is
    // requested again at each blocked save, so the decision point
    // stays where the UI can explain it.
    let eval_blocked = eval.status == manager_models::OffloadStatus::Blocked;
    if eval_blocked && allow_override != Some(true) {
        if let Some(reason) = offload_block_reason(
            &eval.restrictions,
            layers,
            moe_layers,
            &eval.placed_ccs,
            eval.cpu_only,
            false,
        ) {
            return Err(ApiError::InvalidInput(reason));
        }
    }
    let b = new_cfg
        .backends
        .iter_mut()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    if layers > moe_layers {
        return Err(ApiError::InvalidInput(format!(
            "offload of {layers} layers exceeds the model's {moe_layers} MoE layers"
        )));
    }
    let target = manager_models::offload_for_layers(layers, moe_layers);
    let new_raw = manager_models::set_expert_offload(b.raw_flags.as_deref(), target);
    let changed = new_raw != b.raw_flags;
    b.raw_flags = new_raw.clone();
    if changed {
        // A raw-flags change invalidates the recorded tool-call
        // probe (MCP design §7.2).
        b.tool_call_probe = None;
    }
    if changed {
        manager_config::validate(&new_cfg)
            .map_err(ApiError::from)?;
        let summary = match target {
            manager_models::ExpertOffload::None => {
                format!("Backend `{id}` expert offload turned off")
            }
            manager_models::ExpertOffload::AllExperts => {
                format!("Backend `{id}` expert offload set to all {moe_layers} MoE layers")
            }
            manager_models::ExpertOffload::CpuMoeLayers(n) => {
                format!("Backend `{id}` expert offload set to {n} of {moe_layers} MoE layers")
            }
        };
        record_backend_change(
            state,
            &pre_cfg,
            &new_cfg,
            "backend_set_offload",
            summary,
            &id,
        )
        .await
        ?;
        manager_config::save(&new_cfg, &state.config_path)
            .map_err(ApiError::from)?;
        *cfg = new_cfg;
    }
    drop(cfg);
    // Override bookkeeping: the command has succeeded (a config
    // save error above returns before this point, so no override
    // is ever recorded for a save that did not land). It applies
    // even when `changed` is false — authorizing a standing value
    // is the point of the override. A blocked save that proceeded
    // records the override at the placed CCs evaluated under the
    // lock; any non-blocked save clears a standing override (a
    // verified save supersedes it).
    let mut overridden = false;
    if eval_blocked {
        if activation::record_offload_override(
            &state.data_dir,
            &id,
            layers,
            eval.placed_ccs.clone(),
        )
        .is_ok()
        {
            overridden = true;
        }
    } else {
        let _ = activation::clear_offload_overrides(&state.data_dir, &id);
    }
    if changed {
        ensure_backend_specs(state).await;
    }
    state.sync_backend_service_definition(&id).await;
    Ok(json!({
        "backend_id": id,
        "layers": layers,
        "raw_flags": new_raw,
        "changed": changed,
        "overridden": overridden,
        // `persisted` is about the desired state being on disk:
        // after a successful save it is, and when nothing changed
        // it already was (code-review 4.16 — reporting false for
        // the already-correct state made the UI warn about a loss
        // that never happened).
        "persisted": true,
        "applies": "next start",
    }))
}

/// Set a backend's MTP speculative decoding (tracker #53; the
/// flags form's MTP draft control): rewrite only the MTP tokens
/// in the backend's raw flags — `n_max` of 1..=4 enables
/// `--spec-type draft-mtp` with that `--spec-draft-n-max`;
/// `None` turns MTP off. Every other flag is preserved,
/// including `--parallel`: MTP drafting needs a single slot,
/// but the control surfaces that conflict rather than rewriting
/// the operator's slot setting (the no-silent-rewrite rule the
/// offload guardrails follow). The backend's model must be
/// MTP-capable (see [`backend_mtp_info`]). Saved through the
/// same clone-validate-save path as the other backend mutations
/// and journaled as a backend-scope activation change with
/// `applies: "next start"`; no restart happens here.
pub async fn op_backend_set_mtp(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    n_max: Option<u32>,
) -> ApiResult {
    use manager_models::mtp::{MTP_N_MAX_MAX, MTP_N_MAX_MIN};
    if let Some(n) = n_max {
        if !(MTP_N_MAX_MIN..=MTP_N_MAX_MAX).contains(&n) {
            return Err(ApiError::InvalidInput(format!(
                "MTP draft length {n} is outside the control's range \
                 {MTP_N_MAX_MIN}..={MTP_N_MAX_MAX}"
            )));
        }
    }
    // Capability is resolved against the current config before
    // the write lock is taken, and re-resolved under the lock
    // below (a concurrent model assignment can change which
    // model this backend serves — the same shape as
    // op_backend_set_offload).
    {
        let cfg = state.config.read().await;
        let b = cfg
            .backends
            .iter()
            .find(|b| b.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
        // Advanced flags mode (tracker #60): the control rewrites
        // raw-flag tokens the operator owns as text in the mode.
        ensure_backend_not_advanced(b)?;
        if backend_mtp_info(&cfg, b).is_none() {
            return Err(ApiError::InvalidInput(format!(
                "backend {id}'s model does not support MTP drafting"
            )));
        }
    }
    let mut cfg = state.config.write().await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    {
        // Re-validate capability against the config actually
        // being mutated.
        let b = new_cfg
            .backends
            .iter()
            .find(|b| b.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
        // Re-checked under the write lock (tracker #60), with the
        // capability re-validation.
        ensure_backend_not_advanced(b)?;
        if backend_mtp_info(&new_cfg, b).is_none() {
            return Err(ApiError::InvalidInput(format!(
                "backend {id}'s model does not support MTP drafting"
            )));
        }
    }
    let b = new_cfg
        .backends
        .iter_mut()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    let target = match n_max {
        Some(n) => manager_models::MtpDraft::On(Some(n)),
        None => manager_models::MtpDraft::Off,
    };
    let new_raw = manager_models::set_mtp_draft(b.raw_flags.as_deref(), target);
    let changed = new_raw != b.raw_flags;
    b.raw_flags = new_raw.clone();
    if changed {
        // A raw-flags change invalidates the recorded tool-call
        // probe (MCP design §7.2).
        b.tool_call_probe = None;
    }
    if changed {
        manager_config::validate(&new_cfg)
            .map_err(ApiError::from)?;
        let summary = match n_max {
            Some(n) => format!("Backend `{id}` MTP draft set to {n} tokens"),
            None => format!("Backend `{id}` MTP draft turned off"),
        };
        record_backend_change(
            state,
            &pre_cfg,
            &new_cfg,
            "backend_set_mtp",
            summary,
            &id,
        )
        .await
        ?;
        manager_config::save(&new_cfg, &state.config_path)
            .map_err(ApiError::from)?;
        *cfg = new_cfg;
    }
    drop(cfg);
    if changed {
        ensure_backend_specs(state).await;
    }
    state.sync_backend_service_definition(&id).await;
    Ok(json!({
        "backend_id": id,
        "n_max": n_max,
        "raw_flags": new_raw,
        "changed": changed,
        // `persisted` is about the desired state being on disk:
        // after a successful save it is, and when nothing changed
        // it already was (same note as backend_set_offload).
        "persisted": true,
        "applies": "next start",
    }))
}

/// Set a backend's parallel slot count (tracker #61; the flags
/// form's Parallel slots field): rewrite only the `--parallel`
/// tokens in the backend's raw flags — `slots` in the control's
/// range writes the canonical `--parallel N`; `None` removes the
/// token, returning the backend to llama.cpp's default (auto).
/// Every other flag is preserved. This is the one write path for
/// the setting: the MTP control surfaces a conflicting slot count
/// but never rewrites it, so a slot change is always an explicit
/// operator action in this field. Saved through the same
/// clone-validate-save path as the other backend mutations and
/// journaled as a backend-scope activation change with
/// `applies: "next start"`; no restart happens here.
pub async fn op_backend_set_parallel(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    slots: Option<u32>,
) -> ApiResult {
    if let Some(n) = slots {
        validate_parallel_slots(n)?;
    }
    let mut cfg = state.config.write().await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    let b = new_cfg
        .backends
        .iter_mut()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    let new_raw =
        manager_models::set_parallel_slots(b.raw_flags.as_deref(), slots.map(i64::from));
    let changed = new_raw != b.raw_flags;
    b.raw_flags = new_raw.clone();
    if changed {
        // A raw-flags change invalidates the recorded tool-call
        // probe (MCP design §7.2).
        b.tool_call_probe = None;
    }
    if changed {
        manager_config::validate(&new_cfg)
            .map_err(ApiError::from)?;
        let summary = match slots {
            Some(n) => format!("Backend `{id}` parallel slots set to {n}"),
            None => {
                format!("Backend `{id}` parallel slots returned to the llama.cpp default (auto)")
            }
        };
        record_backend_change(
            state,
            &pre_cfg,
            &new_cfg,
            "backend_set_parallel",
            summary,
            &id,
        )
        .await
        ?;
        manager_config::save(&new_cfg, &state.config_path)
            .map_err(ApiError::from)?;
        *cfg = new_cfg;
    }
    drop(cfg);
    if changed {
        ensure_backend_specs(state).await;
    }
    state.sync_backend_service_definition(&id).await;
    Ok(json!({
        "backend_id": id,
        "slots": slots,
        "raw_flags": new_raw,
        "changed": changed,
        // `persisted` is about the desired state being on disk:
        // after a successful save it is, and when nothing changed
        // it already was (same note as backend_set_offload).
        "persisted": true,
        "applies": "next start",
    }))
}


/// Turn a backend's advanced flags mode on or off (tracker #60).
///
/// The mode is editor state, and flipping it is deliberately
/// minimal: it writes ONLY the `flags_advanced` field — never the
/// backend's flags, by a single token. Entering advanced mode
/// cannot canonicalize the string the structured setters would
/// rewrite (they rewrite on save; this op performs no flags save),
/// so the string the operator sees on entry is exactly the string
/// that was live before. Leaving writes nothing either: the raw
/// string stays as saved, and the structured form re-decodes it
/// through the existing decoders (ServerFlags, expert offload,
/// MTP) when it renders.
///
/// The flip is NOT journaled as an activation change: by itself it
/// alters nothing a restart would activate — the journaled event
/// is the raw-flags save that follows it. For the same reason the
/// tool-call probe is untouched (the serving configuration did
/// not change) and no service definition is re-synced (the launch
/// spec does not carry the mode).
pub async fn op_backend_set_flags_mode(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    advanced: bool,
) -> ApiResult {
    let mut cfg = state.config.write().await;
    let mut new_cfg = cfg.clone();
    let b = new_cfg
        .backends
        .iter_mut()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    let changed = b.flags_advanced != advanced;
    b.flags_advanced = advanced;
    let raw_flags = b.raw_flags.clone();
    if changed {
        manager_config::validate(&new_cfg)
            .map_err(ApiError::from)?;
        manager_config::save(&new_cfg, &state.config_path)
            .map_err(ApiError::from)?;
        *cfg = new_cfg;
    }
    Ok(json!({
        "backend_id": id,
        "flags_advanced": advanced,
        "raw_flags": raw_flags,
        "changed": changed,
        "persisted": true,
    }))
}

/// Save a backend's raw flags edited as text (tracker #60, the
/// advanced flags editor's save). Only available while the
/// backend is in advanced mode: in structured mode the raw string
/// is written by the token-owning setters and the config patch,
/// and this op refuses rather than becoming a second writer.
///
/// Validation is warn, never block: the string is checked with
/// [`manager_models::raw_flag_warnings`] and the warnings ride
/// the response, but any string saves — an unrecognized flag may
/// be exactly what the operator needs, and llama-server's own
/// start is the real check. The input is normalized the way the
/// launch tokenizer sees it: leading/trailing whitespace is not
/// content, and an all-whitespace string is no flags at all
/// (`None`) — the same state the structured setters produce when
/// their tokens are removed.
///
/// Saved through the same clone-validate-save path as the other
/// flags setters and journaled as a backend-scope activation
/// change (kind `backend_set_raw_flags`); a change clears the
/// recorded tool-call probe (MCP design §7.2), as every raw-flags
/// write does. Applies on the backend's next start; nothing is
/// restarted here.
pub async fn op_backend_set_raw_flags(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    raw_flags: Option<String>,
) -> ApiResult {
    let new_raw: Option<String> = raw_flags
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let warnings = manager_models::raw_flag_warnings(new_raw.as_deref());
    let mut cfg = state.config.write().await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    let b = new_cfg
        .backends
        .iter_mut()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    if !b.flags_advanced {
        return Err(ApiError::InvalidInput(format!(
            "backend {id} is not in advanced flags mode: in structured mode \
             its raw flags are written by the structured flags controls; \
             turn advanced mode on (backend_set_flags_mode) to edit them \
             as text"
        )));
    }
    let changed = b.raw_flags != new_raw;
    b.raw_flags = new_raw.clone();
    if changed {
        // A raw-flags change invalidates the recorded tool-call
        // probe (MCP design §7.2).
        b.tool_call_probe = None;
        manager_config::validate(&new_cfg)
            .map_err(ApiError::from)?;
        record_backend_change(
            state,
            &pre_cfg,
            &new_cfg,
            "backend_set_raw_flags",
            format!("Backend `{id}` raw flags changed"),
            &id,
        )
        .await
        ?;
        manager_config::save(&new_cfg, &state.config_path)
            .map_err(ApiError::from)?;
        *cfg = new_cfg;
    }
    drop(cfg);
    if changed {
        ensure_backend_specs(state).await;
    }
    state.sync_backend_service_definition(&id).await;
    Ok(json!({
        "backend_id": id,
        "raw_flags": new_raw,
        "changed": changed,
        "warnings": warnings,
        // `persisted` is about the desired state being on disk:
        // after a successful save it is, and when nothing changed
        // it already was (same note as backend_set_offload).
        "persisted": true,
        "applies": "next start",
    }))
}

/// Remove a backend (spec §14 `backend_delete`). Refused while the
/// backend is running — stop it first. The id is scrubbed from every
/// group's member list (the gateway config generator rejects unknown
/// members), from every group's fallback chain (config validation
/// rejects unknown fallbacks, so a delete that left one behind could
/// never be saved), and from models' assignment lists. A group left
/// with no members stays in the config: it generates no gateway
/// deployments and can gain members again later.
pub async fn op_backend_delete(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    {
        let sup = state.supervisor().await;
        let mut sup = sup.lock().await;
        sup.reap_exited();
        if let Some(st) = sup.status().iter().find(|s| s.id == id) {
            if st.pid.is_some() {
                return Err(ApiError::InvalidInput(format!(
                    "backend {id} is running; stop it before removing it"
                )));
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
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    new_cfg.backends.remove(pos);
    for g in new_cfg.gateway.groups.iter_mut() {
        g.members.retain(|m| m != &id);
        g.fallbacks.retain(|f| f != &id);
    }
    for m in new_cfg.models.iter_mut() {
        m.assigned_backends.retain(|b| b != &id);
    }
    manager_config::validate(&new_cfg)
        .map_err(ApiError::from)?;
    record_gateway_change(
        state,
        &pre_cfg,
        &new_cfg,
        "backend_delete",
        format!("Backend `{id}` removed"),
        false,
    )
    .await
    ?;
    manager_config::save(&new_cfg, &state.config_path)
        .map_err(ApiError::from)?;
    *cfg = new_cfg;
    drop(cfg);
    ensure_backend_specs(state).await;
    // A deleted backend's service registration is the operator's to
    // remove (Settings → Supervision) — a config delete must not fire
    // a UAC prompt from a list action. Record the leftover so it is
    // discoverable rather than silent.
    if state
        .service_route_active(&manager_services::backend_service_name(&id))
        .await
    {
        append_wizard_log(
            &state.data_dir,
            &[format!(
                "[{}] backend {id} was deleted from the configuration but its Windows service is still registered; unregister it in Settings → Supervision",
                chrono::Utc::now().to_rfc3339()
            )],
        );
    }
    Ok(json!({ "backend_id": id, "deleted": true, "persisted": true }))
}


/// Move a backend to a group, or out of all groups
/// (predicted-throughput design §8.5 — the Groups page's drop and
/// "Move to" menu). Move semantics (decision D5): a backend is in
/// at most one group, so the id is removed from every group's
/// member list and appended to the target's; `group: None` is
/// ungroup. A hand-edited config that lists the backend in several
/// groups is normalized by the move — the response's
/// `removed_from` names every group it left, so the page can state
/// the collapse. Dropping a backend on the lane it solely occupies
/// (or ungrouping an ungrouped backend) succeeds without a config
/// write. Persists through the same clone-validate-save path as
/// `backend_add`/`backend_delete` and journals as a gateway-scope
/// change, so the Activation Plan applies the move at the next
/// gateway activation, exactly like every other membership change;
/// the gateway does not restart here.
pub async fn op_gateway_set_backend_group(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    group: Option<String>,
) -> ApiResult {
    let target: Option<String> = match group {
        Some(name) => {
            let name = name.trim().to_string();
            if name.is_empty() {
                return Err(ApiError::InvalidInput(
                    "group name must not be empty; pass null to ungroup the backend"
                        .to_string(),
                ));
            }
            Some(name)
        }
        None => None,
    };
    let mut cfg = state.config.write().await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    if !new_cfg.backends.iter().any(|b| b.id == id) {
        return Err(ApiError::NotFound(format!("backend {id}")));
    }
    if let Some(name) = &target {
        if !new_cfg.gateway.groups.iter().any(|g| &g.name == name) {
            return Err(ApiError::NotFound(format!("group {name}")));
        }
    }
    let mut removed_from: Vec<String> = Vec::new();
    let mut added = false;
    for g in new_cfg.gateway.groups.iter_mut() {
        let is_target = target.as_deref() == Some(g.name.as_str());
        if !is_target && g.members.iter().any(|m| m == &id) {
            g.members.retain(|m| m != &id);
            removed_from.push(g.name.clone());
        }
    }
    if let Some(name) = &target {
        let g = new_cfg
            .gateway
            .groups
            .iter_mut()
            .find(|g| &g.name == name)
            .expect("target group existence checked above");
        if !g.members.iter().any(|m| m == &id) {
            g.members.push(id.clone());
            added = true;
        }
    }
    let changed = added || !removed_from.is_empty();
    if changed {
        manager_config::validate(&new_cfg)
            .map_err(ApiError::from)?;
        let summary = match &target {
            Some(name) => format!("Backend `{id}` moved to group `{name}`"),
            None => format!("Backend `{id}` removed from all groups (ungrouped)"),
        };
        record_gateway_change(
            state,
            &pre_cfg,
            &new_cfg,
            "gateway_set_backend_group",
            summary,
            false,
        )
        .await
        ?;
        manager_config::save(&new_cfg, &state.config_path)
            .map_err(ApiError::from)?;
        *cfg = new_cfg;
    }
    Ok(json!({
        "backend_id": id,
        "group": target,
        "removed_from": removed_from,
        "changed": changed,
        // Same note as the flags setters: after a successful save
        // the desired state is on disk, and when nothing changed
        // it already was.
        "persisted": true,
    }))
}

/// Create an empty group (predicted-throughput design §8.5,
/// decision D11 — the Groups page's New group). The name follows
/// the same slug rule `backend_add` applies to the group it joins
/// or creates. The new group starts with the creation default
/// strategy and no members or fallbacks; it generates no gateway
/// deployments until a backend moves into it, so the save
/// journals an activation change only if the generated gateway
/// configuration actually moves (an empty group does not move
/// it — `record_gateway_change` decides, as for every gateway
/// edit).
pub async fn op_gateway_create_group(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    name: String,
) -> ApiResult {
    let name = name.trim().to_string();
    // The slug rule backend_add applies to group names (kept in
    // step deliberately: one naming rule for groups everywhere).
    let valid_slug = !name.is_empty()
        && name.len() <= 48
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if !valid_slug {
        return Err(ApiError::InvalidInput(
            "group name must be lowercase letters, digits, and hyphens".to_string(),
        ));
    }
    let mut cfg = state.config.write().await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    if new_cfg.gateway.groups.iter().any(|g| g.name == name) {
        return Err(ApiError::InvalidInput(format!(
            "group '{name}' already exists"
        )));
    }
    new_cfg.gateway.groups.push(manager_config::ModelGroup {
        name: name.clone(),
        members: Vec::new(),
        strategy: manager_config::RoutingStrategy::SimpleShuffle,
        fallbacks: Vec::new(),
        purpose: String::new(),
    });
    manager_config::validate(&new_cfg)
        .map_err(ApiError::from)?;
    record_gateway_change(
        state,
        &pre_cfg,
        &new_cfg,
        "gateway_create_group",
        format!("Group `{name}` created"),
        false,
    )
    .await
    ?;
    manager_config::save(&new_cfg, &state.config_path)
        .map_err(ApiError::from)?;
    *cfg = new_cfg;
    Ok(json!({ "name": name, "persisted": true }))
}

/// Delete a group (predicted-throughput design §8.5, decision
/// D11). Only an empty group deletes: a group with members is
/// refused with the member count, never cascaded — emptying a
/// group is a series of visible moves on the Groups page, not a
/// side effect of a delete. (Member here is the config's member
/// list, which includes `host:port` external endpoints; a group
/// holding only those is still not empty.) Same save path and
/// gateway journaling as the other group ops.
pub async fn op_gateway_delete_group(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    name: String,
) -> ApiResult {
    let name = name.trim().to_string();
    let mut cfg = state.config.write().await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    let pos = new_cfg
        .gateway
        .groups
        .iter()
        .position(|g| g.name == name)
        .ok_or_else(|| ApiError::NotFound(format!("group {name}")))?;
    let members = new_cfg.gateway.groups[pos].members.len();
    if members > 0 {
        return Err(ApiError::InvalidInput(format!(
            "group '{name}' is not empty: it has {members} member(s); \
             move them elsewhere before deleting the group"
        )));
    }
    new_cfg.gateway.groups.remove(pos);
    manager_config::validate(&new_cfg)
        .map_err(ApiError::from)?;
    record_gateway_change(
        state,
        &pre_cfg,
        &new_cfg,
        "gateway_delete_group",
        format!("Group `{name}` deleted"),
        false,
    )
    .await
    ?;
    manager_config::save(&new_cfg, &state.config_path)
        .map_err(ApiError::from)?;
    *cfg = new_cfg;
    Ok(json!({ "name": name, "deleted": true, "persisted": true }))
}

/// Exact argv the supervisor would spawn (must equal the wizard's
/// planned_commands output per spec).
pub async fn op_backend_get_launch_command(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    ensure_backend_specs(state).await;
    let argv = state
        .supervisor()
        .await
        .lock()
        .await
        .launch_argv(&id)
        .map_err(ApiError::from)?;
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
/// `groups` counts ENABLED groups only (a group is enabled iff at
/// least one member backend is enabled — the derived rule in
/// manager-gateway): the generated config serves enabled groups
/// only, so the configured total would misstate what the gateway
/// runs (Pete, 2026-10-17, tracker #11).
/// `routing_log_len` is the routing decision ring's actual current
/// entry count (not its capacity). `openrouter_cred_present` reports
/// whether the cloud tier's configured credential reference exists in
/// Windows Credential Manager (presence only — the value is never
/// read); it is null when no reference is configured.
pub async fn op_gateway_status(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let (port, bind_host, probe_host, groups, routing_rules, cred_ref) = {
        let cfg = state.config.read().await;
        (
            cfg.gateway.port,
            cfg.gateway.bind_host.clone(),
            manager_wizard::steps::gateway_probe_host(&cfg),
            manager_gateway::enabled_group_count(&cfg),
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
    // that are genuinely pending — a running process still holds
    // the pre-change state (pending_entries applies the live-state
    // rule: changes whose targets are all stopped, or whose
    // gateway is down, are already effective at the next start and
    // never count here) — or live config drift while the gateway
    // is up serving the stale file.
    let (activation_pending, activation_changes) = {
        let cfg = state.config.read().await;
        let journal = activation::load_journal(&state.data_dir);
        let started_at = backend_start_times(state).await;
        let running = backend_running_ids(state).await;
        let configured: std::collections::HashSet<String> =
            cfg.backends.iter().map(|b| b.id.clone()).collect();
        let pending = activation::pending_entries(
            &state.data_dir,
            &journal,
            &started_at,
            &configured,
            &running,
            up,
        );
        let presence = mcp_credential_presence(&[&cfg]).await;
        let drift = activation::config_drift(&cfg, &state.data_dir, |t| presence.contains(t));
        (
            !pending.is_empty() || (drift && up),
            // The count the activation plan itself reports:
            // journal entries plus the plan's synthesized drift
            // item when drift is pending work (tracker #57) —
            // never "pending" beside a zero count.
            pending.len() + usize::from(drift && up),
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
        // The LiteLLM version recorded by the last gateway spawn
        // (MCP design §4.3); null when no spawn state has recorded
        // one — the UI renders that as "not measured".
        "litellm_version": manager_wizard::recorded_litellm_version(&state.data_dir),
    }))
}


/// The activation plan: every saved change that is not live yet,
/// across scopes — gateway changes waiting on a gateway restart and
/// backend changes (flags, tensor split, model assignment) waiting
/// on their backend's (re)start — with the state the operator's
/// decision depends on. Each change carries its `activation`:
/// `gateway_restart`, `restart` (a running backend must restart),
/// or `next_start` (nothing running holds the old state; the change
/// takes effect when the relevant process next starts). Changes
/// whose targets are all stopped — or whose gateway is down — are
/// already effective by that same rule and are NOT part of the
/// plan: the plan exists only while a running process holds
/// pre-change state (activation::pending_entries). The payload's
/// `drift` follows that rule too: it reports live drift against
/// the RUNNING gateway only — while the gateway is down a stale
/// generated file is not pending work (it regenerates at the next
/// start) and is never stated as the plan's reason
/// (activation::drift_is_pending). When drift is the pending
/// work it also appears in `changes` as a synthesized
/// `gateway_drift` item (activation::drift_plan_change), so a
/// pending plan's change count is never zero (tracker #57); the
/// raw comparison rides along as `files_differ`, informational
/// in any gateway state.
pub async fn op_gateway_activation_plan(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let cfg = state.config.read().await.clone();
    let journal = activation::load_journal(&state.data_dir);
    let started_at = backend_start_times(state).await;
    let running = backend_running_ids(state).await;
    let configured: std::collections::HashSet<String> =
        cfg.backends.iter().map(|b| b.id.clone()).collect();
    let probe_host = manager_wizard::steps::gateway_probe_host(&cfg);
    let port = cfg.gateway.port;
    let up = tokio::task::spawn_blocking(move || probe_gateway(&probe_host, port))
        .await
        .unwrap_or(false);
    let pending_entries = activation::pending_entries(
        &state.data_dir,
        &journal,
        &started_at,
        &configured,
        &running,
        up,
    );
    let presence = mcp_credential_presence(&[&cfg]).await;
    // Drift enters the plan only as pending work: while the
    // gateway is down a stale generated file regenerates at its
    // next start, so the raw file comparison must not surface in
    // the plan — a backend-only plan otherwise asserts gateway
    // drift its apply never touches (tracker #45; the rule lives
    // in activation::drift_is_pending). The raw comparison rides
    // the payload separately as `files_differ`, informational
    // only, so the plan view can note a stale file the next start
    // will regenerate without calling it pending.
    let drift_detail =
        activation::config_drift_detail(&cfg, &state.data_dir, |t| presence.contains(t));
    let drift = activation::drift_is_pending(drift_detail.any(), up);
    let pending = !pending_entries.is_empty() || drift;
    let entry_ts = |e: &activation::ActivationEntry| {
        chrono::DateTime::parse_from_rfc3339(&e.ts)
            .ok()
            .map(|t| t.with_timezone(&chrono::Utc))
    };
    let mut backend_restart_ids: Vec<String> = Vec::new();
    let mut changes: Vec<Value> = pending_entries
        .iter()
        .map(|e| {
            let activation_kind = if e.scope == "backend" {
                // Needs a restart when a target is running a process
                // started before the change (or with unknown start —
                // an adopted process: restarting is the safe call).
                let ts = entry_ts(e);
                let stale = |t: &String| {
                    running.contains(t)
                        && started_at
                            .get(t)
                            .map(|s| ts.map(|ts| *s < ts).unwrap_or(true))
                            .unwrap_or(true)
                };
                if e.targets.iter().any(stale) {
                    for t in &e.targets {
                        if stale(t) && !backend_restart_ids.contains(t) {
                            backend_restart_ids.push(t.clone());
                        }
                    }
                    "restart"
                } else {
                    "next_start"
                }
            } else if up {
                "gateway_restart"
            } else {
                "next_start"
            };
            json!({
                "ts": e.ts,
                "kind": e.kind,
                "summary": e.summary,
                "scope": e.scope,
                "targets": e.targets,
                "activation": activation_kind,
            })
        })
        .collect();
    // Drift that makes the plan pending is also an ITEM in it
    // (tracker #57): the journal cannot contain it — drift's
    // inputs include journal-free state (an MCP credential's
    // presence flips the armed view with no config save) — so
    // without a synthesized item, a drift-only plan offered an
    // Apply over zero counted changes on every surface. The item
    // is derived here, at plan computation, never journaled.
    if drift {
        changes.push(activation::drift_plan_change(
            &drift_detail,
            activation::gateway_config_written_at(&state.data_dir),
        ));
    }
    let gateway_pending = drift
        || pending_entries.iter().any(|e| e.scope != "backend")
        || changes.iter().any(|c| c["activation"] == "gateway_restart");
    let actionable = gateway_pending || changes.iter().any(|c| c["activation"] == "restart");
    Ok(json!({
        "pending": pending,
        "gateway_up": up,
        "gateway_pending": gateway_pending,
        "drift": drift,
        "files_differ": drift_detail.any(),
        "applies_at_next_start": pending && !up,
        "actionable": actionable,
        "backend_restart_ids": backend_restart_ids,
        "changes": changes,
        "snapshot_available": journal.snapshot.is_some() && !pending_entries.is_empty(),
    }))
}


/// Cancel the pending activation plan's changes: restore the
/// configuration snapshot taken before the first pending change.
/// Refused when the configuration has changed through another path
/// since (the restore would discard that change too).
pub async fn op_gateway_activation_cancel(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let mut cfg = state.config.write().await;
    // Live-state resolution with the real start times, running set,
    // and gateway probe: entries whose changes are already live
    // (their backends restarted) or already effective (their
    // targets are stopped, the gateway is down) are not pending
    // and must not be "cancelled" (code-review 4.1; Pete's report,
    // 2026-10-17 — cancel was the only resolution offered for an
    // already-effective change, and it reverted the edit).
    let started_at = backend_start_times(state).await;
    let running = backend_running_ids(state).await;
    let configured: std::collections::HashSet<String> =
        cfg.backends.iter().map(|b| b.id.clone()).collect();
    let probe_host = manager_wizard::steps::gateway_probe_host(&cfg);
    let port = cfg.gateway.port;
    let up = tokio::task::spawn_blocking(move || probe_gateway(&probe_host, port))
        .await
        .unwrap_or(false);
    let restored = match activation::cancel_target(
        &state.data_dir,
        &cfg,
        &started_at,
        &configured,
        &running,
        up,
    ) {
        Ok(Some(restored)) => restored,
        Ok(None) => return Ok(json!({ "cancelled": false, "reason": "no pending changes" })),
        Err(msg) => return Err(ApiError::InvalidInput(msg)),
    };
    manager_config::validate(&restored)
        .map_err(ApiError::from)?;
    manager_config::save(&restored, &state.config_path)
        .map_err(ApiError::from)?;
    *cfg = restored.clone();
    drop(cfg);
    // Regenerate the gateway YAML from the restored config and
    // write it immediately (MCP design §10.2): the on-disk YAML is
    // what the running gateway serves and what the drift check
    // compares against, so leaving the cancelled changes' YAML in
    // place would leave disk, config, and gateway disagreeing
    // until the next start. A regeneration failure does NOT fail
    // the cancel — the config restore already succeeded, and the
    // next gateway start regenerates the YAML from the restored
    // config anyway — so the failure is reported in the result
    // payload instead of as a command error.
    let yaml_path = state.data_dir.join("gateway").join("config.yaml");
    let yaml_error: Option<String> = match manager_gateway::generate_config_yaml(&restored) {
        Ok(yaml) => {
            let write = std::fs::create_dir_all(
                yaml_path.parent().expect("the YAML path has a parent"),
            )
            .and_then(|_| std::fs::write(&yaml_path, yaml));
            write.err().map(|e| format!("could not write {}: {e}", yaml_path.display()))
        }
        Err(e) => Some(format!("could not regenerate the gateway config: {e}")),
    };
    activation::clear_journal(&state.data_dir).map_err(ApiError::InvalidInput)?;
    ensure_backend_specs(state).await;
    // The plan's pending → resolved transition in the notification
    // monitor is a cancellation, not an application: stay silent.
    {
        let mut watch = state.notify_watch.lock().unwrap_or_else(|e| e.into_inner());
        watch.activation_cancelled = true;
    }
    Ok(json!({
        "cancelled": true,
        "yaml_regenerated": yaml_error.is_none(),
        "yaml_error": yaml_error,
    }))
}



/// Mark the gateway side of the activation plan complete after a
/// successful gateway (re)start. The start-gateway step regenerates
/// everything from the live configuration, so gateway-scope
/// journaled changes are live by definition at that point (the
/// plan also self-resolves through the journal's mtime rule; this
/// just clears promptly). Backend-scope entries are NOT cleared
/// here: they resolve through their backends' (re)starts, which
/// the scoped resolution rule observes directly.
pub async fn op_gateway_activation_complete(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let (before, _) = activation_unresolved(state).await;
    let gateway_entries = before.iter().filter(|e| e.scope == "gateway").count();
    activation::clear_gateway_scope(&state.data_dir)
        .map_err(ApiError::InvalidInput)?;
    if gateway_entries > 0 {
        // The gateway side of a plan just went live. When nothing
        // remains pending, the plan is applied: fire the toast here
        // and mark it so the monitor does not repeat it when it
        // observes the same resolution. When backend entries are
        // still outstanding, the monitor fires it as they resolve.
        let (after, drift) = activation_unresolved(state).await;
        if after.is_empty() && !drift {
            {
                let mut watch = state
                    .notify_watch
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                watch.activation_applied_notified = true;
            }
            notify::notify(
                sink.as_ref(),
                state,
                notify::Category::Activation,
                "Activation plan applied",
                "All saved changes are now live.",
            )
            .await;
        }
    }
    Ok(json!({ "cleared": true }))
}


/// Record one finished activation plan execution in the durable
/// history ([`activation_history`]). The apply itself is
/// orchestrated by the desktop UI (`applyActivationPlan`), which
/// reports the execution whole when it ends — including an aborted
/// one, whose record is the one an operator most needs to review.
///
/// Best-effort by contract, like the bookkeeping it follows:
/// recording must never fail or alter the activation it describes,
/// so an unparseable report or a failed write answers
/// `recorded: false` instead of an error. The server assigns the
/// execution id; any id in the report is ignored.
pub async fn op_gateway_activation_record_execution(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    record: Value,
) -> ApiResult {
    let mut execution: activation_history::ActivationExecution =
        match serde_json::from_value(record) {
            Ok(e) => e,
            Err(_) => return Ok(json!({ "recorded": false })),
        };
    let id = activation_history::record_execution(&state.data_dir, &mut execution);
    Ok(json!({ "recorded": id.is_some(), "id": id }))
}

/// The activation plan execution history: with no `execution_id`,
/// the recorded executions newest-first as summaries (id, times,
/// status, source, plan state, per-outcome counts); with one, that
/// execution's full record — the plan as computed at apply time
/// with every item's outcome — or `null` when it is not in the
/// retained history. A missing or corrupt history file reads as an
/// empty history, never an error.
pub async fn op_gateway_activation_history(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    execution_id: Option<String>,
) -> ApiResult {
    let history = activation_history::load_history(&state.data_dir);
    if let Some(id) = execution_id {
        let found = history.executions.iter().rev().find(|e| e.id == id);
        return Ok(json!({ "execution": found }));
    }
    let summaries: Vec<Value> = history
        .executions
        .iter()
        .rev()
        .map(|e| {
            let (applied, skipped, failed) = e.outcome_counts();
            json!({
                "id": e.id,
                "started_at": e.started_at,
                "finished_at": e.finished_at,
                "status": e.status,
                "source": e.source,
                "mode": e.mode,
                "drift": e.drift,
                "gateway_up": e.gateway_up,
                "gateway_pending": e.gateway_pending,
                "items": e.items.len(),
                "applied": applied,
                "skipped": skipped,
                "failed": failed,
            })
        })
        .collect();
    Ok(json!({ "executions": summaries }))
}

/// Quiesce wait: poll the gateway's in-flight request gauge until
/// it reaches zero or the timeout expires. A gateway that does not
/// answer metrics (down, or the metrics config not yet active)
/// counts as drained — nothing can be in flight through a gateway
/// that is not serving, and the gauge series only exists once
/// traffic has flowed.
pub async fn op_gateway_quiesce_wait(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    timeout_secs: u64,
) -> ApiResult {
    let timeout = timeout_secs.clamp(5, 600);
    let (host, port) = {
        let cfg = state.config.read().await;
        (
            manager_wizard::steps::gateway_probe_host(&cfg),
            cfg.gateway.port,
        )
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
    let mut last_in_flight: Option<f64>;
    loop {
        let key = RealSystemOps::default()
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
            None => return Ok(json!({ "drained": true, "in_flight": 0, "timed_out": false })),
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
pub async fn op_gateway_stop(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let data_dir = state.data_dir.clone();
    // Service mode: the SCM owns the gateway process; the pid-file
    // stop would kill the service host's child out from under it.
    let stopped = if state
        .service_route_active(manager_services::GATEWAY_SERVICE_NAME)
        .await
    {
        tokio::task::spawn_blocking(|| {
            use manager_services::ServiceControl;
            let control = manager_services::scm::platform_control();
            control
                .stop(manager_services::GATEWAY_SERVICE_NAME)
                .map_err(|e| e.to_string())?;
            for _ in 0..150 {
                match control
                    .query(manager_services::GATEWAY_SERVICE_NAME)
                    .map_err(|e| e.to_string())?
                    .state
                {
                    manager_services::ServiceState::Stopped
                    | manager_services::ServiceState::NotInstalled => return Ok(true),
                    _ => std::thread::sleep(std::time::Duration::from_millis(100)),
                }
            }
            Err("gateway service did not stop within 15 s".to_string())
        })
        .await
        .map_err(|e| ApiError::Gateway(format!("gateway stop task failed: {e}")))?
        .map_err(ApiError::Gateway)?
    } else {
        tokio::task::spawn_blocking({
            let data_dir = data_dir.clone();
            move || manager_wizard::stop_gateway(&data_dir)
        })
        .await
        .map_err(|e| ApiError::Gateway(format!("gateway stop task failed: {e}")))?
        .map_err(ApiError::Gateway)?
    };
    // The app-managed PostgreSQL server exists to serve the
    // gateway's database; stop it after the gateway so nothing is
    // left holding pooled connections to a stopping server.
    // Best-effort: the gateway stop already succeeded, so a
    // PostgreSQL stop problem is reported in the payload, not as a
    // command failure.
    let postgres_stopped =
        manager_wizard::steps::stop_postgres_server(&RealSystemOps::default(), &data_dir)
            .await
            .unwrap_or(false);
    // The operator stopped the gateway deliberately: the
    // notification monitor must not report it as down. Clear the
    // expectation until a probe sees it up again.
    {
        let mut watch = state.notify_watch.lock().unwrap_or_else(|e| e.into_inner());
        watch.gateway_expected = false;
        watch.gateway_up = Some(false);
        watch.gateway_down_strikes = 0;
    }
    Ok(json!({ "ok": true, "stopped": stopped, "postgres_stopped": postgres_stopped }))
}


/// One-shot gateway liveness probe: connect to <host>:<port> with a
/// 500 ms timeout, `GET /health/liveliness` with a 1 s read timeout,
/// and report whether the reply's first line is a syntactically valid
/// HTTP status line. Any status code counts — 200, 401, and 404 all
/// prove a live HTTP listener, which is all "up" means here. Any
/// error (refused, timeout, malformed reply) is simply "not up";
/// runs on a blocking thread and never fails the caller. The host is
/// the app's probe host for the configured bind address (loopback
/// whenever the bind covers it).
///
/// The path is LiteLLM's unauthenticated liveliness endpoint, not
/// `/v1/models`: this probe fires on every gateway_status poll
/// (~15 s), and the keyless `/v1/models` requests filled the gateway
/// log with "No api key passed in" auth rejections — noise that was
/// later mistaken for a startup failure's cause (2026-10-16).
pub(crate) fn probe_gateway(host: &str, port: u16) -> bool {
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
        .write_all(
            b"GET /health/liveliness HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        )
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
pub async fn op_gateway_set_bind_host(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    host: String,
) -> ApiResult {
    if host.parse::<std::net::IpAddr>().is_err() {
        return Err(ApiError::InvalidInput(format!(
            "bind host {host:?} is not an IP address"
        )));
    }
    let mut cfg = state.config.write().await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    new_cfg.gateway.bind_host = host.clone();
    manager_config::validate(&new_cfg).map_err(ApiError::from)?;
    record_gateway_change(
        state,
        &pre_cfg,
        &new_cfg,
        "gateway_bind",
        format!("Gateway network access changed to {host}"),
        true,
    )
    .await?;
    manager_config::save(&new_cfg, &state.config_path).map_err(ApiError::from)?;
    *cfg = new_cfg;
    Ok(json!({ "bind_host": host, "restart_required": true }))
}

/// Fetch the gateway's Prometheus exposition (`GET /metrics`, the
/// master key as Bearer), following redirects. Redirect following
/// is load-bearing, not a nicety: LiteLLM's prometheus route is
/// registered with a trailing slash on current versions, so
/// `GET /metrics` answers 307 to `/metrics/` — a fetcher that does
/// not follow the redirect never receives a body. That defect left
/// the Dashboard's gateway statistics permanently empty in the
/// field (every poll 307'd and was silently skipped) while the
/// banner blamed a gateway restart that could not help.
///
/// The body arrives with chunked transfer encoding: LiteLLM's
/// metrics app streams the exposition with no content length, so
/// chunked is the normal case, and [`metrics_http_get`] de-chunks
/// it strictly (a truncated or malformed stream fails the cycle —
/// partial counters would corrupt the window deltas). An earlier
/// version of this fetcher skipped chunked bodies outright on the
/// theory that they would corrupt the parse; since the gateway
/// answers ONLY chunked, that discarded every scrape and left the
/// statistics permanently empty again (#58). Returns the body only
/// on a final HTTP 200; anything else — gateway down, the metrics
/// callback not active (config predates v0.1.18 until the next
/// gateway start), an auth failure, a redirect loop — is None and
/// the poll cycle is skipped.
fn fetch_gateway_metrics(host: &str, port: u16, master_key: &str) -> Option<String> {
    const MAX_REDIRECTS: usize = 3;
    let mut path = "/metrics".to_string();
    for _ in 0..=MAX_REDIRECTS {
        let resp = metrics_http_get(host, port, &path, master_key)?;
        match resp.status {
            200 => return Some(resp.body),
            301 | 302 | 303 | 307 | 308 => {
                path = metrics_redirect_path(&path, resp.location.as_deref()?)?;
            }
            _ => return None,
        }
    }
    None
}


/// One gateway-metrics poll cycle: resolve the probe host and port,
/// read the stored master key (never generate one here — polling
/// must not create credentials), fetch and record a snapshot. Any
/// missing piece skips the cycle silently.
pub async fn poll_gateway_metrics_once(state: &AppState) {
    let (host, port) = {
        let cfg = state.config.read().await;
        (
            manager_wizard::steps::gateway_probe_host(&cfg),
            cfg.gateway.port,
        )
    };
    let Some(key) = RealSystemOps::default()
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

/// Preview the generated LiteLLM config.yaml — plus, when a typed
/// routing rule is enabled, the generated `llmdil_routing.py`
/// callback (the Generated config card's second tab; design §7).
/// Null when no rule is enabled and no callback file is generated.
pub async fn op_gateway_preview_yaml(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let cfg = state.config.read().await;
    let yaml = manager_gateway::generate_config_yaml(&cfg)
        .map_err(ApiError::from)?;
    let routing_py = manager_gateway::generate_routing_callback(&cfg);
    Ok(json!({ "yaml": yaml, "routing_py": routing_py }))
}


/// Trace routing for a tagged request: `request` carries the model group
/// name (`{"group": "planner"}`). Runs `pick_backend` as an explainer
/// over the current config (every group member assumed healthy, no
/// load/latency data — the reason string says so) and records the
/// decision in the request ring. This is a dry run for the UI: live
/// traffic is routed by LiteLLM under its single global strategy, not
/// by `pick_backend`.
pub async fn op_gateway_test_routing(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    request: Value,
) -> ApiResult {
    let group_name = request
        .get("group")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::Gateway("request must carry a \"group\" string".to_string()))?;
    let cfg = state.config.read().await;
    let group = cfg
        .gateway
        .groups
        .iter()
        .find(|g| g.name == group_name)
        .ok_or_else(|| ApiError::NotFound(format!("model group {group_name}")))?;
    let stats: Vec<manager_gateway::BackendStats> = group
        .members
        .iter()
        .map(|m| manager_gateway::BackendStats::healthy(m.clone()))
        .collect();
    let decision = manager_gateway::trace_routing(group, &stats, false, now_ms());
    state.routing_log.lock().await.record(decision.clone());
    Ok(serde_json::to_value(&decision)?)
}

/// Recent gateway routing decisions (traced via `gateway_test_routing`),
/// newest first.
pub async fn op_gateway_recent_requests(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let recent = state.routing_log.lock().await.recent();
    Ok(json!({ "requests": recent }))
}

// ---------------------------------------------------------------------------
// Models: additional §14 commands
// ---------------------------------------------------------------------------

/// Pause an in-flight download (partial file kept for resume).
pub async fn op_model_pause_download(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    let downloads = state.downloads.lock().await;
    let handle = downloads
        .get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("no download in flight for {id}")))?;
    handle.pause().await?;
    Ok(json!({ "model_id": id, "paused": true }))
}

/// Resume a paused download.
pub async fn op_model_resume_download(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    let downloads = state.downloads.lock().await;
    let handle = downloads
        .get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("no download in flight for {id}")))?;
    handle.resume().await.map_err(ApiError::from)?;
    Ok(json!({ "model_id": id, "resumed": true }))
}

/// SHA-256-verify an installed model file against the hash pinned in
/// the model's config entry. Records the outcome in `verified_at`
/// (stamped on success, cleared on mismatch) and persists the config.
/// The download-outcome gate follows the same result: success REMOVES
/// any recorded verification failure for the model (the start
/// preflight stops refusing), a mismatch records one.
pub async fn op_model_verify_checksum(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    let (path, sha) = {
        let cfg = state.config.read().await;
        let m = cfg
            .models
            .iter()
            .find(|m| m.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("model {id}")))?;
        if !is_real_pin(&m.sha256) {
            return Err(ApiError::InvalidInput(format!(
                "no SHA-256 hash is pinned for model {id}; pin one in the app configuration to enable verification"
            )));
        }
        (
            resolve_under_data_dir(&cfg.data_dir, &m.gguf_path),
            m.sha256.clone(),
        )
    };
    // Track the manual verify in the same set the download watcher
    // uses: list_models then reports the model as verifying (the
    // hash of a multi-GiB file takes minutes and the card must not
    // look idle or re-offer the button), and the start preflight
    // refuses to launch on a file whose verification is in flight.
    // The mark is removed on every exit path below.
    {
        let mut verifying = state
            .verifying_downloads
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !verifying.insert(id.clone()) {
            return Err(ApiError::InvalidInput(format!(
                "verification already in progress for model {id}"
            )));
        }
    }
    let verify_result = manager_models::verify_sha256(&path, &sha).await;
    state
        .verifying_downloads
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);
    let ok = verify_result?;
    {
        let mut cfg = state.config.write().await;
        if let Some(m) = cfg.models.iter_mut().find(|m| m.id == id) {
            // The stamp and the hash it verified against move
            // together (code-review 2.4).
            if ok {
                m.verified_at = Some(chrono::Utc::now());
                m.verified_sha256 = Some(sha.clone());
            } else {
                m.verified_at = None;
                m.verified_sha256 = None;
            }
        }
        manager_config::save(&cfg, &state.config_path)?;
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

/// Rename a model's short display name (the operator's own label,
/// stored as `display_name` on the model config). The effective
/// short name — this override, else the catalog name, else the id —
/// is what `list_models` reports and every short-name control
/// shows. The model id itself never changes. Mutation happens on a
/// clone installed in memory only after a successful save, so a
/// failed save leaves memory and disk in agreement.
pub async fn op_model_set_short_name(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    name: String,
) -> ApiResult {
    let mut cfg = state.config.write().await;
    let mut new_cfg = cfg.clone();
    let m = new_cfg
        .models
        .iter_mut()
        .find(|m| m.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("model {id}")))?;
    // An empty name CLEARS the override (the effective name falls
    // back to the catalog name / id): there was previously no way
    // to undo a rename — empty input failed validation like any
    // other bad name (code-review 4.14).
    if name.trim().is_empty() {
        m.display_name = None;
        manager_config::save(&new_cfg, &state.config_path)?;
        *cfg = new_cfg;
        return Ok(json!({ "id": id, "name": Value::Null }));
    }
    let validated = manager_config::validate_display_name(&name).map_err(ApiError::InvalidInput)?;
    m.display_name = Some(validated.clone());
    manager_config::save(&new_cfg, &state.config_path)?;
    *cfg = new_cfg;
    Ok(json!({ "id": id, "name": validated }))
}

/// Delete a model file (guarded): refuses while any backend references
/// the model, or while a download is in flight for it. Removes the
/// model from the config on success and persists the removal; any
/// partial download file beside it is deleted too.
pub async fn op_model_delete(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    reap_downloads(state).await;
    if state.downloads.lock().await.contains_key(&id) {
        return Err(ApiError::Models(format!(
            "download in flight for model {id}; cancel it before deleting"
        )));
    }
    let mut cfg = state.config.write().await;
    let pos = cfg
        .models
        .iter()
        .position(|m| m.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("model {id}")))?;
    let model = cfg.models[pos].clone();
    // Delete the file first: on failure the config entry is kept. The
    // file path is the model's gguf_path resolved under the data dir —
    // the same path download_model wrote and list_models probes.
    let file_path = resolve_under_data_dir(&cfg.data_dir, &model.gguf_path);
    let report = manager_models::delete_model_file(&model, &cfg.backends, &file_path)
        .map_err(ApiError::from)?;
    cfg.models.remove(pos);
    // Persist the removal — without the save, the deleted model
    // reappears in the config (and the Models view) on the next launch.
    manager_config::save(&cfg, &state.config_path)?;
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
pub async fn op_model_set_enabled(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    enabled: bool,
) -> ApiResult {
    let mut cfg = state.config.write().await;
    let model = cfg
        .models
        .iter_mut()
        .find(|m| m.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("model {id}")))?;
    model.enabled = enabled;
    manager_config::save(&cfg, &state.config_path)?;
    drop(cfg);
    Ok(json!({ "model_id": id, "enabled": enabled }))
}

/// Set or clear a model's operator MoE declaration: the MoE layer
/// count and the default offload layers that give a model the
/// curated catalog does not carry expert-offload controls (the
/// creation default and the Edit Flags slider's green limit are
/// resolved from it when the catalog pins neither). Both fields
/// `None` clears the declaration. The pair is validated atomically
/// (see [`manager_config::validate_moe_declaration`]) before
/// anything is written, and the save validates again, so a
/// rejected edit leaves the stored model untouched. Catalog
/// models keep resolving from the catalog — a declaration stored
/// on one is shadowed by the catalog pin, never an override.
pub async fn op_model_set_offload_spec(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    moe_layers: Option<u32>,
    default_offload_layers: Option<u32>,
) -> ApiResult {
    manager_config::validate_moe_declaration(moe_layers, default_offload_layers)
        .map_err(ApiError::InvalidInput)?;
    let mut cfg = state.config.write().await;
    let mut new_cfg = cfg.clone();
    let m = new_cfg
        .models
        .iter_mut()
        .find(|m| m.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("model {id}")))?;
    m.moe_layers = moe_layers;
    m.default_offload_layers = default_offload_layers;
    manager_config::save(&new_cfg, &state.config_path)?;
    *cfg = new_cfg;
    Ok(json!({
        "id": id,
        "moe_layers": moe_layers,
        "default_offload_layers": default_offload_layers,
    }))
}

/// Add a model to the registry: an id, the https URL of its GGUF file,
/// and optionally an explicit file name, quant label, parameter
/// count (see [`model_add_entry`] for how the count is resolved and
/// when the add is refused), and a MoE declaration (the MoE layer
/// count plus the default offload layers — the pair that gives a
/// model the curated catalog does not carry expert-offload
/// controls; see [`manager_config::validate_moe_declaration`]).
/// The model starts enabled with no pinned SHA-256 — its downloads run unverified (the Models view shows
/// "hash not pinned") until a hash is recorded in the configuration.
#[allow(clippy::too_many_arguments)]
pub async fn op_model_add(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    source_url: String,
    file_name: Option<String>,
    quant: Option<String>,
    params_b: Option<f32>,
    moe_layers: Option<u32>,
    default_offload_layers: Option<u32>,
) -> ApiResult {
    // Resolve everything before taking the config lock: a rejected
    // add must leave the in-memory config and the saved file
    // untouched. (The previous code pushed the entry into the live
    // config first, so when the save then failed validation the
    // invalid entry stayed in memory and poisoned every later save
    // in the session.)
    let entry = model_add_entry(
        &id,
        &source_url,
        file_name,
        quant,
        params_b,
        moe_layers,
        default_offload_layers,
    )?;
    let id = entry.id.clone();
    let mut cfg = state.config.write().await;
    if cfg.models.iter().any(|m| m.id == id) {
        return Err(ApiError::InvalidInput(format!(
            "a model with id '{id}' already exists"
        )));
    }
    // Save a clone and swap it in only on success — the pattern the
    // other config-mutating commands use. save() validates before
    // writing, so a failure here also leaves state untouched.
    let mut new_cfg = cfg.clone();
    new_cfg.models.push(entry);
    manager_config::save(&new_cfg, &state.config_path)?;
    *cfg = new_cfg;
    drop(cfg);
    Ok(json!({ "model_id": id }))
}


/// Assign a model to a backend with a tensor split (spec §14
/// `model_assign`). Sets the backend's model file, applies the split, and
/// records a restart task for the guided rolling restart.
pub async fn op_model_assign(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
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
        return Err(ApiError::InvalidInput(format!(
            "tensor split has {} entries but there are {gpu_count} GPUs",
            split.len()
        )));
    }
    // Mutate a clone and commit it to state.config only after
    // placement, validation, journaling, and the save all succeed
    // (the #32 pattern; code review B-4): the mutations below run
    // ahead of checks that can fail — apply_placement, validate,
    // the save itself — and a failure that left them in the live
    // config would persist on the next unrelated save with no
    // journal entry.
    let mut guard = state.config.write().await;
    let pre_cfg = guard.clone();
    let mut cfg = pre_cfg.clone();
    let model = cfg
        .models
        .iter()
        .find(|m| m.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("model {id}")))?;
    let gguf_path = model.gguf_path.clone();
    // The model's default offload flags (catalog pin, or the
    // operator's declared default on the model record), resolved
    // before the mutable backend borrow below.
    let default_flags = model_default_raw_flags(model);
    let flags_untouched = {
        let backend = cfg
            .backends
            .iter_mut()
            .find(|b| b.id == backend_id)
            .ok_or_else(|| ApiError::NotFound(format!("backend {backend_id}")))?;
        let prev_model_file = backend.model_file.clone();
        let prev_raw_flags = backend.raw_flags.clone();
        backend.model_file = gguf_path;
        // Advanced flags mode (tracker #60): the assignment itself
        // proceeds — the model a backend serves is not flags state —
        // but its two flags side effects are skipped for an
        // advanced backend: the catalog default fill below (it
        // would write the operator's raw string) and the placement
        // application (placement_apply is refused outright in the
        // mode). The response reports the skip.
        let untouched = backend.flags_advanced;
        // A backend with no raw flags of its own takes the newly
        // assigned model's default offload (e.g. Nemotron's
        // --n-cpu-moe 16 from the catalog, or a user-added MoE
        // model's declared default) — the same preservation rule
        // as the catalog definition refresh: backends carrying
        // operator flags keep them, and the offload slider
        // adjusts from there.
        if !untouched
            && backend
                .raw_flags
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        {
            if let Some(flags) = &default_flags {
                backend.raw_flags = Some(flags.clone());
            }
        }
        if !untouched {
            manager_models::apply_placement(backend, &split)
                .map_err(ApiError::from)?;
        }
        // A model-file or raw-flags change invalidates the recorded
        // tool-call probe (MCP design §7.2).
        if backend.model_file != prev_model_file || backend.raw_flags != prev_raw_flags {
            backend.tool_call_probe = None;
        }
        untouched
    };
    if let Some(model) = cfg.models.iter_mut().find(|m| m.id == id) {
        if !model.assigned_backends.contains(&backend_id) {
            model.assigned_backends.push(backend_id.clone());
        }
    }
    manager_config::validate(&cfg)
        .map_err(ApiError::from)?;
    // The assignment activates twice over: the gateway's routes
    // change with the backend's model (gateway scope, recorded only
    // when the generated YAML actually differs), and the backend
    // loads the new model at its next (re)start (backend scope).
    // Recorded before the save, per the journal ordering rule.
    record_gateway_change(
        state,
        &pre_cfg,
        &cfg,
        "model_assign",
        format!("Backend `{backend_id}` now serves model `{id}`"),
        false,
    )
    .await
    ?;
    record_backend_change(
        state,
        &pre_cfg,
        &cfg,
        "model_assign",
        format!("Backend `{backend_id}` now serves model `{id}`"),
        &backend_id,
    )
    .await?;
    manager_config::save(&cfg, &state.config_path)?;
    // Commit point: every fallible step has succeeded, so the
    // working clone becomes the live config.
    *guard = cfg;
    drop(guard);
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
    Ok(json!({
        "task_id": task_id,
        "persisted": true,
        // True when the target backend is in advanced flags mode
        // (tracker #60): its split and raw flags were left
        // untouched by the assignment.
        "flags_untouched": flags_untouched,
    }))
}


// ---------------------------------------------------------------------------
// Telemetry: additional §14 commands
// ---------------------------------------------------------------------------

/// Set the telemetry poll interval. Persisted immediately via
/// `manager_config::save`; the running poller's cadence is fixed when
/// the poller starts (PollHandle exposes no re-cadence), so the new
/// interval takes effect on the next app start.
pub async fn op_telemetry_set_interval(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    ms: u64,
) -> ApiResult {
    // Persisted immediately. The running poller's cadence is fixed when
    // the poller starts (PollHandle exposes no re-cadence), so the new
    // interval takes effect on the next app start; the GPUs view labels
    // the control accordingly.
    let mut cfg = state.config.write().await;
    cfg.telemetry.poll_interval_ms = ms;
    manager_config::save(&cfg, &state.config_path)?;
    Ok(json!({ "ok": true, "poll_interval_ms": ms }))
}

/// Reset peak trackers (spec §5: peak marks are resettable).
pub async fn op_telemetry_reset_peaks(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    lock_telemetry(&state.telemetry().await).reset_peaks();
    Ok(json!({ "ok": true }))
}

/// Write the current snapshot to the data dir.
pub async fn op_telemetry_save_snapshot(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let samples: Vec<GpuSample> = lock_telemetry(&state.telemetry().await).snapshot();
    let path = state
        .data_dir
        .join(format!("telemetry-snapshot-{}.json", now_ms()));
    std::fs::write(&path, serde_json::to_string_pretty(&samples)?).map_err(ApiError::from)?;
    Ok(json!({ "path": path }))
}

/// The configured model a backend serves, resolved by comparing
/// data-dir-resolved file paths (the same rule placement apply and
/// the UI's model derivation use).
fn model_for_backend<'a>(
    cfg: &'a AppConfig,
    b: &manager_config::BackendConfig,
) -> Option<&'a manager_config::ModelConfig> {
    let target = resolve_under_data_dir(&cfg.data_dir, &b.model_file);
    cfg.models
        .iter()
        .find(|m| resolve_under_data_dir(&cfg.data_dir, &m.gguf_path) == target)
}

/// Cache of parsed GGUF layouts, keyed by (path, file length,
/// mtime): a model's header is parsed once per file version, not
/// once per poll. `None` values cache unparseable files too.
#[allow(clippy::type_complexity)]
static GGUF_LAYOUT_CACHE: std::sync::OnceLock<
    std::sync::Mutex<
        std::collections::HashMap<(PathBuf, u64, i64), Option<manager_models::GgufLayout>>,
    >,
> = std::sync::OnceLock::new();

/// Parse a GGUF file's header layout, reading longer prefixes until
/// the parser stops reporting `Incomplete` (headers run a few MiB;
/// the tokenizer metadata dominates). Returns None for missing or
/// unparseable files.
fn gguf_layout_cached(path: &PathBuf) -> Option<manager_models::GgufLayout> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let key = (path.clone(), meta.len(), mtime);
    let cache =
        GGUF_LAYOUT_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    if let Ok(guard) = cache.lock() {
        if let Some(hit) = guard.get(&key) {
            return hit.clone();
        }
    }
    let mut layout = None;
    for prefix in [8 * 1024 * 1024usize, 64 * 1024 * 1024, 256 * 1024 * 1024] {
        let want = (meta.len() as usize).min(prefix);
        let mut buf = vec![0u8; want];
        let read_ok = std::fs::File::open(path)
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
            .is_ok();
        if !read_ok {
            break;
        }
        match manager_models::parse_gguf_layout(&buf) {
            Ok(l) => {
                layout = Some(l);
                break;
            }
            Err(manager_models::GgufError::Incomplete) if want < meta.len() as usize => continue,
            Err(_) => break,
        }
    }
    if let Ok(mut guard) = cache.lock() {
        guard.insert(key, layout.clone());
    }
    layout
}

/// A backend's model weight layout, resolved once for the memory
/// requirement and the offload slider: total weight bytes, the
/// routed-expert share, the MoE layer count, KV bytes per token,
/// and the derivation basis. The layout comes from the downloaded
/// GGUF when available (basis Exact), else the catalog's pins
/// (Catalog — expert bytes and MoE layer count are both pinned, so
/// a partial offload resolves before download), else a
/// parameter-count estimate (Estimate). None when the backend's
/// model cannot be resolved.
struct WeightLayout {
    total_bytes: u64,
    expert_bytes: Option<u64>,
    /// Per-MoE-block expert bytes from the GGUF parse, when the
    /// layout came from one — partial offload is priced from the
    /// true first-N block sums, not a uniform share.
    expert_bytes_by_block: Option<Vec<u64>>,
    moe_layers: Option<u32>,
    kv_per_token: Option<u64>,
    basis: manager_models::MemoryBasis,
    /// The curated catalog entry for the backend's model, when it
    /// is a catalog model (the offload slider's green limit and
    /// default live there).
    catalog: Option<manager_models::ModelSpec>,
    /// The offload slider's green limit (in MoE layers) for the
    /// backend's model: the catalog's pinned default when the
    /// catalog carries one, else the operator's declared default
    /// on the model record ([`ModelConfig::default_offload_layers`])
    /// — the fallback that gives a user-added MoE model offload
    /// controls. `None` when neither source pins one.
    green_limit_layers: Option<u32>,
}

fn resolve_weight_layout(
    cfg: &AppConfig,
    b: &manager_config::BackendConfig,
) -> Option<WeightLayout> {
    let model = model_for_backend(cfg, b)?;
    let path = resolve_under_data_dir(&cfg.data_dir, &model.gguf_path);
    let layout = gguf_layout_cached(&path);
    let catalog = manager_models::curated_models()
        .into_iter()
        .find(|s| s.id == model.id);
    let file_len = std::fs::metadata(&path)
        .ok()
        .filter(|m| m.is_file())
        .map(|m| m.len());
    // One source per figure set, and the basis labels THAT source:
    // when the GGUF parses, the total is the parse's own tensor
    // sum — the previous version paired the catalog's (or the file
    // length's) total with the parse's expert bytes and labeled
    // the mixture Exact, a label neither source earned
    // (code-review 4.12). Without a parse, the catalog's pins are
    // Catalog; anything else is an Estimate.
    let (total_bytes, expert_bytes, expert_bytes_by_block, kv_per_token, moe_layers, basis) =
        match (&layout, &catalog) {
            (Some(l), _) => (
                l.tensor_bytes,
                Some(l.expert_bytes),
                Some(l.expert_bytes_by_block.clone()),
                l.kv.map(|k| k.bytes_per_token()),
                l.moe_block_count,
                manager_models::MemoryBasis::Exact,
            ),
            (None, Some(c)) => (
                c.size_bytes,
                c.expert_bytes,
                None,
                None,
                c.moe_layers,
                manager_models::MemoryBasis::Catalog,
            ),
            (None, None) => (
                file_len.unwrap_or_else(|| {
                    manager_wizard::steps::estimate_model_bytes(model.params_b as f64)
                }),
                None,
                None,
                None,
                None,
                manager_models::MemoryBasis::Estimate,
            ),
        };
    // The slider's green limit resolves per figure, catalog
    // first: a catalog pin wins; where the catalog carries no
    // default (or no entry at all), the operator's declared
    // default on the model record supplies it.
    let green_limit_layers = catalog
        .as_ref()
        .and_then(|c| c.default_offload_layers)
        .or(model.default_offload_layers);
    Some(WeightLayout {
        total_bytes,
        expert_bytes,
        expert_bytes_by_block,
        moe_layers,
        kv_per_token,
        basis,
        catalog,
        green_limit_layers,
    })
}

/// A backend's memory requirement (GPU-resident weights, system-RAM
/// weights, KV at its configured context) under its current serving
/// configuration — see [`manager_models::memory_requirement`]. None
/// when the backend's model cannot be resolved.
fn backend_memory_requirement(
    cfg: &AppConfig,
    b: &manager_config::BackendConfig,
) -> Option<manager_models::MemoryRequirement> {
    let layout = resolve_weight_layout(cfg, b)?;
    let offload = manager_models::expert_offload(b.raw_flags.as_deref());
    Some(manager_models::memory_requirement(
        layout.total_bytes,
        layout.expert_bytes,
        layout.expert_bytes_by_block.as_deref(),
        layout.basis,
        offload,
        layout.kv_per_token,
        layout.moe_layers,
        b.flags.n_ctx,
    ))
}

struct OffloadInfo {
    /// MoE layers in total (the slider's maximum).
    moe_layers: u32,
    /// Current slider position: MoE layers on system RAM.
    layers: u32,
    expert_bytes: u64,
    total_bytes: u64,
    /// The green limit in layers (the model's default offload —
    /// the catalog pin, or the operator's declared default for a
    /// model outside the catalog).
    green_limit_layers: u32,
    /// Slider band boundaries as fractions (see
    /// [`manager_models::offload_bands`]).
    green_end: f64,
    yellow_end: f64,
    /// System-RAM weight bytes at every slider position
    /// `0..=moe_layers` (see
    /// [`manager_models::sys_bytes_by_position`]).
    sys_bytes_by_position: Vec<u64>,
    /// True when the saved offload spelling is not the canonical
    /// form for its position (a hand-written `--cpu-moe`, an
    /// `--n-cpu-moe=46` `=` spelling, ...). Whitespace-only
    /// differences are not custom: the comparison is over
    /// whitespace-split tokens re-joined.
    custom: bool,
    /// The catalog's per-architecture offload restrictions for the
    /// backend's model (empty outside the curated catalog).
    restrictions: Vec<manager_models::OffloadRestriction>,
}


fn backend_offload_info(cfg: &AppConfig, b: &manager_config::BackendConfig) -> Option<OffloadInfo> {
    let layout = resolve_weight_layout(cfg, b)?;
    let expert_bytes = layout.expert_bytes?;
    let moe_layers = layout.moe_layers?;
    // The bands derive from the model's green limit (the catalog
    // default, or the operator's declared default for a model the
    // catalog does not carry), so a model with no green limit
    // from either source gets no slider rather than invented
    // band boundaries.
    let green_limit_layers = layout.green_limit_layers?;
    if expert_bytes == 0 || moe_layers == 0 {
        return None;
    }
    let (green_end, yellow_end) = manager_models::offload_bands(moe_layers, green_limit_layers);
    let offload = manager_models::expert_offload(b.raw_flags.as_deref());
    // Custom spelling: rewriting the detected position back through
    // the canonical writer must reproduce the saved tokens exactly
    // (after token normalization, so spacing alone never counts).
    let canonical = manager_models::set_expert_offload(b.raw_flags.as_deref(), offload);
    let custom = token_normalized_flags(b.raw_flags.as_deref())
        != token_normalized_flags(canonical.as_deref());
    let restrictions = layout
        .catalog
        .as_ref()
        .map(|c| c.offload_restrictions.clone())
        .unwrap_or_default();
    let sys_bytes_by_position = manager_models::sys_bytes_by_position(
        expert_bytes,
        layout.expert_bytes_by_block.as_deref(),
        moe_layers,
    );
    Some(OffloadInfo {
        moe_layers,
        layers: offload.layers(moe_layers),
        expert_bytes,
        total_bytes: layout.total_bytes,
        green_limit_layers,
        green_end,
        yellow_end,
        sys_bytes_by_position,
        custom,
        restrictions,
    })
}

/// The `offload` object a backend row carries — the slider state
/// plus its guardrail object — shared by list_backends and the add
/// form's preview (backend_add_preview), so the add form renders
/// the offload control from exactly the payload the Edit Flags form
/// renders from. The guardrail is emitted when a placed GPU is
/// governed by a restriction (status by position), or when the
/// placement is unknown (empty placed set, not CPU-only) while the
/// model carries restrictions and the saved position is partial
/// (status untested, against the first restriction's architecture);
/// null otherwise.
fn offload_row_json(o: &OffloadInfo, eval: Option<&OffloadEvaluation>) -> Value {
    let guardrail = eval.and_then(|eval| {
        let status = serde_json::to_value(eval.status).unwrap_or(Value::Null);
        if let Some(cc) = eval.restricting_cc {
            let restriction = eval
                .restrictions
                .iter()
                .find(|r| r.compute_capability == cc)?;
            Some(json!({
                "compute_capability": format!("{}.{}", cc.0, cc.1),
                "allowed": serde_json::to_value(restriction.allowed)
                    .unwrap_or(Value::Null),
                "status": status,
                "nearest_verified_layers": eval.nearest_verified_layers,
                "has_override": eval.has_override,
                "overridden": eval.overridden,
            }))
        } else if eval.placed_ccs.is_empty()
            && !eval.cpu_only
            && !eval.restrictions.is_empty()
            && eval.layers != 0
            && eval.layers != eval.moe_layers
        {
            let first = &eval.restrictions[0];
            Some(json!({
                "compute_capability": format!(
                    "{}.{}",
                    first.compute_capability.0, first.compute_capability.1
                ),
                "allowed": serde_json::to_value(first.allowed)
                    .unwrap_or(Value::Null),
                "status": status,
                "nearest_verified_layers": eval.nearest_verified_layers,
                "has_override": eval.has_override,
                "overridden": eval.overridden,
            }))
        } else {
            None
        }
    });
    json!({
        "moe_layers": o.moe_layers,
        "layers": o.layers,
        "expert_bytes": o.expert_bytes,
        "total_bytes": o.total_bytes,
        "green_limit_layers": o.green_limit_layers,
        "green_end": o.green_end,
        "yellow_end": o.yellow_end,
        "sys_bytes_by_position": o.sys_bytes_by_position,
        "custom": o.custom,
        "guardrail": guardrail,
    })
}

struct MtpInfo {
    /// Whether `--spec-type draft-mtp` is in the saved raw flags.
    enabled: bool,
    /// The explicit `--spec-draft-n-max`, or `None` when MTP is on
    /// without one (llama.cpp's default then applies) or off.
    n_max: Option<u32>,
    /// The explicit `--parallel` slot count, when the raw flags
    /// set one. MTP drafting needs a single slot; the control
    /// compares this against 1 to surface the conflict — the MTP
    /// control itself never rewrites it (the flags form's
    /// Parallel slots field is the write path, tracker #61).
    parallel: Option<i64>,
    /// True when the saved MTP spelling is not the canonical form
    /// for its state (a bare `--spec-type draft-mtp` with no
    /// length token, an `=` spelling, ...). The same comparison
    /// as the offload slider's `custom`: the decoded state
    /// rewritten canonically must reproduce the saved tokens
    /// exactly (after token normalization, so spacing alone never
    /// counts).
    custom: bool,
}

/// A backend's MTP control state, when its model is MTP-capable.
/// Capability is a catalog pin resolved live by model id or
/// source URL (see [`manager_models::catalog_mtp`]) — never
/// inferred from a model's name or file — so a backend whose
/// capability is unknown gets no control rather than a
/// speculative one.
fn backend_mtp_info(cfg: &AppConfig, b: &manager_config::BackendConfig) -> Option<MtpInfo> {
    let model = model_for_backend(cfg, b)?;
    if !manager_models::catalog_mtp(&model.id, &model.source_url) {
        return None;
    }
    let state = manager_models::mtp_draft(b.raw_flags.as_deref());
    let canonical = manager_models::set_mtp_draft(b.raw_flags.as_deref(), state);
    let custom = token_normalized_flags(b.raw_flags.as_deref())
        != token_normalized_flags(canonical.as_deref());
    Some(MtpInfo {
        enabled: matches!(state, manager_models::MtpDraft::On(_)),
        n_max: match state {
            manager_models::MtpDraft::On(n) => n,
            manager_models::MtpDraft::Off => None,
        },
        parallel: manager_models::parallel_slots(b.raw_flags.as_deref()),
        custom,
    })
}

struct ParallelInfo {
    /// The explicit `--parallel` slot count, or `None` when the
    /// raw flags set none — llama.cpp's default (auto) then
    /// applies (see [`manager_models::parallel`]).
    slots: Option<i64>,
    /// True when the saved parallel spelling is not the canonical
    /// form for its state (an `-np` or `=` spelling, repeated
    /// tokens, a non-canonical position). The same comparison as
    /// the offload/MTP `custom`: the decoded state rewritten
    /// canonically must reproduce the saved tokens exactly (after
    /// token normalization, so spacing alone never counts).
    custom: bool,
}

/// A backend's parallel-slots control state (tracker #61).
/// Unlike the offload and MTP states this is not capability
/// gated: the Parallel slots field renders for every configured
/// backend, so every configured row carries it.
fn backend_parallel_info(b: &manager_config::BackendConfig) -> ParallelInfo {
    let slots = manager_models::parallel_slots(b.raw_flags.as_deref());
    let canonical = manager_models::set_parallel_slots(b.raw_flags.as_deref(), slots);
    let custom = token_normalized_flags(b.raw_flags.as_deref())
        != token_normalized_flags(canonical.as_deref());
    ParallelInfo { slots, custom }
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
///
/// When a backend is in scope and its process is running, the fit
/// check credits the weights that process already holds on each GPU
/// (its configured split applied to the GPU-resident weight total;
/// an empty split means the whole model on the first GPU): applying
/// the new split restarts the backend, freeing its current weights
/// first, so only the additional allocation per GPU must fit in free
/// VRAM. A stopped backend holds nothing and earns no credit. The
/// architecture records no launched-split separate from the
/// configured one, so the configured split is the credit basis —
/// the same basis the Placement form's default selection uses.
pub async fn op_placement_propose(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    model_id: String,
    split: Vec<f32>,
    backend_id: Option<String>,
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
                    ApiError::NotFound(format!(
                        "model {model_id} is neither a catalog entry nor a configured model"
                    ))
                })?;
            let path = resolve_under_data_dir(&cfg.data_dir, &m.gguf_path);
            let size_bytes = match std::fs::metadata(&path) {
                Ok(md) if md.is_file() => md.len(),
                _ if m.params_b > 0.0 => (m.params_b as f64 * 1e9 * 4.92 / 8.0 * 1.05) as u64,
                _ => {
                    return Err(ApiError::InvalidInput(format!(
                        "model size unknown for {model_id}: the file is not downloaded and no params_b is configured to estimate from"
                    )))
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
                expert_bytes: None,
                moe_layers: m.moe_layers,
                default_offload_layers: m.default_offload_layers,
                offload_restrictions: Vec::new(),
                mtp: false,
            }
        }
    };
    // Backend-scoped residency: when the caller names the serving
    // backend (the backend card's Placement form does), the
    // projection uses that backend's memory requirement — with
    // expert offload, the split divides only the GPU-resident
    // weights and the system-RAM share rides along in the payload.
    // The config read finishes before telemetry is touched (the
    // telemetry accessor reads the config while initializing).
    let (resident_bytes, sys_bytes, credit_cfg_split) = match &backend_id {
        Some(bid) => {
            let cfg = state.config.read().await;
            let found = cfg.backends.iter().find(|b| &b.id == bid).and_then(|b| {
                let serves = model_for_backend(&cfg, b)
                    .map(|m| m.id == model_id)
                    .unwrap_or(false);
                if serves {
                    backend_memory_requirement(&cfg, b)
                        .map(|req| (req, b.flags.tensor_split.clone()))
                } else {
                    None
                }
            });
            match found {
                Some((req, split_cfg)) if req.sys_weights_mib > 0 => (
                    Some(req.gpu_weights_mib * manager_models::MIB),
                    req.sys_weights_mib * manager_models::MIB,
                    Some(split_cfg),
                ),
                Some((_, split_cfg)) => (None, 0, Some(split_cfg)),
                None => (None, 0, None),
            }
        }
        None => (None, 0, None),
    };
    // The current-allocation credit applies only while the backend's
    // process actually holds the weights: running or starting. A
    // stopped backend's weights are already absent from VRAM, so the
    // telemetry free figures need no adjustment for it.
    let credit_active = match (&backend_id, &credit_cfg_split) {
        (Some(bid), Some(_)) => {
            let sup = state.supervisor().await.lock().await;
            sup.status().iter().any(|s| {
                &s.id == bid && matches!(s.state, ProcessState::Running | ProcessState::Starting)
            })
        }
        _ => false,
    };
    let store = state.telemetry().await;
    let guard = lock_telemetry(&store);
    let mut samples = guard.snapshot();
    samples.sort_by_key(|s| s.index);
    if samples.is_empty() {
        return Err(ApiError::Telemetry(
            "no telemetry yet; cannot project placement".to_string(),
        ));
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
    let down_indices: Vec<u32> = samples.iter().filter(|s| s.down).map(|s| s.index).collect();
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
    // The credit basis, aligned to the snapshot's GPU order: the
    // backend's configured split when it covers the current GPU set;
    // an empty configured split means an unsplit launch, which loads
    // the whole model on the first GPU. A configured split recorded
    // against a different GPU count is stale and earns no credit.
    let current_split: Option<Vec<f32>> = match (&credit_cfg_split, credit_active) {
        (Some(cfg_split), true)
            if cfg_split.len() == free_mib.len() && cfg_split.iter().any(|f| *f > 0.0) =>
        {
            Some(cfg_split.clone())
        }
        (Some(cfg_split), true) if cfg_split.is_empty() && !free_mib.is_empty() => {
            let mut v = vec![0.0; free_mib.len()];
            v[0] = 1.0;
            Some(v)
        }
        _ => None,
    };
    let projection = manager_models::propose_placement_resident(
        &spec,
        &split,
        &free_mib,
        resident_bytes,
        sys_bytes,
        current_split.as_deref(),
    )
    .map_err(ApiError::from)?;
    let mut payload = serde_json::to_value(&projection)?;
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
/// With `backend_id` given, the apply is scoped to that one backend
/// (which must serve the model): the GPUs placement form works per
/// backend, so two backends serving the same model can hold different
/// splits. The split takes effect on each backend's next start. All mutation
/// happens on a clone installed in memory only after a successful
/// save, so a late failure leaves memory and disk in agreement.
pub async fn op_placement_apply(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    model_id: String,
    split: Vec<f32>,
    backend_id: Option<String>,
) -> ApiResult {
    // Target resolution runs before telemetry validation: the
    // advanced-flags refusal below must be answerable from the
    // config alone (and targets are needed either way). The
    // telemetry accessor is only touched after the config read
    // guard is dropped — it reads the config while initializing,
    // so calling it under any config lock could deadlock (the
    // same constraint model_assign documents).
    let cfg = state.config.read().await;
    let pre_cfg = cfg.clone();
    let model = cfg
        .models
        .iter()
        .find(|m| m.id == model_id)
        .ok_or_else(|| ApiError::NotFound(format!("model {model_id}")))?;
    let backend_ids = if let Some(bid) = backend_id {
        // Backend-scoped apply (the GPUs placement form): the split
        // goes to the one selected backend, which must serve the
        // model — recorded as assigned, or its model file resolving
        // to the model's gguf_path.
        let backend = cfg
            .backends
            .iter()
            .find(|b| b.id == bid)
            .ok_or_else(|| ApiError::NotFound(format!("backend {bid}")))?;
        let model_path = resolve_under_data_dir(&cfg.data_dir, &model.gguf_path);
        let serves = model.assigned_backends.iter().any(|b| b == &bid)
            || resolve_under_data_dir(&cfg.data_dir, &backend.model_file) == model_path;
        if !serves {
            return Err(ApiError::InvalidInput(format!(
                "backend {bid} does not serve model {model_id}"
            )));
        }
        vec![bid]
    } else {
        let mut ids = model.assigned_backends.clone();
        if ids.is_empty() {
            // No recorded assignment: derive the targets from the
            // backends whose model file resolves (under the data dir)
            // to the model's gguf_path. The apply below records them
            // into assigned_backends, so later applies take the direct
            // path.
            let model_path = resolve_under_data_dir(&cfg.data_dir, &model.gguf_path);
            ids = cfg
                .backends
                .iter()
                .filter(|b| resolve_under_data_dir(&cfg.data_dir, &b.model_file) == model_path)
                .map(|b| b.id.clone())
                .collect();
        }
        ids
    };
    // Advanced flags mode (tracker #60): placement writes the
    // structured tensor split, so an apply touching an advanced
    // backend is refused whole — before telemetry validation and
    // before any task is recorded — rather than leaving the other
    // targets half-placed.
    let advanced_targets: Vec<&str> = backend_ids
        .iter()
        .filter(|bid| {
            cfg.backends
                .iter()
                .any(|b| &b.id == *bid && b.flags_advanced)
        })
        .map(|s| s.as_str())
        .collect();
    drop(cfg);
    if !advanced_targets.is_empty() {
        return Err(ApiError::InvalidInput(format!(
            "placement cannot be applied: {} in advanced flags mode; its tensor \
             split is not written by the structured placement form until \
             advanced mode is turned off (backend_set_flags_mode)",
            advanced_targets
                .iter()
                .map(|id| format!("backend {id} is"))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    if backend_ids.is_empty() {
        return Err(ApiError::NotFound(format!(
            "model {model_id} is not referenced by any backend; point a backend's model file at it first"
        )));
    }
    // Validate the split length against the telemetry snapshot.
    let gpu_count = lock_telemetry(&state.telemetry().await).snapshot().len();
    if gpu_count == 0 {
        return Err(ApiError::Telemetry(
            "no telemetry yet; cannot validate placement".to_string(),
        ));
    }
    if split.len() != gpu_count {
        // Same wording as manager_models::validate_split.
        return Err(ApiError::InvalidInput(format!(
            "tensor split has {} entries but there are {gpu_count} GPUs",
            split.len()
        )));
    }
    let task_id = state
        .record_task(
            "placement_apply",
            json!({ "model_id": model_id, "backends": backend_ids, "split": split }),
        )
        .await;
    let mut cfg = state.config.write().await;
    let outcome: Result<(), ApiError> = (async {
        let mut new_cfg = cfg.clone();
        // Existence first: a missing backend must not leave earlier
        // backends mutated while the command reports failure.
        for bid in &backend_ids {
            if !new_cfg.backends.iter().any(|b| &b.id == bid) {
                return Err(ApiError::NotFound(format!("backend {bid}")));
            }
        }
        for bid in &backend_ids {
            let backend = new_cfg
                .backends
                .iter_mut()
                .find(|b| &b.id == bid)
                .expect("existence checked above");
            manager_models::apply_placement(backend, &split)
                .map_err(ApiError::from)?;
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
            .map_err(ApiError::from)?;
        // Journal before the save (ordering rule): each target
        // backend activates the split at its next (re)start — a
        // backend-scope plan entry per target (the gateway YAML
        // does not carry splits, so no gateway entry is created
        // for this change).
        for bid in &backend_ids {
            record_backend_change(
                state,
                &pre_cfg,
                &new_cfg,
                "backend_placement",
                format!("Backend `{bid}` tensor split changed"),
                bid,
            )
            .await?;
        }
        // Persist — without the save, the applied split silently
        // vanished on app restart while the command reported success.
        manager_config::save(&new_cfg, &state.config_path)?;
        *cfg = new_cfg;
        Ok(())
    })
    .await;
    drop(cfg);
    match outcome {
        Ok(()) => {
            for bid in &backend_ids {
                state.sync_backend_service_definition(bid).await;
            }
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
                    json!({ "model_id": model_id, "error": e.to_string() }),
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
pub async fn op_task_abort(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    task_id: String,
) -> ApiResult {
    let mut tasks = state.tasks.lock().await;
    match tasks.get_mut(&task_id) {
        Some(rec) => {
            rec.status = TaskStatus::Aborted;
            Ok(json!({ "ok": true, "task_id": task_id }))
        }
        None => Err(ApiError::NotFound(format!("task {task_id}"))),
    }
}

/// All tracked tasks.
pub async fn op_tasks_list(state: &Arc<AppState>, _sink: &Arc<dyn EventSink>) -> ApiResult {
    let tasks = state.tasks.lock().await;
    let list: Vec<&TaskRecord> = tasks.values().collect();
    Ok(serde_json::to_value(list)?)
}

/// Export a diagnostics bundle (spec §14 `diagnostics_export_bundle`):
/// zip of config snapshot + per-scope recent logs + telemetry snapshot +
/// versions file. Returns the bundle path.
pub async fn op_diagnostics_export_bundle(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
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
    let telemetry_json = serde_json::to_string_pretty(&samples)?;
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
    // The wizard log joins as the "wizard" scope: step outcomes,
    // start markers for steps that never finished, and busy
    // rejections live only in that file — a bundle without it
    // cannot show why a setup step blocked or stalled.
    let (wizard_records, _) = wizard_log_records(&state.data_dir);
    if !wizard_records.is_empty() {
        let lines: Vec<String> = wizard_records
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
        log_scopes.push(("wizard".to_string(), lines));
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
    .map_err(ApiError::from)?;
    Ok(json!({ "path": path }))
}

// ---------------------------------------------------------------------------
// MCP / VS Code (§14)
// ---------------------------------------------------------------------------

/// MCP server list with live status (spec §14 `mcp_list`), built
/// from the persisted registry. Includes credential presence per
/// env key (Windows Credential Manager lookup; values never read)
/// with dual-target acceptance for the legacy servers (§10.1).
pub async fn op_mcp_list(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let registry = state.mcp_registry().await;
    let servers = state.mcp_servers().await;
    let statuses = {
        let sup = state.supervisor().await.lock().await;
        manager_mcp::list_status(&sup, &servers)
    };
    let running: HashMap<String, bool> = statuses
        .iter()
        .map(|s| (s.id.clone(), s.running))
        .collect();
    // Presence checks are blocking cmdkey calls; run off the async worker.
    let servers_for_presence = servers.clone();
    let found_targets: Vec<Option<String>> = tokio::task::spawn_blocking(move || {
        servers_for_presence
            .iter()
            .map(|s| manager_mcp::credential_target_found(s, cmdkey_target_exists))
            .collect()
    })
    .await
    .map_err(|e| ApiError::Mcp(format!("presence check failed: {e}")))?;
    let entries: Vec<McpListEntry> = registry
        .iter()
        .zip(servers.iter())
        .zip(found_targets.iter())
        .map(|((cfg, spec), found)| {
            let present = found.is_some();
            McpListEntry {
                id: cfg.id.clone(),
                display_name: spec.name.clone(),
                name: spec.name.clone(),
                transport: cfg.transport,
                exposure: cfg.exposure,
                mode: spec.mode,
                command: cfg.command.clone(),
                args: cfg.args.clone(),
                url: cfg.url.clone(),
                env_keys: cfg.env_keys.clone(),
                cred_ref: cfg.cred_ref.clone(),
                signup_url: mcp_signup_url(cfg),
                credential_hint: mcp_credential_hint(cfg),
                allowed_tools: cfg.allowed_tools.clone(),
                metered: cfg.metered,
                rpm_limit: cfg.rpm_limit,
                running: running.get(&cfg.id).copied().unwrap_or(false),
                credential_present: present,
                credential_status: if present {
                    "present".to_string()
                } else {
                    "missing".to_string()
                },
                credential_target_found: found.clone(),
                credential_presence: cfg
                    .env_keys
                    .iter()
                    .map(|k| (k.clone(), present))
                    .collect(),
            }
        })
        .collect();
    Ok(serde_json::to_value(&entries)?)
}


/// The registration / API-key page for a server's credential:
/// its own stored `signup_url` when set, else the curated seed
/// entry's for the same id — configurations seeded before the
/// field existed carry the curated servers without it, and they
/// must still surface the link.
fn mcp_signup_url(server: &manager_config::McpServerConfig) -> Option<String> {
    server
        .signup_url
        .clone()
        .filter(|u| !u.trim().is_empty())
        .or_else(|| {
            manager_config::curated_mcp_servers()
                .into_iter()
                .find(|c| c.id == server.id)
                .and_then(|c| c.signup_url)
        })
}

/// The operator-facing description of a server's credential —
/// what kind of credential to store and where it comes from:
/// its own stored `credential_hint` when set, else the seeded
/// registry entry's for the same id. Configurations persisted
/// before the field existed carry the seeded servers without
/// it, and their credential fields must still be labelled
/// (Pete, 2026-10-18). The fallback consults the full seeded
/// registry (not only the curated pair) because the legacy
/// servers' hints live in the legacy seed.
fn mcp_credential_hint(server: &manager_config::McpServerConfig) -> Option<String> {
    server
        .credential_hint
        .clone()
        .filter(|h| !h.trim().is_empty())
        .or_else(|| {
            manager_config::seeded_mcp_servers()
                .into_iter()
                .find(|c| c.id == server.id)
                .and_then(|c| c.credential_hint)
        })
}

/// The Gateway-exposed MCP servers a gateway start would omit for
/// want of a credential (Pete, 2026-10-18): the data behind the
/// start-gateway credential prompt. Each entry is a server that
/// declares env keys and names a credential target, where the
/// target is absent from Credential Manager — storing it (via
/// `mcp_store_credential`) before the start makes the start's
/// arming pass include the server in that same start; skipping
/// leaves it out, which is the start's standing behavior.
/// Desktop-only by design: non-interactive starts (service mode,
/// supervision, the REST activation apply) never consult this —
/// they omit unarmed servers silently, as before.
pub async fn op_mcp_start_preflight(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let cfg = state.config.read().await.clone();
    let presence = mcp_credential_presence(&[&cfg]).await;
    let servers: Vec<Value> = cfg
        .mcp
        .servers
        .iter()
        .filter(|s| {
            s.exposure == manager_config::McpExposure::Gateway && !s.env_keys.is_empty()
        })
        .filter_map(|s| {
            let cred_ref = s.cred_ref.as_deref().filter(|r| !r.is_empty())?;
            if presence.contains(cred_ref) {
                return None;
            }
            Some(json!({
                "id": s.id,
                "display_name": s.display_name,
                "cred_ref": cred_ref,
                "env_keys": s.env_keys,
                "signup_url": mcp_signup_url(s),
                "credential_hint": mcp_credential_hint(s),
                "metered": s.metered,
            }))
        })
        .collect();
    Ok(json!({ "servers": servers }))
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

/// Connection test for one MCP server (spec §14 `mcp_test_connection`),
/// keyed on the server's persisted EXPOSURE (MCP design §8):
/// Gateway servers are tested against the running gateway (see
/// [`mcp_test_gateway_connection`]); Local keeps the supervised-
/// process liveness check and http servers the URL probe; Disabled
/// servers report not-ok.
pub async fn op_mcp_test_connection(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    let registry = state.mcp_registry().await;
    let entry = registry
        .iter()
        .find(|s| s.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("mcp server {id}")))?;
    if entry.exposure == manager_config::McpExposure::Gateway {
        return mcp_test_gateway_connection(state, entry).await;
    }
    let servers = state.mcp_servers().await;
    let server = servers
        .iter()
        .find(|s| s.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("mcp server {id}")))?;
    match server.mode {
        manager_mcp::McpMode::Disabled => {
            Ok(json!({ "id": id, "ok": false, "detail": "server disabled" }))
        }
        manager_mcp::McpMode::Local => {
            let running = {
                let sup = state.supervisor().await.lock().await;
                manager_mcp::list_status(&sup, std::slice::from_ref(server))
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
                ApiError::Mcp(format!(
                    "remote server {id} has no url configured"
                ))
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


/// Install Continue.dev + Cline via the wizard's `vscode-extensions` step
/// (spec §9, §14 `vscode_install_extensions`). Runs `code
/// --install-extension` for the pinned extensions through the real system
/// ops; returns the step outcome.
pub async fn op_vscode_install_extensions(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let task_id = state
        .record_task("vscode_install_extensions", json!({}))
        .await;
    let cfg = state.config.read().await.clone();
    let data_dir = state.data_dir.clone();
    let ctx = WizardContext::new(
        cfg,
        data_dir,
        RunMode::Install,
        Arc::new(RealSystemOps::default()),
    );
    let mut wizard = Wizard::new(
        vec![Box::new(VscodeExtensions::default())],
        RunMode::Install,
    )?;
    let report = wizard.run_all(&ctx).await;
    let (done, total) = report.progress();
    state
        .finish_task(
            &task_id,
            TaskStatus::Done,
            json!({ "done": done, "total": total }),
        )
        .await;
    Ok(serde_json::to_value(&report)?)
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
pub async fn op_vscode_write_config(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    backup: bool,
) -> ApiResult {
    let cfg = state.config.read().await;
    // The editor client authenticates with the gateway master key;
    // ensure (generate + store on first use) so the written config
    // works against the key-guarded gateway. The key itself goes to
    // ~/.continue/.env — config.yaml references it as
    // ${{ secrets.LITELLM_API_KEY }} and carries no literal secret.
    let master_key = manager_wizard::ensure_gateway_master_key(&RealSystemOps::default()).await?;
    let new_yaml = continue_config_yaml(&cfg);
    let new_models: Value = serde_yaml::from_str::<Value>(&new_yaml)
        .map_err(|e| ApiError::Config(e.to_string()))?
        .get("models")
        .cloned()
        .unwrap_or(Value::Null);
    drop(cfg);

    let staged_dir = state.data_dir.join("vscode");
    std::fs::create_dir_all(&staged_dir).map_err(ApiError::from)?;
    let staged = staged_dir.join("continue-config.yaml");
    std::fs::write(&staged, new_yaml.as_bytes()).map_err(ApiError::from)?;
    std::fs::write(
        staged_dir.join("continue.env"),
        format!("LITELLM_API_KEY={master_key}\n"),
    )
    .map_err(ApiError::from)?;

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
    std::fs::create_dir_all(&dir).map_err(ApiError::from)?;
    let canonical = dir.join("config.yaml");
    let legacy = dir.join("config.json");
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut old_models = Value::Null;
    let mut backup_path = Value::Null;
    if canonical.exists() {
        let existing = std::fs::read_to_string(&canonical).map_err(ApiError::from)?;
        old_models = serde_yaml::from_str::<Value>(&existing)
            .ok()
            .and_then(|v| v.get("models").cloned())
            .unwrap_or(Value::Null);
        let bak = dir.join(format!("config.yaml.bak-{ts_ms}"));
        std::fs::rename(&canonical, &bak).map_err(ApiError::from)?;
        backup_path = serde_json::to_value(&bak)?;
    } else if legacy.exists() {
        // No YAML yet: the diff's old side is the legacy JSON config.
        let existing = std::fs::read_to_string(&legacy).map_err(ApiError::from)?;
        old_models = serde_json::from_str::<Value>(&existing)
            .ok()
            .and_then(|v| v.get("models").cloned())
            .unwrap_or(Value::Null);
    }
    std::fs::write(&canonical, new_yaml.as_bytes()).map_err(ApiError::from)?;
    // The .env secret: merged by key, never backed up or replaced.
    let env_path = dir.join(".env");
    let existing_env = std::fs::read_to_string(&env_path).unwrap_or_default();
    std::fs::write(
        &env_path,
        merge_env_contents(&existing_env, "LITELLM_API_KEY", &master_key),
    )
    .map_err(ApiError::from)?;
    // A legacy config.json is inert once config.yaml exists, but if
    // the YAML is ever removed the JSON — with its stale literal
    // key — becomes live again. Back it aside with the same backup.
    let mut legacy_backup_path = Value::Null;
    if legacy.exists() {
        let bak = dir.join(format!("config.json.bak-{ts_ms}"));
        std::fs::rename(&legacy, &bak).map_err(ApiError::from)?;
        legacy_backup_path = serde_json::to_value(&bak)?;
    }
    out["canonical"] = serde_json::to_value(&canonical)?;
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

// ---------------------------------------------------------------------------
// Windows ML (§14)
// ---------------------------------------------------------------------------

/// Probe WinMLServer availability; limits recorded verbatim.
pub async fn op_winml_probe(state: &Arc<AppState>, _sink: &Arc<dyn EventSink>) -> ApiResult {
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

// ---------------------------------------------------------------------------
// Updates (§14)
// ---------------------------------------------------------------------------

/// Check all components for updates. Explicitly deferred: spec §16 puts
/// Updates in P6 (post-v1); per-component pin/auto-update toggles work now.
pub async fn op_updates_check_all(state: &Arc<AppState>, _sink: &Arc<dyn EventSink>) -> ApiResult {
    let _ = state;
    Err(ApiError::Unsupported(
        "Update checking ships in P6; per-component pin/auto-update toggles work now".to_string(),
    ))
}

/// Apply an update. Explicitly deferred: spec §16 puts Updates in P6
/// (post-v1); per-component pin/auto-update toggles work now.
pub async fn op_updates_apply(
    _state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    component_id: String,
) -> ApiResult {
    let _ = component_id;
    Err(ApiError::Unsupported(
        "Update checking ships in P6; per-component pin/auto-update toggles work now".to_string(),
    ))
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

// ---------------------------------------------------------------------------
// Entrypoint
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg() -> AppConfig {
        let mut cfg = AppConfig::default_config();
        cfg.data_dir = PathBuf::from("/data");
        cfg
    }

    /// An AppState over a temp dir for the advanced-flags op
    /// tests (tracker #60): config file and data dir in scratch
    /// space, so saves and the activation journal land there.
    fn advanced_test_state(cfg: AppConfig) -> (Arc<AppState>, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "llm-mgr-advanced-flags-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = cfg;
        cfg.data_dir = dir.clone();
        let state = Arc::new(AppState::new(cfg, dir.join("config.json"), dir.clone()));
        (state, dir)
    }

    fn test_sink() -> Arc<dyn EventSink> {
        Arc::new(NoopSink)
    }

    /// Code review B-3: user creation refuses the reserved
    /// `opt-exp-*` experiment namespace, naming the reservation.
    #[tokio::test]
    async fn backend_add_refuses_reserved_temp_prefix() {
        let (state, dir) = advanced_test_state(test_cfg());
        let sink = test_sink();
        let r = op_backend_add(
            &state,
            &sink,
            "opt-exp-1".to_string(),
            "qwen3.8-27b".to_string(),
            9999,
            "planner".to_string(),
            false,
            true,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        match r {
            Err(ApiError::InvalidInput(msg)) => {
                assert!(msg.contains("reserved"), "{msg}");
                assert!(msg.contains("opt-exp-"), "{msg}");
            }
            other => panic!("expected the reservation refusal, got {other:?}"),
        }
        // Nothing was created.
        let cfg = state.config.read().await;
        assert!(!cfg.backends.iter().any(|b| b.id == "opt-exp-1"));
        drop(cfg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Groups page membership (predicted-throughput design
    /// §8.5): the move joins exactly one group and leaves every
    /// other; the response names the group it left.
    #[tokio::test]
    async fn gateway_set_backend_group_moves_membership() {
        let (state, dir) = advanced_test_state(test_cfg());
        let sink = test_sink();
        let r = op_gateway_set_backend_group(
            &state,
            &sink,
            "coder".to_string(),
            Some("planner".to_string()),
        )
        .await
        .unwrap();
        assert_eq!(r["changed"], json!(true));
        assert_eq!(r["group"], json!("planner"));
        assert_eq!(r["removed_from"], json!(["coder"]));
        let cfg = state.config.read().await;
        let memberships: Vec<&str> = cfg
            .gateway
            .groups
            .iter()
            .filter(|g| g.members.iter().any(|m| m == "coder"))
            .map(|g| g.name.as_str())
            .collect();
        assert_eq!(memberships, vec!["planner"]);
        drop(cfg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A move onto the lane the backend solely occupies — and an
    /// ungroup of an already-ungrouped backend — succeed without
    /// a write (design §8.4: drops are idempotent).
    #[tokio::test]
    async fn gateway_set_backend_group_noop_drops_do_not_write() {
        let (state, dir) = advanced_test_state(test_cfg());
        let sink = test_sink();
        let r = op_gateway_set_backend_group(
            &state,
            &sink,
            "coder".to_string(),
            Some("coder".to_string()),
        )
        .await
        .unwrap();
        assert_eq!(r["changed"], json!(false));
        assert_eq!(r["removed_from"], json!([]));
        // Ungroup once (a real change), then again (a no-op).
        let r = op_gateway_set_backend_group(&state, &sink, "coder".to_string(), None)
            .await
            .unwrap();
        assert_eq!(r["changed"], json!(true));
        assert_eq!(r["group"], Value::Null);
        let r = op_gateway_set_backend_group(&state, &sink, "coder".to_string(), None)
            .await
            .unwrap();
        assert_eq!(r["changed"], json!(false));
        let cfg = state.config.read().await;
        assert!(!cfg
            .gateway
            .groups
            .iter()
            .any(|g| g.members.iter().any(|m| m == "coder")));
        drop(cfg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A hand-edited config can list one backend in several
    /// groups; a move collapses it to the single target group
    /// and reports every group it was removed from (design §8.3).
    #[tokio::test]
    async fn gateway_set_backend_group_collapses_multi_membership() {
        let mut cfg = test_cfg();
        // coder is seeded into group "coder"; add it to "planner" too.
        cfg.gateway
            .groups
            .iter_mut()
            .find(|g| g.name == "planner")
            .unwrap()
            .members
            .push("coder".to_string());
        let (state, dir) = advanced_test_state(cfg);
        let sink = test_sink();
        let r = op_gateway_set_backend_group(
            &state,
            &sink,
            "coder".to_string(),
            Some("tool-runner".to_string()),
        )
        .await
        .unwrap();
        assert_eq!(r["changed"], json!(true));
        let mut removed: Vec<String> =
            serde_json::from_value(r["removed_from"].clone()).unwrap();
        removed.sort();
        assert_eq!(removed, vec!["coder".to_string(), "planner".to_string()]);
        let cfg = state.config.read().await;
        let memberships: Vec<&str> = cfg
            .gateway
            .groups
            .iter()
            .filter(|g| g.members.iter().any(|m| m == "coder"))
            .map(|g| g.name.as_str())
            .collect();
        assert_eq!(memberships, vec!["tool-runner"]);
        drop(cfg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Unknown backends and groups are NotFound; an empty group
    /// string is refused rather than read as ungroup.
    #[tokio::test]
    async fn gateway_set_backend_group_rejects_unknowns() {
        let (state, dir) = advanced_test_state(test_cfg());
        let sink = test_sink();
        let r = op_gateway_set_backend_group(
            &state,
            &sink,
            "nope".to_string(),
            Some("planner".to_string()),
        )
        .await;
        assert!(matches!(r, Err(ApiError::NotFound(_))), "{r:?}");
        let r = op_gateway_set_backend_group(
            &state,
            &sink,
            "coder".to_string(),
            Some("nope".to_string()),
        )
        .await;
        assert!(matches!(r, Err(ApiError::NotFound(_))), "{r:?}");
        let r = op_gateway_set_backend_group(
            &state,
            &sink,
            "coder".to_string(),
            Some("  ".to_string()),
        )
        .await;
        assert!(matches!(r, Err(ApiError::InvalidInput(_))), "{r:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Group lifecycle (design §8.5/D11): create makes an empty
    /// group under the slug rule and refuses duplicates; delete
    /// refuses a non-empty group with its member count and
    /// deletes an empty one.
    #[tokio::test]
    async fn gateway_group_create_and_delete() {
        let (state, dir) = advanced_test_state(test_cfg());
        let sink = test_sink();
        let r = op_gateway_create_group(&state, &sink, "batch-jobs".to_string())
            .await
            .unwrap();
        assert_eq!(r["name"], json!("batch-jobs"));
        {
            let cfg = state.config.read().await;
            let g = cfg
                .gateway
                .groups
                .iter()
                .find(|g| g.name == "batch-jobs")
                .unwrap();
            assert!(g.members.is_empty());
            assert_eq!(g.strategy, manager_config::RoutingStrategy::SimpleShuffle);
        }
        let r = op_gateway_create_group(&state, &sink, "batch-jobs".to_string()).await;
        assert!(matches!(r, Err(ApiError::InvalidInput(_))), "{r:?}");
        let r = op_gateway_create_group(&state, &sink, "Bad Name".to_string()).await;
        assert!(matches!(r, Err(ApiError::InvalidInput(_))), "{r:?}");
        // Non-empty delete is refused, naming the count.
        let r = op_gateway_delete_group(&state, &sink, "coder".to_string()).await;
        match r {
            Err(ApiError::InvalidInput(msg)) => {
                assert!(msg.contains("not empty"), "{msg}");
                assert!(msg.contains('1'), "{msg}");
            }
            other => panic!("expected the not-empty refusal, got {other:?}"),
        }
        let r = op_gateway_delete_group(&state, &sink, "batch-jobs".to_string())
            .await
            .unwrap();
        assert_eq!(r["deleted"], json!(true));
        let r = op_gateway_delete_group(&state, &sink, "batch-jobs".to_string()).await;
        assert!(matches!(r, Err(ApiError::NotFound(_))), "{r:?}");
        let cfg = state.config.read().await;
        assert!(!cfg.gateway.groups.iter().any(|g| g.name == "batch-jobs"));
        drop(cfg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Code review B-3: the whole-config patch path may not
    /// smuggle in a new backend in the reserved namespace either.
    #[tokio::test]
    async fn update_config_refuses_a_new_reserved_temp_backend() {
        let (state, dir) = advanced_test_state(test_cfg());
        let sink = test_sink();
        let mut v = serde_json::to_value(state.config.read().await.clone()).unwrap();
        let mut extra = v["backends"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["id"] == "coder")
            .unwrap()
            .clone();
        extra["id"] = json!("opt-exp-9");
        extra["port"] = json!(9999);
        extra["enabled"] = json!(false);
        v["backends"].as_array_mut().unwrap().push(extra);
        let patch = json!({ "backends": v["backends"].clone() });
        match op_update_config(&state, &sink, patch).await {
            Err(ApiError::InvalidInput(msg)) => {
                assert!(msg.contains("reserved"), "{msg}");
                assert!(msg.contains("opt-exp-"), "{msg}");
            }
            other => panic!("expected the reservation refusal, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Code review B-3: the reservation guards creation only —
    /// a config that already carries a reserved id (a pre-fix
    /// leftover, which the startup sweep owns) is not newly
    /// rejected by an unrelated patch.
    #[tokio::test]
    async fn update_config_leaves_a_preexisting_reserved_temp_backend_alone() {
        let mut cfg = test_cfg();
        let mut extra = cfg
            .backends
            .iter()
            .find(|b| b.id == "coder")
            .unwrap()
            .clone();
        extra.id = "opt-exp-9".to_string();
        extra.port = 9999;
        extra.enabled = false;
        cfg.backends.push(extra);
        let (state, dir) = advanced_test_state(cfg);
        let sink = test_sink();
        let r = op_update_config(&state, &sink, json!({ "supervision": { "mode": "process" } }))
            .await
            .unwrap();
        assert_eq!(r["ok"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Settings-pages sweep (Oct 19, 2026): every field the sweep
    /// wired to a Settings/Gateway editor must round-trip through
    /// its real save path — op call, save, reload from disk shows
    /// the new value. Covers the dedicated ops (notifications,
    /// telemetry interval, updates pin/auto), the generic config
    /// patch (Postgres port, Admin API port, WinML, MXC policy
    /// path), and the gateway config patch (port, OpenRouter tier,
    /// group fallbacks).
    #[tokio::test]
    async fn settings_controls_round_trip_through_real_ops() {
        let (state, dir) = advanced_test_state(test_cfg());
        let sink = test_sink();

        // Notifications master: to false, and back to true.
        let notif = manager_config::NotificationConfig {
            enabled: false,
            ..Default::default()
        };
        op_notifications_set_config(&state, &sink, notif).await.unwrap();
        let loaded = manager_config::load(&state.config_path).unwrap();
        assert!(!loaded.notifications.enabled);
        let notif = manager_config::NotificationConfig {
            enabled: true,
            ..Default::default()
        };
        op_notifications_set_config(&state, &sink, notif).await.unwrap();
        let loaded = manager_config::load(&state.config_path).unwrap();
        assert!(loaded.notifications.enabled);

        // Telemetry poll interval.
        op_telemetry_set_interval(&state, &sink, 2500).await.unwrap();
        let loaded = manager_config::load(&state.config_path).unwrap();
        assert_eq!(loaded.telemetry.poll_interval_ms, 2500);

        // Generic config patch: Postgres port, Admin API port,
        // WinML enable + port + model, MXC policy path.
        op_update_config(&state, &sink, json!({ "postgres": { "port": 54330 } }))
            .await
            .unwrap();
        op_update_config(&state, &sink, json!({ "admin_api": { "port": 47822 } }))
            .await
            .unwrap();
        op_update_config(
            &state,
            &sink,
            json!({ "winml": { "enabled": true, "port": 8091, "model_id": "qwen3-8b" } }),
        )
        .await
        .unwrap();
        op_update_config(&state, &sink, json!({ "mxc": { "path": "/tmp/custom-policy.json" } }))
            .await
            .unwrap();
        let loaded = manager_config::load(&state.config_path).unwrap();
        assert_eq!(loaded.postgres.port, 54330);
        assert_eq!(loaded.admin_api.port, 47822);
        assert!(loaded.winml.enabled);
        assert_eq!(loaded.winml.port, 8091);
        assert_eq!(loaded.winml.model_id.as_deref(), Some("qwen3-8b"));
        assert_eq!(loaded.mxc.path, PathBuf::from("/tmp/custom-policy.json"));

        // Component pin + auto-update, then unpin + re-enable.
        op_updates_pin(&state, &sink, "litellm".to_string(), Some("1.105.1".to_string()))
            .await
            .unwrap();
        op_updates_set_auto(&state, &sink, "litellm".to_string(), false)
            .await
            .unwrap();
        let loaded = manager_config::load(&state.config_path).unwrap();
        let comp = loaded.updates.components.get("litellm").unwrap();
        assert_eq!(comp.pinned_version.as_deref(), Some("1.105.1"));
        assert!(!comp.auto_update);
        op_updates_pin(&state, &sink, "litellm".to_string(), None)
            .await
            .unwrap();
        op_updates_set_auto(&state, &sink, "litellm".to_string(), true)
            .await
            .unwrap();
        let loaded = manager_config::load(&state.config_path).unwrap();
        let comp = loaded.updates.components.get("litellm").unwrap();
        assert_eq!(comp.pinned_version, None);
        assert!(comp.auto_update);

        // Gateway patch: port, the OpenRouter tier fields, and one
        // group's fallback chain (the full groups array rides
        // through in stored form, only the chain changes).
        op_update_gateway_config(&state, &sink, json!({ "port": 4001 }))
            .await
            .unwrap();
        op_update_gateway_config(
            &state,
            &sink,
            json!({ "openrouter": { "cred_ref": "openrouter-key", "daily_cap_usd": 12.5, "model_allowlist": ["openai/gpt-5"] } }),
        )
        .await
        .unwrap();
        let mut groups_patch =
            serde_json::to_value(&state.config.read().await.gateway.groups).unwrap();
        groups_patch.as_array_mut().unwrap()[0]["fallbacks"] = json!(["openrouter"]);
        op_update_gateway_config(&state, &sink, json!({ "groups": groups_patch }))
            .await
            .unwrap();
        let loaded = manager_config::load(&state.config_path).unwrap();
        assert_eq!(loaded.gateway.port, 4001);
        assert_eq!(loaded.gateway.openrouter.cred_ref.as_deref(), Some("openrouter-key"));
        assert_eq!(loaded.gateway.openrouter.daily_cap_usd, 12.5);
        assert_eq!(loaded.gateway.openrouter.model_allowlist, vec!["openai/gpt-5".to_string()]);
        assert_eq!(loaded.gateway.groups[0].fallbacks, vec!["openrouter".to_string()]);
        // The groups patch must not reset the stored purpose.
        assert_eq!(
            loaded.gateway.groups[0].purpose,
            test_cfg().gateway.groups[0].purpose
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn advanced_mode_flip_writes_no_flags_and_no_journal() {
        let mut cfg = test_cfg();
        // A deliberately non-canonical raw string (an `=` spelling
        // and doubled spacing): any canonicalizing writer would
        // rewrite it, so byte equality proves the flip wrote no
        // flags (tracker #60, requirement: entry performs no write).
        cfg.backends
            .iter_mut()
            .find(|b| b.id == "coder")
            .unwrap()
            .raw_flags = Some("--n-cpu-moe=46  --threads 8".to_string());
        let (state, dir) = advanced_test_state(cfg);
        let sink = test_sink();
        let r = op_backend_set_flags_mode(&state, &sink, "coder".to_string(), true)
            .await
            .unwrap();
        assert_eq!(r["flags_advanced"], true);
        assert_eq!(r["changed"], true);
        assert_eq!(r["raw_flags"], "--n-cpu-moe=46  --threads 8");
        // On disk: the string is byte-identical and the mode is on.
        let text = std::fs::read_to_string(dir.join("config.json")).unwrap();
        assert!(text.contains("--n-cpu-moe=46  --threads 8"), "{text}");
        assert!(text.contains("\"flags_advanced\": true"), "{text}");
        // A mode flip is not an activation change: no journal entry.
        assert!(activation::load_journal(&dir).entries.is_empty());
        // Re-flipping to the same state writes nothing.
        let r = op_backend_set_flags_mode(&state, &sink, "coder".to_string(), true)
            .await
            .unwrap();
        assert_eq!(r["changed"], false);
        // Leaving the mode also writes no flags.
        let r = op_backend_set_flags_mode(&state, &sink, "coder".to_string(), false)
            .await
            .unwrap();
        assert_eq!(r["raw_flags"], "--n-cpu-moe=46  --threads 8");
        let saved = manager_config::load(&state.config_path).unwrap();
        let coder = saved.backends.iter().find(|b| b.id == "coder").unwrap();
        assert!(!coder.flags_advanced);
        assert_eq!(coder.raw_flags.as_deref(), Some("--n-cpu-moe=46  --threads 8"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn raw_flags_save_requires_mode_journals_and_warns_without_blocking() {
        let mut cfg = test_cfg();
        cfg.backends
            .iter_mut()
            .find(|b| b.id == "planner")
            .unwrap()
            .tool_call_probe = Some(manager_config::ToolCallProbe {
            outcome: manager_config::ToolCallProbeOutcome::Verified,
            at_ms: 1,
        });
        let (state, dir) = advanced_test_state(cfg);
        let sink = test_sink();
        // Structured mode: the dedicated save refuses — the
        // structured writers own the string there.
        let err = op_backend_set_raw_flags(
            &state,
            &sink,
            "planner".to_string(),
            Some("--parallel 1".to_string()),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&err, ApiError::InvalidInput(m) if m.contains("not in advanced flags mode")),
            "{err}"
        );
        op_backend_set_flags_mode(&state, &sink, "planner".to_string(), true)
            .await
            .unwrap();
        const RAW: &str = "--parallel 1 --spec-type draft-mtp --spec-draft-n-max 2 --threads 8";
        let r = op_backend_set_raw_flags(&state, &sink, "planner".to_string(), Some(RAW.to_string()))
            .await
            .unwrap();
        assert_eq!(r["changed"], true);
        assert_eq!(r["raw_flags"], RAW);
        assert!(r["warnings"].as_array().unwrap().is_empty());
        // Journaled as a backend-scope activation change, and the
        // tool-call probe was cleared by the flags change.
        let journal = activation::load_journal(&dir);
        let entries: Vec<_> = journal
            .entries
            .iter()
            .filter(|e| e.kind == "backend_set_raw_flags")
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].scope, "backend");
        assert_eq!(entries[0].targets, vec!["planner".to_string()]);
        let saved = manager_config::load(&state.config_path).unwrap();
        let planner = saved.backends.iter().find(|b| b.id == "planner").unwrap();
        assert_eq!(planner.raw_flags.as_deref(), Some(RAW));
        assert_eq!(planner.tool_call_probe, None);
        // A no-change save journals nothing new.
        let r = op_backend_set_raw_flags(&state, &sink, "planner".to_string(), Some(RAW.to_string()))
            .await
            .unwrap();
        assert_eq!(r["changed"], false);
        assert_eq!(
            activation::load_journal(&dir)
                .entries
                .iter()
                .filter(|e| e.kind == "backend_set_raw_flags")
                .count(),
            1
        );
        // Warn, never block: a malformed value and an unrecognized
        // flag both save, with the warnings on the response.
        let r = op_backend_set_raw_flags(
            &state,
            &sink,
            "planner".to_string(),
            Some("--parallel abc --bogus-flag 3".to_string()),
        )
        .await
        .unwrap();
        assert_eq!(r["changed"], true);
        let warnings = r["warnings"].as_array().unwrap();
        assert_eq!(warnings.len(), 2);
        assert!(warnings.iter().any(|w| w["token"] == "--parallel"));
        assert!(warnings.iter().any(|w| w["token"] == "--bogus-flag"));
        // An all-whitespace save clears the flags entirely.
        let r = op_backend_set_raw_flags(&state, &sink, "planner".to_string(), Some("   ".to_string()))
            .await
            .unwrap();
        assert_eq!(r["changed"], true);
        assert_eq!(r["raw_flags"], Value::Null);
        let saved = manager_config::load(&state.config_path).unwrap();
        assert_eq!(
            saved.backends.iter().find(|b| b.id == "planner").unwrap().raw_flags,
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn structured_writers_refused_in_advanced_mode() {
        let mut cfg = test_cfg();
        for id in ["planner", "coder"] {
            cfg.backends
                .iter_mut()
                .find(|b| b.id == id)
                .unwrap()
                .flags_advanced = true;
        }
        // Give planner an MTP-capable model so the MTP refusal is
        // distinguishable from the capability refusal.
        serve_model(
            &mut cfg,
            "planner",
            "qwen3.8-27b-mtp",
            "models/Qwen3.8-27B-MTP.gguf",
            Some("--parallel 1"),
        );
        let (state, dir) = advanced_test_state(cfg);
        let sink = test_sink();
        let refused = |r: Result<Value, ApiError>| match r {
            Err(ApiError::InvalidInput(m)) => m.contains("advanced flags mode"),
            other => panic!("expected the advanced-mode refusal, got {other:?}"),
        };
        assert!(refused(
            op_backend_update_flags(&state, &sink, "planner".to_string(), 32768, 512).await
        ));
        assert!(refused(
            op_backend_set_offload(&state, &sink, "coder".to_string(), 4, None).await
        ));
        assert!(refused(
            op_backend_set_mtp(&state, &sink, "planner".to_string(), Some(2)).await
        ));
        assert!(refused(
            op_placement_apply(
                &state,
                &sink,
                "qwen3.8-27b-mtp".to_string(),
                vec![1.0],
                Some("planner".to_string()),
            )
            .await
        ));
        // The config patch path refuses a flags-field patch...
        assert!(refused(
            op_backend_apply_config(
                &state,
                &sink,
                "planner".to_string(),
                json!({ "n_ctx": 4096 }),
                "OnNextRestart".to_string(),
            )
            .await
        ));
        // ...but a non-flags patch still applies in the mode.
        let r = op_backend_apply_config(
            &state,
            &sink,
            "planner".to_string(),
            json!({ "enabled": false }),
            "OnNextRestart".to_string(),
        )
        .await
        .unwrap();
        assert_eq!(r["persisted"], true);
        // update_config cannot rewrite an advanced backend's flags.
        let mut v = serde_json::to_value(state.config.read().await.clone()).unwrap();
        for b in v["backends"].as_array_mut().unwrap().iter_mut() {
            if b["id"] == "coder" {
                b["raw_flags"] = json!("--threads 4");
            }
        }
        let patch = json!({ "backends": v["backends"].clone() });
        assert!(refused(op_update_config(&state, &sink, patch).await));
        // A benign whole-config patch (the supervision toggle the
        // Settings view sends) still succeeds.
        let r = op_update_config(&state, &sink, json!({ "supervision": { "mode": "process" } }))
            .await
            .unwrap();
        assert_eq!(r["ok"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn model_assign_leaves_advanced_backends_flags_untouched() {
        let nemotron = manager_config::ModelConfig {
            id: "nemotron-3.5-lightning-30b".to_string(),
            gguf_path: PathBuf::from("models/nemotron.gguf"),
            quant: "Q4_K_M".to_string(),
            params_b: 30.0,
            sha256: "a".repeat(64),
            verified_at: None,
            verified_sha256: None,
            source_url: String::new(),
            assigned_backends: Vec::new(),
            enabled: true,
            display_name: None,
            moe_layers: None,
            default_offload_layers: None,
        };
        // Advanced: the assignment proceeds, but neither the
        // catalog raw-flags fill (the backend's blank flags would
        // take Nemotron's --n-cpu-moe 16 in structured mode) nor
        // the split application touches the backend's flags.
        let mut cfg = test_cfg();
        cfg.models.push(nemotron.clone());
        {
            let coder = cfg.backends.iter_mut().find(|b| b.id == "coder").unwrap();
            coder.flags_advanced = true;
            coder.raw_flags = Some("  ".to_string());
            coder.flags.tensor_split = Vec::new();
        }
        let (state, dir) = advanced_test_state(cfg);
        // Pin an empty telemetry store: op_model_assign skips its
        // split-vs-GPU-count check when no telemetry has polled (its
        // documented no-telemetry path). Against live host telemetry
        // the fixed [1.0] split is rejected on multi-GPU machines,
        // which is unrelated to the flags behavior under test.
        let _ = state.telemetry.set(Arc::new(std::sync::Mutex::new(
            TelemetryStore::new(),
        )));
        let sink = test_sink();
        let r = op_model_assign(
            &state,
            &sink,
            "nemotron-3.5-lightning-30b".to_string(),
            "coder".to_string(),
            vec![1.0],
        )
        .await
        .unwrap();
        assert_eq!(r["flags_untouched"], true);
        let saved = manager_config::load(&state.config_path).unwrap();
        let coder = saved.backends.iter().find(|b| b.id == "coder").unwrap();
        assert_eq!(coder.model_file, PathBuf::from("models/nemotron.gguf"));
        assert_eq!(coder.raw_flags.as_deref(), Some("  "));
        assert!(coder.flags.tensor_split.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
        // Structured control case: the same assignment fills the
        // catalog default and applies the split. (split_mode is
        // preset to Layer: a bare [1.0] split under mode none is
        // invalid config, unrelated to this test.)
        let mut cfg = test_cfg();
        cfg.models.push(nemotron);
        {
            let coder = cfg.backends.iter_mut().find(|b| b.id == "coder").unwrap();
            coder.raw_flags = Some("  ".to_string());
            coder.flags.tensor_split = Vec::new();
            coder.flags.split_mode = manager_config::SplitMode::Layer;
        }
        let (state, dir) = advanced_test_state(cfg);
        // Same empty-telemetry pin as the advanced case above.
        let _ = state.telemetry.set(Arc::new(std::sync::Mutex::new(
            TelemetryStore::new(),
        )));
        let r = op_model_assign(
            &state,
            &sink,
            "nemotron-3.5-lightning-30b".to_string(),
            "coder".to_string(),
            vec![1.0],
        )
        .await
        .unwrap();
        assert_eq!(r["flags_untouched"], false);
        let saved = manager_config::load(&state.config_path).unwrap();
        let coder = saved.backends.iter().find(|b| b.id == "coder").unwrap();
        assert_eq!(coder.raw_flags.as_deref(), Some("--n-cpu-moe 16"));
        assert_eq!(coder.flags.tensor_split, vec![1.0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn model_assign_failure_leaves_config_and_journal_untouched() {
        // Code review B-4: op_model_assign used to mutate the live
        // config ahead of its fallible steps, so a failure after the
        // mutation point left the in-memory config half-migrated —
        // and the next unrelated save persisted it with no journal
        // entry. Here apply_placement rejects the split (sums to
        // 0.5) after model_file was set and the catalog raw-flags
        // fill was written: exactly the traced half-migration. The
        // op now mutates a clone and commits only after placement,
        // validation, journaling, and the save all succeed.
        let nemotron = manager_config::ModelConfig {
            id: "nemotron-3.5-lightning-30b".to_string(),
            gguf_path: PathBuf::from("models/nemotron.gguf"),
            quant: "Q4_K_M".to_string(),
            params_b: 30.0,
            sha256: "a".repeat(64),
            verified_at: None,
            verified_sha256: None,
            source_url: String::new(),
            assigned_backends: Vec::new(),
            enabled: true,
            display_name: None,
            moe_layers: None,
            default_offload_layers: None,
        };
        let mut cfg = test_cfg();
        cfg.models.push(nemotron);
        {
            let coder = cfg.backends.iter_mut().find(|b| b.id == "coder").unwrap();
            // Blank flags: the assignment would fill the catalog
            // default (--n-cpu-moe 16) before placement fails.
            coder.raw_flags = Some("  ".to_string());
            coder.flags.tensor_split = Vec::new();
            coder.flags.split_mode = manager_config::SplitMode::Layer;
        }
        let (state, dir) = advanced_test_state(cfg);
        let sink = test_sink();
        let before = serde_json::to_value(state.config.read().await.clone()).unwrap();
        let r = op_model_assign(
            &state,
            &sink,
            "nemotron-3.5-lightning-30b".to_string(),
            "coder".to_string(),
            vec![0.5],
        )
        .await;
        assert!(r.is_err(), "a split summing to 0.5 must be refused: {r:?}");
        let after = serde_json::to_value(state.config.read().await.clone()).unwrap();
        assert_eq!(
            before, after,
            "a failed assign must leave the live config byte-identical"
        );
        assert!(
            activation::load_journal(&state.data_dir).entries.is_empty(),
            "a failed assign must not journal"
        );
        // The state is not wedged: a valid assign right after the
        // failure succeeds and persists in full.
        let r = op_model_assign(
            &state,
            &sink,
            "nemotron-3.5-lightning-30b".to_string(),
            "coder".to_string(),
            vec![1.0],
        )
        .await
        .unwrap();
        assert_eq!(r["persisted"], true);
        let saved = manager_config::load(&state.config_path).unwrap();
        let coder = saved.backends.iter().find(|b| b.id == "coder").unwrap();
        assert_eq!(coder.model_file, PathBuf::from("models/nemotron.gguf"));
        assert_eq!(coder.raw_flags.as_deref(), Some("--n-cpu-moe 16"));
        assert_eq!(coder.flags.tensor_split, vec![1.0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn advanced_save_then_structured_decode_round_trip() {
        let mut cfg = test_cfg();
        serve_model(
            &mut cfg,
            "planner",
            "qwen3.8-27b-mtp",
            "models/Qwen3.8-27B-MTP.gguf",
            None,
        );
        cfg.backends
            .iter_mut()
            .find(|b| b.id == "planner")
            .unwrap()
            .flags_advanced = true;
        let (state, dir) = advanced_test_state(cfg);
        let sink = test_sink();
        // A mix of modeled flags (offload, MTP, parallel) and a
        // foreign flag no control models (--threads).
        const RAW: &str =
            "--parallel 1 --spec-type draft-mtp --spec-draft-n-max 2 --threads 8 -ot exps=CPU";
        let r = op_backend_set_raw_flags(&state, &sink, "planner".to_string(), Some(RAW.to_string()))
            .await
            .unwrap();
        assert_eq!(r["changed"], true);
        assert!(r["warnings"].as_array().unwrap().is_empty());
        // Leaving the mode writes nothing: the string survives
        // byte-for-byte, foreign tokens included.
        let r = op_backend_set_flags_mode(&state, &sink, "planner".to_string(), false)
            .await
            .unwrap();
        assert_eq!(r["raw_flags"], RAW);
        let saved = manager_config::load(&state.config_path).unwrap();
        let b = saved.backends.iter().find(|b| b.id == "planner").unwrap();
        assert!(!b.flags_advanced);
        assert_eq!(b.raw_flags.as_deref(), Some(RAW));
        // The existing decoders read the saved string exactly as
        // the structured form renders it.
        assert_eq!(
            manager_models::expert_offload(b.raw_flags.as_deref()),
            manager_models::ExpertOffload::AllExperts
        );
        assert_eq!(
            manager_models::mtp_draft(b.raw_flags.as_deref()),
            manager_models::MtpDraft::On(Some(2))
        );
        assert_eq!(manager_models::parallel_slots(b.raw_flags.as_deref()), Some(1));
        let info = backend_mtp_info(&saved, b).expect("planner-mtp is MTP-capable");
        assert!(info.enabled);
        assert_eq!(info.n_max, Some(2));
        assert_eq!(info.parallel, Some(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn optimize_log_roundtrip_with_previous_file() {
        // Tracker #52: the optimize scope's append/reader pair —
        // lines land in logs/optimize.log under source "optimize",
        // and a rotated optimize.log.prev reads back archived
        // with has_previous set (the Logs-view scope contract).
        let dir = std::env::temp_dir().join(format!(
            "llm-mgr-optimize-log-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        std::fs::write(
            dir.join("logs").join("optimize.log.prev"),
            "[2026-01-01T00:00:00Z] earlier run line\n",
        )
        .unwrap();
        append_optimize_log(
            &dir,
            &["[2026-01-01T00:01:00Z] consultation start".to_string()],
        );
        append_optimize_log(&dir, &[]); // empty appends are no-ops
        let (records, has_previous) = optimize_log_records(&dir);
        assert!(has_previous);
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|r| r.source == "optimize"));
        let archived: Vec<&LogRecord> = records.iter().filter(|r| r.archived).collect();
        assert_eq!(archived.len(), 1);
        assert!(archived[0].line.contains("earlier run line"));
        let current: Vec<&LogRecord> = records.iter().filter(|r| !r.archived).collect();
        assert_eq!(current.len(), 1);
        assert!(current[0].line.contains("consultation start"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mcp_signup_url_prefers_stored_then_curated_seed() {
        let cfg = test_cfg();
        let brave = cfg
            .mcp
            .servers
            .iter()
            .find(|s| s.id == "brave_search")
            .expect("default config seeds brave_search");
        // The seeded entry carries the curated URL itself.
        assert_eq!(
            mcp_signup_url(brave).as_deref(),
            Some(manager_config::BRAVE_SEARCH_SIGNUP_URL)
        );
        // A config seeded before the field existed (no stored URL)
        // still resolves the curated seed's URL by id — the
        // mcp_start_preflight prompt depends on this for installs
        // that predate the field.
        let mut legacy = brave.clone();
        legacy.signup_url = None;
        assert_eq!(
            mcp_signup_url(&legacy).as_deref(),
            Some(manager_config::BRAVE_SEARCH_SIGNUP_URL)
        );
        // A stored URL wins over the curated seed's.
        let mut custom = brave.clone();
        custom.signup_url = Some("https://example.com/keys".to_string());
        assert_eq!(
            mcp_signup_url(&custom).as_deref(),
            Some("https://example.com/keys")
        );
        // A blank stored URL counts as unset and falls back.
        let mut blank = brave.clone();
        blank.signup_url = Some("  ".to_string());
        assert_eq!(
            mcp_signup_url(&blank).as_deref(),
            Some(manager_config::BRAVE_SEARCH_SIGNUP_URL)
        );
        // An unknown server with no stored URL has no link.
        let mut other = brave.clone();
        other.id = "not_a_curated_server".to_string();
        other.signup_url = None;
        assert_eq!(mcp_signup_url(&other), None);
    }

    #[test]
    fn mcp_credential_hint_prefers_stored_then_seed() {
        let cfg = test_cfg();
        let find = |id: &str| {
            cfg.mcp
                .servers
                .iter()
                .find(|s| s.id == id)
                .unwrap_or_else(|| panic!("default config seeds {id}"))
                .clone()
        };
        // The seeded entries carry their hints themselves — the
        // legacy pair's as well as the curated pair's (the signup
        // fallback only covers the curated pair; the hint fallback
        // covers the whole seeded registry).
        assert_eq!(
            mcp_credential_hint(&find("github")).as_deref(),
            Some("GitHub personal access token — created under GitHub Settings, Developer settings, Personal access tokens")
        );
        assert_eq!(
            mcp_credential_hint(&find("brave_search")).as_deref(),
            Some("Brave Search API key — the subscription key from the Brave Search API page")
        );
        // A config persisted before the field existed (no stored
        // hint) still resolves the seed's hint by id — existing
        // installs' credential fields stay labelled.
        let mut legacy = find("gdrive");
        legacy.credential_hint = None;
        assert_eq!(
            mcp_credential_hint(&legacy).as_deref(),
            Some("Path to the Google OAuth credentials JSON file — downloaded from Google Cloud Console, APIs & Services, Credentials; store the file path, not the file contents")
        );
        // A stored hint wins over the seed's, and a blank stored
        // hint counts as unset and falls back.
        let mut custom = find("github");
        custom.credential_hint = Some("Example token".to_string());
        assert_eq!(
            mcp_credential_hint(&custom).as_deref(),
            Some("Example token")
        );
        custom.credential_hint = Some("  ".to_string());
        assert_eq!(
            mcp_credential_hint(&custom).as_deref(),
            Some("GitHub personal access token — created under GitHub Settings, Developer settings, Personal access tokens")
        );
        // A server with no credential (web_fetch) and an unknown
        // id both resolve to no hint.
        assert_eq!(mcp_credential_hint(&find("web_fetch")), None);
        let mut other = find("github");
        other.id = "not_a_seeded_server".to_string();
        other.credential_hint = None;
        assert_eq!(mcp_credential_hint(&other), None);
    }

    #[test]
    fn evaluate_backend_offload_places_and_overrides() {
        let mut cfg = test_cfg();
        let b = serve_model(
            &mut cfg,
            "coder",
            "qwen3-coder-30b",
            "models/Qwen3-Coder-Next-UD-Q4_K_M.gguf",
            Some("--n-cpu-moe 46"),
        );
        // The coder backend is unsplit in the test config, so it
        // places on GPU 0; make GPU 0 a Pascal card.
        let mut gpus = test_gpus();
        gpus[0].compute_capability = (6, 0);
        let journal = activation::ActivationJournal::default();
        let eval = evaluate_backend_offload(&cfg, &b, &gpus, &journal)
            .expect("coder backend evaluates");
        assert_eq!(eval.layers, 46);
        assert_eq!(eval.moe_layers, 48);
        assert_eq!(eval.status, manager_models::OffloadStatus::Blocked);
        assert_eq!(eval.restricting_cc, Some((6, 0)));
        assert_eq!(eval.nearest_verified_layers, 48);
        assert!(!eval.has_override);
        assert!(!eval.overridden);

        // A journaled override for this backend, position, and
        // placed-CC set flips overridden (and only that).
        let journal = activation::ActivationJournal {
            offload_overrides: vec![activation::OffloadOverride {
                backend_id: "coder".to_string(),
                layers: 46,
                placed_ccs: vec![(6, 0)],
                ts: "2026-10-17T00:00:00Z".to_string(),
            }],
            ..Default::default()
        };
        let eval = evaluate_backend_offload(&cfg, &b, &gpus, &journal)
            .expect("coder backend evaluates");
        assert!(eval.has_override);
        assert!(eval.overridden);

        // Same position on an unrestricted GPU: no restriction
        // matches, nothing is blocked.
        let gpus = test_gpus();
        let eval = evaluate_backend_offload(&cfg, &b, &gpus, &journal)
            .expect("coder backend evaluates");
        assert_eq!(eval.restricting_cc, None);
        assert_ne!(eval.status, manager_models::OffloadStatus::Blocked);
    }

    #[test]
    fn validate_ctx_batch_matches_update_flags_rules() {
        assert!(validate_ctx_batch(32768, 512).is_ok());
        assert!(validate_ctx_batch(1024, 1024).is_ok());
        assert!(validate_ctx_batch(1_048_576, 1).is_ok());
        // Below the context floor, above the ceiling, a zero
        // batch, and a batch past the context all refuse.
        assert!(validate_ctx_batch(1023, 512).is_err());
        assert!(validate_ctx_batch(1_048_577, 512).is_err());
        assert!(validate_ctx_batch(32768, 0).is_err());
        assert!(validate_ctx_batch(2048, 4096).is_err());
    }

    #[test]
    fn validate_parallel_slots_matches_set_parallel_rules() {
        assert!(validate_parallel_slots(1).is_ok());
        assert!(validate_parallel_slots(4).is_ok());
        assert!(validate_parallel_slots(1024).is_ok());
        // Zero and past the control's ceiling refuse, with the
        // range named in the message (the flags form renders the
        // same bounds).
        assert!(validate_parallel_slots(0).is_err());
        let err = validate_parallel_slots(1025).expect_err("past the ceiling refuses");
        assert!(
            err.to_string().contains("parallel slots must be between 1 and 1024"),
            "unexpected wording: {err}"
        );
    }

    #[test]
    fn backend_parallel_info_decodes_and_marks_custom_spellings() {
        let info = |raw: Option<&str>| {
            backend_parallel_info(&uncreated_backend(
                "b",
                "models/m.gguf",
                raw.map(str::to_string),
            ))
        };
        // Unset: no explicit count, and nothing to mark — the
        // field presents llama.cpp's default.
        let i = info(None);
        assert_eq!(i.slots, None);
        assert!(!i.custom);
        let i = info(Some("-ot exps=CPU"));
        assert_eq!(i.slots, None);
        assert!(!i.custom);
        // The canonical spelling the control itself writes.
        let i = info(Some("--parallel 2"));
        assert_eq!(i.slots, Some(2));
        assert!(!i.custom);
        let i = info(Some("--spec-type draft-mtp --parallel 1"));
        assert_eq!(i.slots, Some(1));
        assert!(!i.custom);
        // Non-canonical spellings of the same value: the -np and
        // = forms, and a canonical token in a non-canonical
        // position (the shared custom definition: the canonical
        // rewrite must reproduce the saved tokens exactly).
        let i = info(Some("-np 2"));
        assert_eq!(i.slots, Some(2));
        assert!(i.custom);
        let i = info(Some("--parallel=2"));
        assert_eq!(i.slots, Some(2));
        assert!(i.custom);
        let i = info(Some("--parallel 1 --spec-type draft-mtp"));
        assert_eq!(i.slots, Some(1));
        assert!(i.custom);
        // A malformed token decodes as unset but still marks: the
        // flags carry a parallel spelling the control does not
        // write.
        let i = info(Some("--parallel abc"));
        assert_eq!(i.slots, None);
        assert!(i.custom);
    }

    #[test]
    fn split_for_gpu_selection_composes_even_splits() {
        let (split, mode) = split_for_gpu_selection(&[0, 1], &[0, 1]).unwrap();
        assert_eq!(split, vec![0.5, 0.5]);
        assert_eq!(mode, manager_config::SplitMode::Layer);
        let (split, _) = split_for_gpu_selection(&[0, 1], &[1]).unwrap();
        assert_eq!(split, vec![0.0, 1.0]);
        let (split, mode) = split_for_gpu_selection(&[0], &[0]).unwrap();
        assert_eq!(split, vec![1.0]);
        assert_eq!(mode, manager_config::SplitMode::Layer);
        // No telemetry, an empty selection, and an unknown GPU
        // all refuse (placement_apply's validation, mirrored).
        assert!(split_for_gpu_selection(&[], &[0]).is_err());
        assert!(split_for_gpu_selection(&[0, 1], &[]).is_err());
        assert!(split_for_gpu_selection(&[0, 1], &[2]).is_err());
    }

    fn register_test_model(cfg: &mut AppConfig, model_id: &str, file: &str) {
        cfg.models.push(manager_config::ModelConfig {
            id: model_id.to_string(),
            gguf_path: PathBuf::from(file),
            quant: "Q4_K_M".to_string(),
            params_b: 30.0,
            sha256: "a".repeat(64),
            verified_at: None,
            verified_sha256: None,
            source_url: String::new(),
            assigned_backends: Vec::new(),
            enabled: true,
            display_name: None,
            moe_layers: None,
            default_offload_layers: None,
        });
    }

    fn uncreated_backend(
        id: &str,
        file: &str,
        raw_flags: Option<String>,
    ) -> manager_config::BackendConfig {
        manager_config::BackendConfig {
            id: id.to_string(),
            enabled: true,
            model_file: PathBuf::from(file),
            port: 8090,
            flags: manager_config::ServerFlags {
                n_ctx: 32768,
                n_batch: 512,
                tensor_split: Vec::new(),
                split_mode: manager_config::SplitMode::None,
            },
            raw_flags,
            flags_advanced: false,
            restart_policy: manager_config::RestartPolicy {
                max_retries: 5,
                backoff_base_secs: 2,
            },
            overrides_global: false,
            tool_call_probe: None,
            purpose: String::new(),
        }
    }

    #[test]
    fn resolve_new_backend_offload_applies_creation_rules() {
        let mut cfg = test_cfg();
        register_test_model(
            &mut cfg,
            "qwen3-coder-30b",
            "models/Qwen3-Coder-Next-UD-Q4_K_M.gguf",
        );
        // The backend creation would build: unsplit (places on GPU
        // 0), raw flags at the catalog default (full offload).
        let backend = uncreated_backend(
            "coder-2",
            "models/Qwen3-Coder-Next-UD-Q4_K_M.gguf",
            catalog_default_raw_flags("qwen3-coder-30b"),
        );
        // GPU 0 is a Pascal card: partial positions are blocked.
        let mut gpus = test_gpus();
        gpus[0].compute_capability = (6, 0);
        let journal = activation::ActivationJournal::default();

        // A blocked position without an override refuses, naming
        // the snap target.
        let err = resolve_new_backend_offload(&cfg, &backend, &gpus, &journal, 46, false)
            .err()
            .expect("blocked position refuses");
        assert!(
            format!("{err}").contains("outside the verified range"),
            "unexpected refusal: {err}"
        );

        // The same position with a confirmed override resolves and
        // asks for the override to be journaled.
        let resolved =
            resolve_new_backend_offload(&cfg, &backend, &gpus, &journal, 46, true).unwrap();
        assert_eq!(resolved.raw_flags.as_deref(), Some("--n-cpu-moe 46"));
        assert!(resolved.record_override);
        assert_eq!(resolved.placed_ccs, vec![(6, 0)]);

        // The catalog default position resolves to the catalog
        // default flags, with no override to record.
        let resolved =
            resolve_new_backend_offload(&cfg, &backend, &gpus, &journal, 48, false).unwrap();
        assert_eq!(
            resolved.raw_flags,
            catalog_default_raw_flags("qwen3-coder-30b")
        );
        assert!(!resolved.record_override);

        // No offload clears the tokens entirely.
        let resolved =
            resolve_new_backend_offload(&cfg, &backend, &gpus, &journal, 0, false).unwrap();
        assert_eq!(resolved.raw_flags, None);

        // Past the model's MoE layer count refuses.
        assert!(resolve_new_backend_offload(&cfg, &backend, &gpus, &journal, 49, false).is_err());

        // A model with no catalog offload pin is not
        // offload-capable: any chosen position refuses.
        let mut cfg = test_cfg();
        register_test_model(&mut cfg, "qwen3-27b-planner", "models/planner.gguf");
        let backend = uncreated_backend("planner-2", "models/planner.gguf", None);
        let err = resolve_new_backend_offload(&cfg, &backend, &gpus, &journal, 1, false)
            .err()
            .expect("dense model refuses offload");
        assert!(
            format!("{err}").contains("does not support expert offload"),
            "unexpected refusal: {err}"
        );
    }

    #[test]
    fn metrics_redirect_resolution() {
        // The field case: FastAPI's trailing-slash redirect.
        assert_eq!(
            metrics_redirect_path("/metrics", "/metrics/").as_deref(),
            Some("/metrics/")
        );
        // Absolute URL: keep only path+query, stay on the dialed origin.
        assert_eq!(
            metrics_redirect_path("/metrics", "http://127.0.0.1:4000/metrics/?x=1").as_deref(),
            Some("/metrics/?x=1")
        );
        assert_eq!(
            metrics_redirect_path("/metrics", "https://example.test").as_deref(),
            Some("/")
        );
        // Relative reference resolves against the current directory.
        assert_eq!(
            metrics_redirect_path("/a/b", "c").as_deref(),
            Some("/a/c")
        );
        assert_eq!(metrics_redirect_path("/metrics", "  "), None);
    }

    /// A realistic LiteLLM 1.105 exposition excerpt: the series
    /// names carry the prometheus_client `_total` suffix, the
    /// deployment counters carry the `api_base` label the generated
    /// config writes (`http://127.0.0.1:<port>/v1`), and the token
    /// counters carry `model` = `openai/<backend-id>` (label sets
    /// per litellm/types/integrations/prometheus.py in the 1.105.1
    /// wheel).
    fn exposition(success: u64, failure: u64, input: u64, output: u64) -> String {
        format!(
            "# HELP litellm_deployment_success_responses_total Total successful responses\n\
             # TYPE litellm_deployment_success_responses_total counter\n\
             litellm_deployment_success_responses_total{{api_base=\"http://127.0.0.1:8081/v1\",api_provider=\"openai\",litellm_model_name=\"openai/planner\",model_group=\"planner\",requested_model=\"planner\"}} {success}.0\n\
             litellm_deployment_failure_responses_total{{api_base=\"http://127.0.0.1:8081/v1\",api_provider=\"openai\",litellm_model_name=\"openai/planner\",model_group=\"planner\",requested_model=\"planner\"}} {failure}.0\n\
             litellm_input_tokens_metric_total{{model=\"openai/planner\",requested_model=\"planner\"}} {input}.0\n\
             litellm_output_tokens_metric_total{{model=\"openai/planner\",requested_model=\"planner\"}} {output}.0\n"
        )
    }

    /// Frame a body the way the gateway's streaming metrics app
    /// does: hex-sized chunks (small, uneven sizes here so framing
    /// boundaries land mid-line) and a terminating zero chunk.
    fn chunk_frame(body: &str) -> Vec<u8> {
        let bytes = body.as_bytes();
        let mut out = Vec::new();
        let mut sizes = [7usize, 64, 13, 256, 3].into_iter().cycle();
        let mut rest = bytes;
        while !rest.is_empty() {
            let n = sizes.next().unwrap().min(rest.len());
            out.extend_from_slice(format!("{n:x}\r\n").as_bytes());
            out.extend_from_slice(&rest[..n]);
            out.extend_from_slice(b"\r\n");
            rest = &rest[n..];
        }
        out.extend_from_slice(b"0\r\n\r\n");
        out
    }

    /// The raw response exactly as it arrives off the wire from
    /// LiteLLM 1.105's /metrics/ (captured from the real
    /// `make_metrics_asgi_app` under uvicorn): 200, a content type,
    /// `Transfer-Encoding: chunked`, no content length.
    fn chunked_response(body: &str) -> Vec<u8> {
        let mut raw = Vec::new();
        raw.extend_from_slice(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/plain; version=0.0.4; charset=utf-8\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        );
        raw.extend_from_slice(&chunk_frame(body));
        raw
    }

    #[test]
    fn metrics_response_decodes_chunked_exposition() {
        // #58: every real scrape arrives chunked. The decoder must
        // hand the poller the plain exposition, byte for byte.
        let body = exposition(41, 3, 12000, 3400);
        let resp = decode_metrics_response(&chunked_response(&body)).expect("chunked 200 decodes");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, body);
    }

    #[test]
    fn metrics_response_rejects_truncated_or_malformed_chunked() {
        let body = exposition(41, 3, 12000, 3400);
        let raw = chunked_response(&body);
        // Cut before the terminating zero chunk: the stream was
        // interrupted (read timeout mid-body) — a partial
        // exposition must fail the cycle, not parse as smaller
        // counters.
        assert!(decode_metrics_response(&raw[..raw.len() - 10]).is_none());
        // A non-hex chunk size is malformation, not a body.
        let mut bad = Vec::new();
        bad.extend_from_slice(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
        bad.extend_from_slice(b"zz\r\nhello\r\n0\r\n\r\n");
        assert!(decode_metrics_response(&bad).is_none());
        // A chunk claiming more bytes than the stream carries.
        let mut short = Vec::new();
        short.extend_from_slice(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
        short.extend_from_slice(b"ff\r\ntiny\r\n0\r\n\r\n");
        assert!(decode_metrics_response(&short).is_none());
    }

    #[test]
    fn metrics_response_content_length_must_be_complete() {
        let body = exposition(1, 0, 10, 5);
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let resp = decode_metrics_response(raw.as_bytes()).expect("complete body decodes");
        assert_eq!(resp.body, body);
        // One byte short of the declared length: truncated, skip.
        assert!(decode_metrics_response(&raw.as_bytes()[..raw.len() - 1]).is_none());
    }

    #[test]
    fn metrics_response_redirect_keeps_location() {
        let raw = b"HTTP/1.1 307 Temporary Redirect\r\nlocation: /metrics/\r\ncontent-length: 0\r\n\r\n";
        let resp = decode_metrics_response(raw).expect("redirect decodes");
        assert_eq!(resp.status, 307);
        assert_eq!(resp.location.as_deref(), Some("/metrics/"));
    }

    #[test]
    fn chunked_scrapes_populate_the_window() {
        // The whole #58 chain at unit level: two chunked scrapes,
        // decoded, parsed, and recorded, produce the trailing
        // window the dashboard reads — requests_1h is defined.
        use crate::gateway_stats::{parse_metrics, GatewayStatsStore};
        let t0 = std::time::Instant::now();
        let mut store = GatewayStatsStore::default();
        for (at, body) in [
            (t0, exposition(41, 3, 12000, 3400)),
            (
                t0 + std::time::Duration::from_secs(60),
                exposition(55, 5, 18000, 5100),
            ),
        ] {
            let resp = decode_metrics_response(&chunked_response(&body)).expect("scrape decodes");
            store.record(parse_metrics(&resp.body, at));
        }
        let windows = store
            .window_stats(&[("planner".to_string(), 8081)])
            .expect("two snapshots make a window");
        let w = windows["planner"];
        assert_eq!(w.requests, 16); // +14 successes, +2 failures
        assert_eq!(w.errors, 2);
        assert_eq!(w.tokens, 7700); // (18000+5100) - (12000+3400)
    }

    fn catalog_restrictions(model_id: &str) -> Vec<manager_models::OffloadRestriction> {
        manager_models::curated_models()
            .into_iter()
            .find(|s| s.id == model_id)
            .unwrap()
            .offload_restrictions
    }

    fn backend(cfg: &AppConfig, id: &str) -> manager_config::BackendConfig {
        cfg.backends.iter().find(|b| b.id == id).unwrap().clone()
    }

    #[test]
    fn backend_spec_uses_wizard_binary_and_resolves_model() {
        let cfg = test_cfg();
        let b = backend(&cfg, "planner");
        let spec = backend_spec(&cfg, &b, &[]);
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
            let spec = backend_spec(&cfg, &b, &[]);
            let pos = spec.args.iter().position(|a| a == "--split-mode").unwrap();
            assert_eq!(spec.args[pos + 1], want);
        }
    }

    #[test]
    fn backend_spec_tensor_split_joined_with_commas() {
        let cfg = test_cfg();
        let mut b = backend(&cfg, "planner");
        b.flags.tensor_split = vec![0.5, 0.3, 0.2];
        let spec = backend_spec(&cfg, &b, &[]);
        let pos = spec
            .args
            .iter()
            .position(|a| a == "--tensor-split")
            .unwrap();
        assert_eq!(spec.args[pos + 1], "0.5,0.3,0.2");
    }

    /// Three fake inventory GPUs with stable UUIDs.
    fn test_gpus() -> Vec<GpuInfo> {
        (0..3)
            .map(|i| GpuInfo {
                index: i,
                name: format!("GPU {i}"),
                uuid: format!("GPU-uuid-{i}"),
                compute_capability: (8, 6),
                total_vram_mib: 16384,
                pci_bus_id: format!("00000000:0{}:00.0", i + 1),
                driver_version: "581.57".into(),
                driver_model: None,
                source: manager_telemetry::TelemetrySourceKind::NvidiaSmi,
            })
            .collect()
    }

    fn cuda_visible_devices(spec: &ProcessSpec) -> Option<&str> {
        spec.env
            .iter()
            .find(|(k, _)| k == "CUDA_VISIBLE_DEVICES")
            .map(|(_, v)| v.as_str())
    }

    fn tensor_split_arg(spec: &ProcessSpec) -> Option<&str> {
        spec.args
            .iter()
            .position(|a| a == "--tensor-split")
            .map(|pos| spec.args[pos + 1].as_str())
    }

    #[test]
    fn backend_spec_unsplit_restricts_to_gpu0() {
        let cfg = test_cfg();
        let b = backend(&cfg, "planner");
        assert!(b.flags.tensor_split.is_empty());
        let spec = backend_spec(&cfg, &b, &test_gpus());
        assert_eq!(cuda_visible_devices(&spec), Some("GPU-uuid-0"));
        assert_eq!(tensor_split_arg(&spec), None);
    }

    #[test]
    fn backend_spec_single_gpu_on_second_gpu() {
        // Coder's placement: the whole model on GPU 1. The visible
        // set is GPU 1 only (by UUID), and the remapped tensor split
        // is just its proportion — the zero entry for GPU 0 is gone,
        // so llama.cpp never creates its wasted context there.
        let cfg = test_cfg();
        let mut b = backend(&cfg, "planner");
        b.flags.tensor_split = vec![0.0, 1.0];
        let spec = backend_spec(&cfg, &b, &test_gpus());
        assert_eq!(cuda_visible_devices(&spec), Some("GPU-uuid-1"));
        assert_eq!(tensor_split_arg(&spec), Some("1"));
    }

    #[test]
    fn backend_spec_split_across_subset_of_gpus() {
        let cfg = test_cfg();
        let mut b = backend(&cfg, "planner");
        b.flags.tensor_split = vec![0.5, 0.0, 0.5];
        let spec = backend_spec(&cfg, &b, &test_gpus());
        assert_eq!(cuda_visible_devices(&spec), Some("GPU-uuid-0,GPU-uuid-2"));
        assert_eq!(tensor_split_arg(&spec), Some("0.5,0.5"));
    }

    #[test]
    fn backend_spec_cpu_only_gets_empty_visible_devices() {
        let cfg = test_cfg();
        let mut b = backend(&cfg, "planner");
        b.flags.tensor_split = vec![0.5, 0.5];
        b.raw_flags = Some("-ngl 0 --threads 4".into());
        let spec = backend_spec(&cfg, &b, &test_gpus());
        assert_eq!(cuda_visible_devices(&spec), Some(""));
        assert_eq!(
            tensor_split_arg(&spec),
            None,
            "a split over no visible devices is dropped"
        );
        assert!(spec.args.iter().any(|a| a == "--threads"));
    }

    #[test]
    fn backend_spec_cpu_only_via_gpu_layers_equals() {
        let cfg = test_cfg();
        let mut b = backend(&cfg, "planner");
        b.raw_flags = Some("--n-gpu-layers=0".into());
        let spec = backend_spec(&cfg, &b, &test_gpus());
        assert_eq!(cuda_visible_devices(&spec), Some(""));
    }

    #[test]
    fn backend_spec_expert_offload_is_not_cpu_only() {
        // Coder's `-ot exps=CPU` offloads experts only; the rest of
        // the model still loads on the GPU, so the launch restricts
        // to the placement GPU as usual.
        let cfg = test_cfg();
        let mut b = backend(&cfg, "planner");
        b.flags.tensor_split = vec![0.0, 1.0];
        b.raw_flags = Some("-ot exps=CPU".into());
        let spec = backend_spec(&cfg, &b, &test_gpus());
        assert_eq!(cuda_visible_devices(&spec), Some("GPU-uuid-1"));
        assert!(spec.args.iter().any(|a| a == "exps=CPU"));
    }

    #[test]
    fn backend_spec_missing_inventory_gpu_leaves_launch_unrestricted() {
        // GPU 1 fell off the bus before the inventory was taken: its
        // UUID is unknown, so no restriction is guessed at and the
        // configured split passes through verbatim.
        let cfg = test_cfg();
        let mut b = backend(&cfg, "planner");
        b.flags.tensor_split = vec![0.0, 1.0];
        let gpus = vec![test_gpus().remove(0)];
        let spec = backend_spec(&cfg, &b, &gpus);
        assert_eq!(cuda_visible_devices(&spec), None);
        assert_eq!(tensor_split_arg(&spec), Some("0,1"));
    }

    #[test]
    fn backend_spec_undetermined_split_leaves_launch_unrestricted() {
        let cfg = test_cfg();
        let mut b = backend(&cfg, "planner");
        b.flags.tensor_split = vec![0.0, 0.0];
        let spec = backend_spec(&cfg, &b, &test_gpus());
        assert_eq!(cuda_visible_devices(&spec), None);
        assert_eq!(tensor_split_arg(&spec), Some("0,0"));
    }

    #[test]
    fn backend_spec_unsplit_raw_device_selection_is_not_remapped_to_gpu0() {
        // Code-review 4.4: an unsplit backend whose raw flags carry
        // the placement must launch unrestricted with the flags
        // verbatim — not restricted to GPU 0 with the selection
        // dropped.
        let cfg = test_cfg();
        let mut b = backend(&cfg, "planner");
        assert!(b.flags.tensor_split.is_empty());
        b.raw_flags = Some("--main-gpu 1".into());
        let spec = backend_spec(&cfg, &b, &test_gpus());
        assert_eq!(cuda_visible_devices(&spec), None);
        let pos = spec.args.iter().position(|a| a == "--main-gpu").unwrap();
        assert_eq!(spec.args[pos + 1], "1");

        b.raw_flags = Some("--device CUDA1,CUDA2".into());
        let spec = backend_spec(&cfg, &b, &test_gpus());
        assert_eq!(cuda_visible_devices(&spec), None);
        assert!(spec.args.iter().any(|a| a == "CUDA1,CUDA2"));

        b.raw_flags = Some("--tensor-split 0,1".into());
        let spec = backend_spec(&cfg, &b, &test_gpus());
        assert_eq!(cuda_visible_devices(&spec), None);

        // A raw selection of exactly GPU 0 agrees with the unsplit
        // default: the launch restricts as usual.
        b.raw_flags = Some("--main-gpu 0".into());
        let spec = backend_spec(&cfg, &b, &test_gpus());
        assert_eq!(cuda_visible_devices(&spec), Some("GPU-uuid-0"));
    }

    #[test]
    fn raw_flags_pin_cpu_only_is_last_wins() {
        // Code-review 4.7: llama.cpp applies the last occurrence.
        assert!(raw_flags_pin_cpu_only(Some("-ngl 99 -ngl 0")));
        assert!(!raw_flags_pin_cpu_only(Some("-ngl 0 -ngl 99")));
        assert!(raw_flags_pin_cpu_only(Some("-ngl 99 --gpu-layers=0")));
        assert!(!raw_flags_pin_cpu_only(Some("--gpu-layers 0 -ngl 12")));
        assert!(raw_flags_pin_cpu_only(Some("--n-gpu-layers=0")));
        assert!(!raw_flags_pin_cpu_only(Some("-ot exps=CPU")));
        assert!(!raw_flags_pin_cpu_only(None));
    }

    #[test]
    fn backend_spec_remaps_raw_main_gpu_and_drops_conflicts() {
        let cfg = test_cfg();
        let mut b = backend(&cfg, "planner");
        b.flags.tensor_split = vec![0.5, 0.0, 0.5];
        // GPU 2 is placed at visible position 1.
        b.raw_flags = Some("--main-gpu 2".into());
        let spec = backend_spec(&cfg, &b, &test_gpus());
        let pos = spec.args.iter().position(|a| a == "--main-gpu").unwrap();
        assert_eq!(spec.args[pos + 1], "1");
        // GPU 1 is not placed: the flag naming it is dropped.
        b.raw_flags = Some("--main-gpu=1".into());
        let spec = backend_spec(&cfg, &b, &test_gpus());
        assert!(!spec.args.iter().any(|a| a.starts_with("--main-gpu")));
    }

    #[test]
    fn backend_spec_raw_tensor_split_yields_to_placement() {
        let cfg = test_cfg();
        let mut b = backend(&cfg, "planner");
        b.flags.tensor_split = vec![0.0, 1.0];
        b.raw_flags = Some("--tensor-split 0.5,0.5 --threads 4".into());
        let spec = backend_spec(&cfg, &b, &test_gpus());
        assert_eq!(
            spec.args.iter().filter(|a| *a == "--tensor-split").count(),
            1,
            "only the placement's remapped split survives"
        );
        assert_eq!(tensor_split_arg(&spec), Some("1"));
        assert!(spec.args.iter().any(|a| a == "--threads"));
    }

    #[test]
    fn backend_spec_remaps_raw_device_list() {
        let cfg = test_cfg();
        let mut b = backend(&cfg, "planner");
        b.flags.tensor_split = vec![0.5, 0.0, 0.5];
        b.raw_flags = Some("--device CUDA0,CUDA2".into());
        let spec = backend_spec(&cfg, &b, &test_gpus());
        let pos = spec.args.iter().position(|a| a == "--device").unwrap();
        assert_eq!(spec.args[pos + 1], "CUDA0,CUDA1");
        // A device outside the placement leaves nothing behind.
        b.raw_flags = Some("--device CUDA1".into());
        let spec = backend_spec(&cfg, &b, &test_gpus());
        assert!(!spec.args.iter().any(|a| a == "--device"));
    }

    #[test]
    fn backend_spec_filters_host_from_raw_flags() {
        let cfg = test_cfg();
        let mut b = backend(&cfg, "planner");
        b.raw_flags = Some("--host 0.0.0.0 --threads 4".into());
        let spec = backend_spec(&cfg, &b, &[]);
        assert!(!spec.args.iter().any(|a| a == "--host" || a == "0.0.0.0"));
        assert!(spec.args.iter().any(|a| a == "--threads"));
        b.raw_flags = Some("--host=0.0.0.0 --threads 4".into());
        let spec = backend_spec(&cfg, &b, &[]);
        assert!(!spec.args.iter().any(|a| a.starts_with("--host")));
        assert!(spec.args.iter().any(|a| a == "--threads"));
    }

    /// Register a catalog model in a test config and point a
    /// backend at its file, so model resolution finds it.
    fn serve_model(
        cfg: &mut AppConfig,
        backend_id: &str,
        model_id: &str,
        file: &str,
        raw_flags: Option<&str>,
    ) -> manager_config::BackendConfig {
        cfg.models.push(manager_config::ModelConfig {
            id: model_id.to_string(),
            gguf_path: PathBuf::from(file),
            quant: "Q4_K_M".to_string(),
            params_b: 30.0,
            sha256: "a".repeat(64),
            verified_at: None,
            verified_sha256: None,
            source_url: String::new(),
            assigned_backends: vec![backend_id.to_string()],
            enabled: true,
            display_name: None,
            moe_layers: None,
            default_offload_layers: None,
        });
        let b = cfg
            .backends
            .iter_mut()
            .find(|b| b.id == backend_id)
            .unwrap();
        b.model_file = PathBuf::from(file);
        b.raw_flags = raw_flags.map(str::to_string);
        b.clone()
    }

    #[test]
    fn catalog_default_raw_flags_render_the_design_points() {
        assert_eq!(
            catalog_default_raw_flags("qwen3-coder-30b").as_deref(),
            Some("-ot exps=CPU")
        );
        assert_eq!(
            catalog_default_raw_flags("nemotron-3.5-lightning-30b").as_deref(),
            Some("--n-cpu-moe 16")
        );
        assert_eq!(catalog_default_raw_flags("qwen3-27b-planner"), None);
        assert_eq!(catalog_default_raw_flags("not-a-model"), None);
        // The seeded default fleet agrees with the catalog: the
        // coder backend's built-in raw flags are the catalog
        // default for its model.
        let cfg = test_cfg();
        let coder = backend(&cfg, "coder");
        assert_eq!(
            coder.raw_flags.as_deref(),
            catalog_default_raw_flags("qwen3-coder-30b").as_deref()
        );
    }

    #[test]
    fn offload_info_for_nemotron_backend_uses_catalog_pins() {
        let mut cfg = test_cfg();
        // No file on disk in the test config: the payload resolves
        // from the catalog pins (Catalog basis arithmetic).
        let b = serve_model(
            &mut cfg,
            "coder-fast",
            "nemotron-3.5-lightning-30b",
            "models/Nemotron.gguf",
            Some("--n-cpu-moe 16"),
        );
        let info = backend_offload_info(&cfg, &b).expect("nemotron is offload-capable");
        assert_eq!(info.moe_layers, 24);
        assert_eq!(info.layers, 16);
        assert_eq!(info.green_limit_layers, 16);
        assert_eq!(info.expert_bytes, 22_829_334_528);
        assert_eq!(info.total_bytes, 25_266_255_936);
        assert!((info.green_end - 16.0 / 24.0).abs() < 1e-9);
        assert!(info.yellow_end > info.green_end && info.yellow_end < 1.0);
        // The requirement under the same flags prices the partial
        // offload from the catalog pins: 16 of 24 MoE layers of
        // experts to system RAM (14,515 MiB), the rest of the
        // catalog's file size on the GPU (9,582 MiB).
        let req = backend_memory_requirement(&cfg, &b).unwrap();
        assert_eq!(req.sys_weights_mib, 14_515);
        assert_eq!(req.gpu_weights_mib, 9_582);
        assert_eq!(req.basis, manager_models::MemoryBasis::Catalog);
    }

    #[test]
    fn offload_info_for_coder_backend_is_all_layers() {
        let mut cfg = test_cfg();
        let b = serve_model(
            &mut cfg,
            "coder",
            "qwen3-coder-30b",
            "models/Qwen3-Coder-Next-UD-Q4_K_M.gguf",
            Some("-ot exps=CPU"),
        );
        let info = backend_offload_info(&cfg, &b).expect("coder is offload-capable");
        assert_eq!(info.moe_layers, 48);
        assert_eq!(info.layers, 48);
        assert_eq!(info.green_end, 1.0);
        assert_eq!(info.yellow_end, 1.0);
    }

    #[test]
    fn offload_info_absent_for_dense_model() {
        let mut cfg = test_cfg();
        let b = serve_model(
            &mut cfg,
            "planner",
            "qwen3-27b-planner",
            "models/Qwen3.8-27B.gguf",
            None,
        );
        assert!(backend_offload_info(&cfg, &b).is_none());
    }

    /// A minimal GGUF header (directory only — the layout parser
    /// reads nothing past it) with `moe_blocks` blocks carrying one
    /// routed-expert tensor each, plus `dense_blocks` plain blocks.
    fn gguf_header_bytes(moe_blocks: u32, dense_blocks: u32) -> Vec<u8> {
        fn w_str(out: &mut Vec<u8>, s: &str) {
            out.extend_from_slice(&(s.len() as u64).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        fn w_kv_str(out: &mut Vec<u8>, key: &str, v: &str) {
            w_str(out, key);
            out.extend_from_slice(&8u32.to_le_bytes());
            w_str(out, v);
        }
        fn w_kv_u32(out: &mut Vec<u8>, key: &str, v: u32) {
            w_str(out, key);
            out.extend_from_slice(&4u32.to_le_bytes());
            out.extend_from_slice(&v.to_le_bytes());
        }
        fn w_tensor(out: &mut Vec<u8>, name: &str, elems: u64) {
            w_str(out, name);
            out.extend_from_slice(&1u32.to_le_bytes()); // n_dims
            out.extend_from_slice(&elems.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes()); // F32
            out.extend_from_slice(&0u64.to_le_bytes()); // offset
        }
        let total = moe_blocks + dense_blocks;
        let mut b = Vec::new();
        b.extend_from_slice(b"GGUF");
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&(total as u64).to_le_bytes()); // tensor count
        b.extend_from_slice(&2u64.to_le_bytes()); // kv count
        w_kv_str(&mut b, "general.architecture", "testarch");
        w_kv_u32(&mut b, "testarch.block_count", total);
        for i in 0..moe_blocks {
            w_tensor(&mut b, &format!("blk.{i}.ffn_gate_exps.weight"), 256);
        }
        for i in 0..dense_blocks {
            w_tensor(
                &mut b,
                &format!("blk.{}.ffn_gate.weight", moe_blocks + i),
                256,
            );
        }
        b
    }

    #[test]
    fn model_default_raw_flags_render_declared_defaults() {
        let mut cfg = test_cfg();
        register_test_model(&mut cfg, "user-moe-27b", "models/user-moe.gguf");
        let mut model = cfg.models.last().unwrap().clone();
        // No declaration: no default, exactly like a dense model.
        assert_eq!(model_default_raw_flags(&model), None);
        model.moe_layers = Some(48);
        model.default_offload_layers = Some(48);
        assert_eq!(
            model_default_raw_flags(&model).as_deref(),
            Some("-ot exps=CPU")
        );
        model.default_offload_layers = Some(16);
        assert_eq!(
            model_default_raw_flags(&model).as_deref(),
            Some("--n-cpu-moe 16")
        );
        // A declared default of 0 writes no tokens.
        model.default_offload_layers = Some(0);
        assert_eq!(model_default_raw_flags(&model), None);
        // Layers without a default cannot form a position.
        model.default_offload_layers = None;
        assert_eq!(model_default_raw_flags(&model), None);
    }

    #[test]
    fn catalog_pin_wins_over_a_record_declaration() {
        let mut cfg = test_cfg();
        // A Nemotron record carrying a conflicting declaration
        // (full offload): the catalog's 16-of-24 pin still wins,
        // for the creation default and the slider's green limit.
        let b = serve_model(
            &mut cfg,
            "coder-fast",
            "nemotron-3.5-lightning-30b",
            "models/Nemotron.gguf",
            None,
        );
        let m = cfg
            .models
            .iter_mut()
            .find(|m| m.id == "nemotron-3.5-lightning-30b")
            .unwrap();
        m.moe_layers = Some(24);
        m.default_offload_layers = Some(24);
        let model = cfg
            .models
            .iter()
            .find(|m| m.id == "nemotron-3.5-lightning-30b")
            .unwrap()
            .clone();
        assert_eq!(
            model_default_raw_flags(&model).as_deref(),
            Some("--n-cpu-moe 16")
        );
        let info = backend_offload_info(&cfg, &b).expect("nemotron is offload-capable");
        assert_eq!(info.green_limit_layers, 16);
    }

    #[test]
    fn declared_moe_model_gets_offload_info_once_its_file_parses() {
        // The slider's figures split by source: the layer count
        // and expert bytes come from the downloaded GGUF's parse;
        // the green limit — the one input that used to be
        // catalog-only — comes from the record's declaration.
        let dir = std::env::temp_dir().join(format!(
            "llm-mgr-declared-moe-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("models")).unwrap();
        std::fs::write(
            dir.join("models").join("user-moe.gguf"),
            gguf_header_bytes(48, 0),
        )
        .unwrap();
        let mut cfg = test_cfg();
        cfg.data_dir = dir.clone();
        register_test_model(&mut cfg, "user-moe-27b", "models/user-moe.gguf");
        cfg.models.last_mut().unwrap().moe_layers = Some(48);
        cfg.models.last_mut().unwrap().default_offload_layers = Some(16);
        let b = uncreated_backend(
            "moe-backend",
            "models/user-moe.gguf",
            Some("--n-cpu-moe 16".to_string()),
        );
        let info = backend_offload_info(&cfg, &b).expect("declared MoE model is offload-capable");
        assert_eq!(info.moe_layers, 48);
        assert_eq!(info.green_limit_layers, 16);
        assert_eq!(info.layers, 16);
        assert!((info.green_end - 16.0 / 48.0).abs() < 1e-9);
        assert!(info.restrictions.is_empty());
        // Without the declaration the same parse yields no slider:
        // the green limit has no source.
        cfg.models.last_mut().unwrap().moe_layers = None;
        cfg.models.last_mut().unwrap().default_offload_layers = None;
        assert!(backend_offload_info(&cfg, &b).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn user_declared_moe_model_round_trip() {
        // Pete's shape: a user-added 27B-class MoE model the
        // catalog does not carry, declared at add time.
        let (state, dir) = advanced_test_state(test_cfg());
        let sink = test_sink();
        let r = op_model_add(
            &state,
            &sink,
            "qwen3.8-27b-moe".to_string(),
            "https://example.com/Qwen3.8-27B-MoE.gguf".to_string(),
            None,
            None,
            Some(27.0),
            Some(48),
            Some(16),
        )
        .await
        .unwrap();
        assert_eq!(r["model_id"], "qwen3.8-27b-moe");
        let saved = manager_config::load(&state.config_path).unwrap();
        let m = saved
            .models
            .iter()
            .find(|m| m.id == "qwen3.8-27b-moe")
            .unwrap();
        assert_eq!(m.moe_layers, Some(48));
        assert_eq!(m.default_offload_layers, Some(16));
        // The downloaded file lands: a 48-block MoE GGUF.
        std::fs::create_dir_all(dir.join("models")).unwrap();
        std::fs::write(
            dir.join("models").join("Qwen3.8-27B-MoE.gguf"),
            gguf_header_bytes(48, 0),
        )
        .unwrap();
        // Backend creation applies the declared default.
        let r = op_backend_add(
            &state,
            &sink,
            "moe-backend".to_string(),
            "qwen3.8-27b-moe".to_string(),
            8199,
            "moe".to_string(),
            true,
            true,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(r["backend_id"], "moe-backend");
        let saved = manager_config::load(&state.config_path).unwrap();
        let b = saved
            .backends
            .iter()
            .find(|b| b.id == "moe-backend")
            .unwrap()
            .clone();
        assert_eq!(b.raw_flags.as_deref(), Some("--n-cpu-moe 16"));
        // The add form's preview and the Edit Flags payload agree:
        // the control exists, ranged over the parsed layer count,
        // greened at the declared default.
        let preview = op_backend_add_preview(
            &state,
            &sink,
            "qwen3.8-27b-moe".to_string(),
            None,
        )
        .await
        .unwrap();
        let off = preview["offload"].as_object().expect("preview carries offload");
        assert_eq!(off["moe_layers"], 48);
        assert_eq!(off["green_limit_layers"], 16);
        assert_eq!(off["layers"], 16);
        let cfg_now = state.config.read().await.clone();
        let info = backend_offload_info(&cfg_now, &b).expect("offload info for the new backend");
        assert_eq!(info.moe_layers, 48);
        assert_eq!(info.green_limit_layers, 16);
        // The slider op rewrites only the offload tokens: partial,
        // full, and off — a sibling -ot assignment and an unrelated
        // flag survive every rewrite.
        op_backend_set_offload(&state, &sink, "moe-backend".to_string(), 24, None)
            .await
            .unwrap();
        let saved = manager_config::load(&state.config_path).unwrap();
        let flags = saved
            .backends
            .iter()
            .find(|b| b.id == "moe-backend")
            .unwrap()
            .raw_flags
            .clone()
            .unwrap();
        assert_eq!(flags, "--n-cpu-moe 24");
        // A sibling -ot assignment and an unrelated flag reach the
        // record the way the app allows: written as text in
        // advanced mode, carried verbatim when the backend returns
        // to structured mode.
        op_backend_set_flags_mode(&state, &sink, "moe-backend".to_string(), true)
            .await
            .unwrap();
        op_backend_set_raw_flags(
            &state,
            &sink,
            "moe-backend".to_string(),
            Some("--mlock -ot ffn_down_shexp=CPU --n-cpu-moe 24".to_string()),
        )
        .await
        .unwrap();
        op_backend_set_flags_mode(&state, &sink, "moe-backend".to_string(), false)
            .await
            .unwrap();
        op_backend_set_offload(&state, &sink, "moe-backend".to_string(), 48, None)
            .await
            .unwrap();
        let saved = manager_config::load(&state.config_path).unwrap();
        let flags = saved
            .backends
            .iter()
            .find(|b| b.id == "moe-backend")
            .unwrap()
            .raw_flags
            .clone()
            .unwrap();
        assert!(flags.contains("-ot exps=CPU"), "{flags}");
        assert!(flags.contains("-ot ffn_down_shexp=CPU"), "{flags}");
        assert!(flags.contains("--mlock"), "{flags}");
        assert!(!flags.contains("--n-cpu-moe"), "{flags}");
        op_backend_set_offload(&state, &sink, "moe-backend".to_string(), 0, None)
            .await
            .unwrap();
        let saved = manager_config::load(&state.config_path).unwrap();
        let flags = saved
            .backends
            .iter()
            .find(|b| b.id == "moe-backend")
            .unwrap()
            .raw_flags
            .clone()
            .unwrap();
        assert!(!flags.contains("--n-cpu-moe"), "{flags}");
        assert!(!flags.contains("-ot exps=CPU"), "{flags}");
        assert!(flags.contains("-ot ffn_down_shexp=CPU"), "{flags}");
        assert!(flags.contains("--mlock"), "{flags}");
        // Editing the declaration moves the default; clearing it
        // removes the control (the green limit loses its source).
        let r = op_model_set_offload_spec(
            &state,
            &sink,
            "qwen3.8-27b-moe".to_string(),
            Some(48),
            Some(48),
        )
        .await
        .unwrap();
        assert_eq!(r["default_offload_layers"], 48);
        let saved = manager_config::load(&state.config_path).unwrap();
        let m = saved
            .models
            .iter()
            .find(|m| m.id == "qwen3.8-27b-moe")
            .unwrap();
        assert_eq!(model_default_raw_flags(m).as_deref(), Some("-ot exps=CPU"));
        op_model_set_offload_spec(&state, &sink, "qwen3.8-27b-moe".to_string(), None, None)
            .await
            .unwrap();
        let saved = manager_config::load(&state.config_path).unwrap();
        let b = saved
            .backends
            .iter()
            .find(|b| b.id == "moe-backend")
            .unwrap()
            .clone();
        assert!(backend_offload_info(&saved, &b).is_none());
        // Invalid declarations refuse at both write paths and
        // persist nothing.
        assert!(matches!(
            op_model_set_offload_spec(
                &state,
                &sink,
                "qwen3.8-27b-moe".to_string(),
                Some(48),
                Some(49),
            )
            .await,
            Err(ApiError::InvalidInput(_))
        ));
        assert!(matches!(
            op_model_add(
                &state,
                &sink,
                "bad-moe".to_string(),
                "https://example.com/bad.gguf".to_string(),
                None,
                None,
                Some(9.0),
                Some(48),
                None,
            )
            .await,
            Err(ApiError::InvalidInput(_))
        ));
        let saved = manager_config::load(&state.config_path).unwrap();
        assert!(!saved.models.iter().any(|m| m.id == "bad-moe"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dense_user_model_gets_no_offload_control() {
        let (state, dir) = advanced_test_state(test_cfg());
        let sink = test_sink();
        op_model_add(
            &state,
            &sink,
            "dense-7b".to_string(),
            "https://example.com/dense-7b.gguf".to_string(),
            None,
            None,
            Some(7.0),
            None,
            None,
        )
        .await
        .unwrap();
        std::fs::create_dir_all(dir.join("models")).unwrap();
        std::fs::write(
            dir.join("models").join("dense-7b.gguf"),
            gguf_header_bytes(0, 4),
        )
        .unwrap();
        op_backend_add(
            &state,
            &sink,
            "dense-backend".to_string(),
            "dense-7b".to_string(),
            8198,
            "moe".to_string(),
            true,
            true,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let saved = manager_config::load(&state.config_path).unwrap();
        let b = saved
            .backends
            .iter()
            .find(|b| b.id == "dense-backend")
            .unwrap()
            .clone();
        assert_eq!(b.raw_flags, None);
        assert!(backend_offload_info(&saved, &b).is_none());
        let preview = op_backend_add_preview(&state, &sink, "dense-7b".to_string(), None)
            .await
            .unwrap();
        assert!(preview["offload"].is_null());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backend_set_offload_journals_a_backend_scope_entry() {
        // record_backend_change is the producer backend_set_offload
        // journals through: the entry lands in the activation
        // journal with backend scope and the backend as its target.
        let dir = std::env::temp_dir().join(format!(
            "llm-mgr-offload-journal-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let pre = test_cfg();
        let mut post = pre.clone();
        post.backends
            .iter_mut()
            .find(|b| b.id == "coder")
            .unwrap()
            .raw_flags = Some("--n-cpu-moe 24".to_string());
        // record_backend_change (the producer backend_set_offload
        // journals through) is a thin wrapper over
        // activation::record_change with backend scope; exercise
        // that directly — building an AppState for the wrapper is
        // all supervisor and locks, no journal behavior.
        let configured: std::collections::HashSet<String> =
            post.backends.iter().map(|b| b.id.clone()).collect();
        activation::record_change(
            &dir,
            &pre,
            &post,
            "backend_set_offload",
            "Backend `coder` expert offload set to 24 of 48 MoE layers".to_string(),
            "backend",
            vec!["coder".to_string()],
            &std::collections::HashMap::new(),
            &configured,
            &std::collections::HashSet::new(),
        )
        .unwrap();
        let journal = activation::load_journal(&dir);
        let entry = journal
            .entries
            .iter()
            .find(|e| e.kind == "backend_set_offload")
            .expect("journal carries the offload entry");
        assert_eq!(entry.scope, "backend");
        assert_eq!(entry.targets, vec!["coder".to_string()]);
        assert!(entry.summary.contains("24 of 48"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Stateful loopback stub for the offload-probe sequence
    /// tests: answers GET /health and POST /v1/chat/completions
    /// from per-path status scripts (each request pops the next
    /// scripted status; the last one repeats). Returns the base
    /// URL and the per-path request counters.
    fn offload_probe_stub(
        health_script: Vec<u16>,
        chat_script: Vec<u16>,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<usize>>,
        std::sync::Arc<std::sync::Mutex<usize>>,
    ) {
        use std::io::{Read, Write};
        fn next_status(
            script: &std::sync::Mutex<Vec<u16>>,
        ) -> u16 {
            let mut script = script.lock().unwrap();
            if script.len() > 1 {
                script.remove(0)
            } else {
                script[0]
            }
        }
        fn stub_body(path: &str, status: u16) -> String {
            match (path, status) {
                ("/health", 200) => "{\"status\":\"ok\"}".to_string(),
                ("/health", _) => "{\"status\":\"loading\"}".to_string(),
                (_, 200) => "{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"ok\"}}],\"usage\":{\"prompt_tokens\":4000,\"completion_tokens\":16}}".to_string(),
                (_, 503) => "{\"error\":{\"message\":\"Loading model\",\"type\":\"unavailable_error\",\"code\":503}}".to_string(),
                (_, s) => format!(
                    "{{\"error\":{{\"message\":\"stub failure\",\"type\":\"server_error\",\"code\":{s}}}}}"
                ),
            }
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let health_script = std::sync::Arc::new(std::sync::Mutex::new(health_script));
        let chat_script = std::sync::Arc::new(std::sync::Mutex::new(chat_script));
        let health_count = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let chat_count = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        {
            let health_script = std::sync::Arc::clone(&health_script);
            let chat_script = std::sync::Arc::clone(&chat_script);
            let health_count = std::sync::Arc::clone(&health_count);
            let chat_count = std::sync::Arc::clone(&chat_count);
            std::thread::spawn(move || {
                for _ in 0..500 {
                    let Ok((mut stream, _)) = listener.accept() else {
                        return;
                    };
                    // Read the request head, then the body per
                    // Content-Length, before responding.
                    let mut buf: Vec<u8> = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let mut content_length = 0usize;
                    let mut head_end: Option<usize> = None;
                    loop {
                        if head_end.is_none() {
                            head_end = buf
                                .windows(4)
                                .position(|w| w == b"\r\n\r\n")
                                .map(|p| p + 4);
                            if let Some(he) = head_end {
                                let head = String::from_utf8_lossy(&buf[..he]).into_owned();
                                for line in head.lines() {
                                    let lower = line.to_ascii_lowercase();
                                    if let Some(v) = lower.strip_prefix("content-length:") {
                                        content_length = v.trim().parse().unwrap_or(0);
                                    }
                                }
                            }
                        }
                        if let Some(he) = head_end {
                            if buf.len() >= he + content_length {
                                break;
                            }
                        }
                        match stream.read(&mut chunk) {
                            Ok(0) => break,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            Err(_) => break,
                        }
                    }
                    let req = String::from_utf8_lossy(&buf).into_owned();
                    let path = req
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("")
                        .to_string();
                    let status = if path == "/health" {
                        *health_count.lock().unwrap() += 1;
                        next_status(&health_script)
                    } else {
                        *chat_count.lock().unwrap() += 1;
                        next_status(&chat_script)
                    };
                    let body = stub_body(&path, status);
                    let reason = match status {
                        200 => "OK",
                        500 => "Internal Server Error",
                        503 => "Service Unavailable",
                        _ => "Status",
                    };
                    let resp = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes());
                }
            });
        }
        (
            format!("http://127.0.0.1:{port}"),
            health_count,
            chat_count,
        )
    }

    fn offload_probe_test_timing() -> OffloadProbeTiming {
        OffloadProbeTiming {
            ready_budget: Duration::from_secs(10),
            ready_poll: Duration::from_millis(10),
            retry_window: Duration::from_secs(10),
            retry_poll: Duration::from_millis(10),
        }
    }

    #[tokio::test]
    async fn offload_probe_waits_out_loading_503s() {
        // The tracker #46 field shape: /health reports 503 while
        // the model loads, then 200. The probe must not send its
        // chat request until readiness, and must then pass — the
        // old status-blind "ping" gate fired the probe request
        // into the loading 503 and reported a failure.
        let (base, health_count, chat_count) =
            offload_probe_stub(vec![503, 503, 200], vec![200]);
        let mut alive = || {
            Box::pin(async { true })
                as std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
        };
        let outcome = offload_probe_http_sequence(&base, offload_probe_test_timing(), &mut alive)
            .await;
        assert_eq!(outcome, OffloadProbeHttpOutcome::Passed);
        assert!(*health_count.lock().unwrap() >= 3);
        assert_eq!(*chat_count.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn offload_probe_reports_not_ready_without_sending_the_probe() {
        // The backend never becomes ready: the outcome is the
        // distinct NotReady, and the probe request is never sent —
        // a not-ready backend must not read as a probe failure.
        let (base, _health_count, chat_count) = offload_probe_stub(vec![503], vec![200]);
        let timing = OffloadProbeTiming {
            ready_budget: Duration::from_millis(300),
            ..offload_probe_test_timing()
        };
        let mut alive = || {
            Box::pin(async { true })
                as std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
        };
        let outcome = offload_probe_http_sequence(&base, timing, &mut alive).await;
        assert_eq!(outcome, OffloadProbeHttpOutcome::NotReady);
        assert_eq!(*chat_count.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn offload_probe_retries_transient_503_after_readiness() {
        // Ready, but the first probe attempts still draw 503s
        // (startup work racing the probe): retried within the
        // window, then the probe passes.
        let (base, _health_count, chat_count) =
            offload_probe_stub(vec![200], vec![503, 503, 200]);
        let mut alive = || {
            Box::pin(async { true })
                as std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
        };
        let outcome = offload_probe_http_sequence(&base, offload_probe_test_timing(), &mut alive)
            .await;
        assert_eq!(outcome, OffloadProbeHttpOutcome::Passed);
        assert_eq!(*chat_count.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn offload_probe_fails_on_error_status_after_readiness() {
        // A non-transient error status after readiness is a real
        // probe failure, reported immediately (no retry).
        let (base, _health_count, chat_count) = offload_probe_stub(vec![200], vec![500]);
        let mut alive = || {
            Box::pin(async { true })
                as std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
        };
        let outcome = offload_probe_http_sequence(&base, offload_probe_test_timing(), &mut alive)
            .await;
        assert_eq!(
            outcome,
            OffloadProbeHttpOutcome::FailedAfterReady("HTTP 500".to_string())
        );
        assert_eq!(*chat_count.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn offload_probe_persistent_503_after_readiness_is_terminal() {
        // A 503 that outlasts the retry window is reported as a
        // failure after readiness — transient handling must not
        // mask a backend that stays unavailable.
        let (base, _health_count, chat_count) = offload_probe_stub(vec![200], vec![503]);
        let timing = OffloadProbeTiming {
            retry_window: Duration::from_millis(300),
            ..offload_probe_test_timing()
        };
        let mut alive = || {
            Box::pin(async { true })
                as std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
        };
        let outcome = offload_probe_http_sequence(&base, timing, &mut alive).await;
        assert_eq!(
            outcome,
            OffloadProbeHttpOutcome::FailedAfterReady("HTTP 503".to_string())
        );
        assert!(*chat_count.lock().unwrap() >= 2);
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

        /// Loopback HTTP server for the metrics-fetcher tests: answers
        /// each accepted connection with the canned response mapped to
        /// the request's path (404 when unmapped), then closes.
        /// Returns the bound port.
        fn metrics_test_server(
            responses: std::collections::HashMap<String, String>,
            connections: usize,
        ) -> u16 {
            use std::io::{Read, Write};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                for _ in 0..connections {
                    let Ok((mut stream, _)) = listener.accept() else {
                        return;
                    };
                    let mut buf = [0u8; 4096];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req.split_whitespace().nth(1).unwrap_or("").to_string();
                    let resp = responses.get(&path).cloned().unwrap_or_else(|| {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_string()
                    });
                    let _ = stream.write_all(resp.as_bytes());
                }
            });
            port
        }

        #[test]
        fn metrics_fetch_follows_trailing_slash_redirect() {
            let body = "litellm_deployment_success_responses_total 3.0\n";
            let responses = std::collections::HashMap::from([
                (
                    "/metrics".to_string(),
                    "HTTP/1.1 307 Temporary Redirect\r\nLocation: /metrics/\r\nContent-Length: 0\r\n\r\n"
                        .to_string(),
                ),
                (
                    "/metrics/".to_string(),
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    ),
                ),
            ]);
            let port = metrics_test_server(responses, 2);
            let got =
                fetch_gateway_metrics("127.0.0.1", port, "test-key").expect("body after redirect");
            assert_eq!(got, body);
        }

        #[test]
        fn metrics_fetch_direct_200_and_error_status() {
            let responses = std::collections::HashMap::from([(
                "/metrics".to_string(),
                "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nbody".to_string(),
            )]);
            let port = metrics_test_server(responses, 1);
            assert_eq!(
                fetch_gateway_metrics("127.0.0.1", port, "k").as_deref(),
                Some("body")
            );
            // No path mapped -> the server's 404 -> None, no panic.
            let port = metrics_test_server(std::collections::HashMap::new(), 1);
            assert_eq!(fetch_gateway_metrics("127.0.0.1", port, "k"), None);
        }

        #[test]
        fn backend_spec_filters_no_jinja_from_raw_flags() {
            // --jinja is app-owned (MCP design §7.1): every raw form
            // that would disable it is stripped, in both spellings,
            // while unrelated raw flags pass through.
            let cfg = test_cfg();
            let mut b = backend(&cfg, "planner");
            for raw in [
                "--no-jinja --threads 4",
                "--no_jinja --threads 4",
                "--jinja=false --threads 4",
            ] {
                b.raw_flags = Some(raw.into());
                let spec = backend_spec(&cfg, &b, &[]);
                assert!(
                    !spec.args.iter().any(|a| a.starts_with("--no-jinja")
                        || a.starts_with("--no_jinja")
                        || a == "--jinja=false"),
                    "raw form {raw:?} must be filtered"
                );
                assert!(spec.args.iter().any(|a| a == "--threads"));
                // The structured --jinja survives exactly once.
                assert_eq!(
                    spec.args.iter().filter(|a| *a == "--jinja").count(),
                    1
                );
            }
        }

        #[test]
        fn offload_block_reason_partial_on_p100_is_blocked() {
            // Tracker #37: the field failure — Coder-Next at the
            // position `--n-cpu-moe 46` spells, on a Pascal (6,0) GPU.
            let restrictions = catalog_restrictions("qwen3-coder-30b");
            let position = manager_models::expert_offload(Some("--n-cpu-moe 46"));
            let reason = offload_block_reason(
                &restrictions,
                position.layers(48),
                48,
                &[(6, 0)],
                false,
                false,
            )
            .expect("46 of 48 on compute capability 6.0 is blocked");
            assert!(reason.contains("offload of 46 of 48 layers"), "{reason}");
            assert!(reason.contains("compute capability 6.0"), "{reason}");
            assert!(
                reason.contains("set the offload to 48 of 48 layers"),
                "{reason}"
            );
        }

        #[test]
        fn offload_block_reason_overridden_position_is_allowed() {
            let restrictions = catalog_restrictions("qwen3-coder-30b");
            assert_eq!(
                offload_block_reason(&restrictions, 46, 48, &[(6, 0)], false, true),
                None
            );
        }

        #[test]
        fn offload_block_reason_endpoints_are_allowed() {
            let restrictions = catalog_restrictions("qwen3-coder-30b");
            for layers in [0, 48] {
                assert_eq!(
                    offload_block_reason(&restrictions, layers, 48, &[(6, 0)], false, false),
                    None,
                    "endpoint {layers} is verified on 6.0"
                );
            }
        }

        #[test]
        fn offload_block_reason_other_gpu_is_allowed() {
            // Placement re-evaluation: the same saved position on a
            // newer GPU carries no pinned restriction.
            let restrictions = catalog_restrictions("qwen3-coder-30b");
            assert_eq!(
                offload_block_reason(&restrictions, 46, 48, &[(8, 6)], false, false),
                None
            );
        }

        #[test]
        fn offload_block_reason_unrestricted_model_is_allowed() {
            let restrictions = catalog_restrictions("nemotron-3.5-lightning-30b");
            assert!(restrictions.is_empty());
            assert_eq!(
                offload_block_reason(&restrictions, 16, 24, &[(6, 0)], false, false),
                None
            );
        }

        #[test]
        fn offload_block_reason_cpu_only_is_allowed() {
            let restrictions = catalog_restrictions("qwen3-coder-30b");
            assert_eq!(
                offload_block_reason(&restrictions, 46, 48, &[], true, false),
                None
            );
        }

        #[test]
        fn offload_info_payload_fields_for_coder_and_nemotron() {
            // sys_bytes_by_position is exact for Coder-Next (uniform
            // expert blocks): index 0 is empty, index 48 is the full
            // expert bytes.
            let mut cfg = test_cfg();
            let b = serve_model(
                &mut cfg,
                "coder",
                "qwen3-coder-30b",
                "models/Qwen3-Coder-Next-UD-Q4_K_M.gguf",
                Some("--n-cpu-moe 46"),
            );
            let info = backend_offload_info(&cfg, &b).expect("coder info");
            assert_eq!(info.sys_bytes_by_position.len(), 49);
            assert_eq!(info.sys_bytes_by_position[0], 0);
            assert_eq!(info.sys_bytes_by_position[48], info.expert_bytes);
            assert!(!info.custom, "canonical spelling is not custom");
            assert_eq!(info.restrictions.len(), 1);

            // A padded-but-equivalent spelling is not custom either;
            // a spelling the canonical form would not produce is.
            let b = serve_model(
                &mut cfg,
                "coder",
                "qwen3-coder-30b",
                "models/Qwen3-Coder-Next-UD-Q4_K_M.gguf",
                Some("  --n-cpu-moe   46  "),
            );
            let info = backend_offload_info(&cfg, &b).expect("coder info");
            assert!(!info.custom, "whitespace-only difference is not custom");

            let b = serve_model(
                &mut cfg,
                "coder-fast",
                "nemotron-3.5-lightning-30b",
                "models/Nemotron.gguf",
                Some("--n-cpu-moe=16"),
            );
            let info = backend_offload_info(&cfg, &b).expect("nemotron info");
            assert!(
                info.custom,
                "the `=` spelling is not the canonical form for the position"
            );
            assert_eq!(info.sys_bytes_by_position.len(), 25);
            assert!(info.restrictions.is_empty());
        }

        #[test]
        fn model_add_entry_unknown_size_is_a_clean_error() {
            // Tracker #32: an unknown-size model used to be stored with
            // params_b 0.0, which fails config validation. The add must
            // now refuse up front, naming what is missing.
            let r = model_add_entry(
                "obscure-1b",
                "https://example.com/obscure.gguf",
                None,
                None,
                None,
                None,
                None,
            );
            match r {
                Err(ApiError::InvalidInput(msg)) => {
                    assert!(msg.contains("params_b"), "error names the gap: {msg}");
                    assert!(msg.contains("obscure-1b"), "error names the model: {msg}");
                }
                other => panic!("expected InvalidInput, got {other:?}"),
            }
            // A non-positive explicit value counts as "not supplied".
            let r = model_add_entry(
                "obscure-1b",
                "https://example.com/obscure.gguf",
                None,
                None,
                Some(0.0),
                None,
                None,
            );
            assert!(matches!(r, Err(ApiError::InvalidInput(_))), "{r:?}");
        }

        #[test]
        fn model_add_entry_uses_explicit_params() {
            // Known-size add: behavior unchanged from before the fix —
            // enabled, unpinned hash, file name from the URL, quant
            // defaulting to "unknown".
            let e = model_add_entry(
                "my-7b",
                "https://example.com/my-7b-q4.gguf",
                None,
                None,
                Some(7.5),
                None,
                None,
            )
            .expect("explicit params resolve");
            assert_eq!(e.id, "my-7b");
            assert_eq!(e.params_b, 7.5);
            assert_eq!(e.gguf_path, PathBuf::from("models").join("my-7b-q4.gguf"));
            assert_eq!(e.quant, "unknown");
            assert!(e.enabled);
            assert!(e.sha256.is_empty());
            // The built entry passes config validation (the defect was
            // an entry that could not).
            let mut cfg = test_cfg();
            cfg.models.push(e);
            manager_config::validate(&cfg).expect("added entry validates");
            // An explicit value wins over the catalog.
            let e = model_add_entry("qwen3-8b", "https://example.com/x.gguf", None, None, Some(9.0), None, None)
                .expect("explicit params resolve");
            assert_eq!(e.params_b, 9.0);
        }

        #[test]
        fn model_add_entry_resolves_params_from_catalog() {
            // By id.
            let e = model_add_entry("qwen3-8b", "https://example.com/x.gguf", None, None, None, None, None)
                .expect("catalog id resolves");
            assert_eq!(e.params_b, 8.0);
            // By exact source URL under an operator-chosen id.
            let e = model_add_entry(
                "my-qwen3-8b",
                "https://huggingface.co/Qwen/Qwen3-8B-GGUF/resolve/main/Qwen3-8B-Q4_K_M.gguf",
                None,
                None,
                None,
                None,
                None,
            )
            .expect("catalog URL resolves");
            assert_eq!(e.params_b, 8.0);
        }

        #[test]
        fn model_add_entry_stores_and_validates_the_moe_declaration() {
            let e = model_add_entry(
                "moe-27b",
                "https://example.com/moe.gguf",
                None,
                None,
                Some(27.0),
                Some(48),
                Some(16),
            )
            .expect("declared pair resolves");
            assert_eq!(e.moe_layers, Some(48));
            assert_eq!(e.default_offload_layers, Some(16));
            let mut cfg = test_cfg();
            cfg.models.push(e);
            manager_config::validate(&cfg).expect("declared entry validates");
            // The pair is atomic at the write path: half a
            // declaration, an over-range default, and a zero count
            // all refuse.
            for (layers, default) in [
                (Some(48), None),
                (None, Some(16)),
                (Some(48), Some(49)),
                (Some(0), Some(0)),
            ] {
                assert!(
                    matches!(
                        model_add_entry(
                            "moe-27b",
                            "https://example.com/moe.gguf",
                            None,
                            None,
                            Some(27.0),
                            layers,
                            default,
                        ),
                        Err(ApiError::InvalidInput(_))
                    ),
                    "{layers:?} / {default:?} must refuse"
                );
            }
        }

        #[test]
        fn model_add_entry_keeps_the_existing_input_rules() {
            assert!(matches!(
                model_add_entry("  ", "https://example.com/x.gguf", None, None, Some(1.0), None, None),
                Err(ApiError::InvalidInput(_))
            ));
            assert!(matches!(
                model_add_entry("m", "http://example.com/x.gguf", None, None, Some(1.0), None, None),
                Err(ApiError::InvalidInput(_))
            ));
            assert!(matches!(
                model_add_entry(
                    "m",
                    "https://example.com/x.gguf",
                    Some("sub/dir.gguf".to_string()),
                    None,
                    Some(1.0),
                    None,
                    None
                ),
                Err(ApiError::InvalidInput(_))
            ));
        }

        #[test]
        fn model_add_save_step_never_persists_an_invalid_config() {
            // The command's persistence shape: push onto a CLONE, save
            // (which validates first), swap into state only on success.
            // With a hand-built invalid entry (params_b 0.0 — what the
            // old code produced), the save must fail and both the file
            // on disk and the live config must stay exactly as they were.
            let dir = std::env::temp_dir().join(format!(
                "model-add-save-step-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("config.json");
            let cfg = test_cfg();
            manager_config::save(&cfg, &path).expect("baseline config saves");
            let before = std::fs::read(&path).unwrap();

            let mut new_cfg = cfg.clone();
            new_cfg.models.push(manager_config::ModelConfig {
                id: "bad-0b".to_string(),
                gguf_path: PathBuf::from("models").join("bad.gguf"),
                quant: "unknown".to_string(),
                params_b: 0.0,
                sha256: String::new(),
                verified_at: None,
                verified_sha256: None,
                source_url: "https://example.com/bad.gguf".to_string(),
                assigned_backends: Vec::new(),
                enabled: true,
                display_name: None,
                moe_layers: None,
                default_offload_layers: None,
            });
            assert!(manager_config::save(&new_cfg, &path).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), before, "file untouched");
            assert!(
                !cfg.models.iter().any(|m| m.id == "bad-0b"),
                "live config untouched (the entry only ever existed on the clone)"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
}

#[derive(Debug, Clone, PartialEq)]
struct OffloadEvaluation {
    layers: u32,
    moe_layers: u32,
    status: manager_models::OffloadStatus,
    /// First placed compute capability a restriction governs, when
    /// any placed GPU is restricted.
    restricting_cc: Option<(u32, u32)>,
    /// Nearest allowed position under the governing restriction
    /// (the position itself when nothing governs it).
    nearest_verified_layers: u32,
    /// A journaled override exists for exactly this position on
    /// exactly this placement.
    has_override: bool,
    /// The position is blocked AND a journaled override covers it.
    overridden: bool,
    placed_ccs: Vec<(u32, u32)>,
    cpu_only: bool,
    restrictions: Vec<manager_models::OffloadRestriction>,
}

#[derive(Debug, Clone, Serialize)]
struct McpListEntry {
    pub id: String,
    pub display_name: String,
    /// Same value as `display_name` (the name `McpServerStatus`
    /// historically carried), kept for back-compat readers.
    pub name: String,
    pub transport: manager_config::McpTransport,
    pub exposure: manager_config::McpExposure,
    /// Derived runtime mode (Gateway maps to Remote; see
    /// `manager_mcp::server_spec_from_config`).
    pub mode: manager_mcp::McpMode,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub url: Option<String>,
    pub env_keys: Vec<String>,
    pub cred_ref: Option<String>,
    /// Registration / API-key page for the server's credential,
    /// resolved by [`mcp_signup_url`] (stored value, else the
    /// curated seed's).
    pub signup_url: Option<String>,
    /// What credential the operator stores for the server (kind
    /// and where it comes from), resolved by
    /// [`mcp_credential_hint`] (stored value, else the seeded
    /// registry's for the same id). The MCP Servers view renders
    /// it as the credential field's label.
    pub credential_hint: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    pub metered: bool,
    pub rpm_limit: Option<u32>,
    /// Live supervised-process state from `list_status`; only
    /// Local servers are ever spawned, so this is meaningful
    /// (and only ever true) for them.
    pub running: bool,
    /// Credential presence over the accepted targets (see
    /// `credential_target_found`): true when any accepted target
    /// exists.
    pub credential_present: bool,
    /// `"present"` | `"missing"` — the §9 card wording.
    pub credential_status: String,
    /// Which accepted Credential Manager target was found, when
    /// one was (dual-target acceptance, design §10.1: for the two
    /// legacy servers either the unified wizard target or the
    /// legacy `llm-manager/mcp/<id>` target counts).
    pub credential_target_found: Option<String>,
    /// Per-env-key presence, as the pre-Phase-1 payload carried it
    /// (values are never read).
    pub credential_presence: Vec<(String, bool)>,
}

struct MetricsHttpResponse {
    status: u16,
    location: Option<String>,
    /// The message body with any chunked transfer encoding already
    /// decoded (see [`decode_metrics_response`]).
    body: String,
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
    manager_config::save(&new_cfg, &state.config_path)
        .map_err(ApiError::from)?;
    *cfg = new_cfg;
    Ok(json!({ "ok": true, "persisted": true }))
}

/// Fire one toast immediately (Settings → Notifications, "Send test
/// notification"). Deliberately bypasses the category settings via
/// `notify::send_toast`: the control exists to prove the OS delivery
/// path, and a master switch set to off must not make it look broken
/// — the card reports the switch state separately. A send error is
/// returned so the card can show why delivery failed.
pub async fn op_notifications_send_test(
    _state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
) -> ApiResult {
    notify::send_toast(
        sink.as_ref(),
        "LLMdil test notification",
        "Toast delivery is working. Event notifications follow the switches on this card.",
    )
    .map_err(ApiError::Io)?;
    Ok(json!({ "ok": true }))
}

/// Tool-call probe against a backend (MCP design §7.2): health gate,
/// `/props` template evidence, then a direct chat completion that
/// must produce a structured tool call. The outcome is persisted on
/// the backend as `tool_call_probe` (runtime state, not operator
/// config — it never appears in a config diff) and the full probe
/// result is returned.
pub async fn op_backend_test_tool_call(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    let base_url = {
        let cfg = state.config.read().await;
        let backend = cfg
            .backends
            .iter()
            .find(|b| b.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
        format!("http://127.0.0.1:{}", backend.port)
    };
    let result = test_tool_call(&base_url)
        .await
        .map_err(ApiError::from)?;
    // The supervisor and config crates spell the same three outcomes
    // with separate enums (neither depends on the other); the
    // mapping between them lives here, at the layer that holds both.
    let outcome = match result.outcome {
        manager_supervisor::ToolCallProbeOutcome::Verified => {
            manager_config::ToolCallProbeOutcome::Verified
        }
        manager_supervisor::ToolCallProbeOutcome::Unparsed => {
            manager_config::ToolCallProbeOutcome::Unparsed
        }
        manager_supervisor::ToolCallProbeOutcome::Failed => {
            manager_config::ToolCallProbeOutcome::Failed
        }
    };
    let mut cfg = state.config.write().await;
    if let Some(b) = cfg.backends.iter_mut().find(|b| b.id == id) {
        b.tool_call_probe = Some(manager_config::ToolCallProbe {
            outcome,
            at_ms: now_ms(),
        });
    }
    manager_config::save(&cfg, &state.config_path)
        .map_err(ApiError::from)?;
    drop(cfg);
    Ok(serde_json::to_value(&result)?)
}

/// Supervision state for Settings: the mode, the resolved service
/// account, shim presence, and every managed service's registration
/// + SCM state.
pub async fn op_services_list(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let cfg = state.config.read().await.clone();
    let host = service_host_path();
    let mut targets: Vec<(String, String)> = cfg
        .backends
        .iter()
        .map(|b| (b.id.clone(), manager_services::backend_service_name(&b.id)))
        .collect();
    targets.push((
        "gateway".to_string(),
        manager_services::GATEWAY_SERVICE_NAME.to_string(),
    ));
    let queried = tokio::task::spawn_blocking(move || {
        use manager_services::ServiceControl;
        let control = manager_services::scm::platform_control();
        targets
            .into_iter()
            .map(|(target, name)| {
                let status = control.query(&name).ok();
                (target, name, status)
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    let services: Vec<Value> = queried
        .into_iter()
        .map(|(target, name, status)| {
            let live = status.as_ref().filter(|s| {
                !matches!(s.state, manager_services::ServiceState::NotInstalled)
            });
            json!({
                "target": target,
                "service_name": name,
                "registered": live.is_some(),
                "state": live.map(|s| serde_json::to_value(s.state).unwrap_or(Value::Null)),
                "pid": live.and_then(|s| s.pid),
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
/// pattern; the account password reaches the helper through a
/// transient credential the helper deletes (and this command
/// overwrites afterwards either way).
pub async fn op_services_register(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    target: String,
    password: Option<String>,
) -> ApiResult {
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
        manager_services::read_definition(&path)
            .map_err(|e| ApiError::Services(e.to_string()))?
    };
    let account = manager_services::scm::current_user_account().ok_or_else(|| {
        ApiError::Services(
            "could not resolve the current user account for the service".to_string(),
        )
    })?;
    def.account = Some(account.clone());
    let service_name = def.name.clone();
    let def_path = manager_services::write_definition(&state.data_dir, &def)
        .map_err(|e| ApiError::Services(e.to_string()))?;
    let password = password.unwrap_or_default();
    if !password.is_empty() {
        <RealSystemOps as manager_wizard::SystemOps>::credential_write(
            &RealSystemOps::default(),
            manager_services::SERVICE_ACCOUNT_PASSWORD_CRED_TARGET,
            &password,
        )
        .await
        .map_err(|e| {
            ApiError::Services(format!(
                "could not stage the account password: {e}"
            ))
        })?;
    }
    let cmd = manager_wizard::PlannedCommand::new(vec![
        host.display().to_string(),
        "register".to_string(),
        "--definition".to_string(),
        def_path.display().to_string(),
    ]);
    let outcome =
        <RealSystemOps as manager_wizard::SystemOps>::run_elevated(&RealSystemOps::default(), &cmd).await;
    if !password.is_empty() {
        // The helper deletes the credential on any run; if the
        // elevation was declined the helper never ran, so clear the
        // staged value here (an empty write reads back as absent).
        let _ = <RealSystemOps as manager_wizard::SystemOps>::credential_write(
            &RealSystemOps::default(),
            manager_services::SERVICE_ACCOUNT_PASSWORD_CRED_TARGET,
            "",
        )
        .await;
    }
    let out = outcome.map_err(|e| {
        ApiError::Services(format!(
            "elevated registration could not run (was the prompt declined?): {e}"
        ))
    })?;
    if out.exit_code != 0 {
        return Err(ApiError::Services(format!(
            "registration of {service_name} failed (helper exit {}); see logs/service-manage.log in the data directory",
            out.exit_code
        )));
    }
    Ok(json!({ "ok": true, "service_name": service_name, "account": account }))
}

/// Unregister one service (`target`: a backend id, or "gateway")
/// through the elevated helper. The service returns to process
/// supervision; its definition file is left in place.
pub async fn op_services_unregister(
    _state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    target: String,
) -> ApiResult {
    let host = require_service_host()?;
    let service_name = if target == "gateway" {
        manager_services::GATEWAY_SERVICE_NAME.to_string()
    } else {
        manager_services::backend_service_name(&target)
    };
    let cmd = manager_wizard::PlannedCommand::new(vec![
        host.display().to_string(),
        "unregister".to_string(),
        "--name".to_string(),
        service_name.clone(),
    ]);
    let out = <RealSystemOps as manager_wizard::SystemOps>::run_elevated(&RealSystemOps::default(), &cmd)
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
    Ok(json!({ "ok": true, "service_name": service_name }))
}

/// Set an MCP server's exposure (replaces `mcp_set_mode`, MCP
/// Phase 1). Exposure is persisted in `AppConfig.mcp` through the
/// validated save path (`manager_config::save` on a clone; only a
/// successful save installs it in memory — the pattern of
/// `backend_update_flags`). Transitioning INTO Local spawns the
/// supervised process (the old set_mode behaviour); LEAVING Local
/// stops it. Gateway servers are never spawned here — the gateway
/// hosts them (Phase 2).
pub async fn op_mcp_set_exposure(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    exposure: String,
) -> ApiResult {
    let exposure = match exposure.as_str() {
        "gateway" => manager_config::McpExposure::Gateway,
        "local" => manager_config::McpExposure::Local,
        "disabled" => manager_config::McpExposure::Disabled,
        other => {
            return Err(ApiError::Mcp(format!(
                "unknown mcp exposure {other:?}; expected gateway | local | disabled"
            )))
        }
    };
    let previous_exposure = {
        let mut cfg = state.config.write().await;
        let mut new_cfg = cfg.clone();
        let server = new_cfg
            .mcp
            .servers
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("mcp server {id}")))?;
        let previous = server.exposure;
        server.exposure = exposure;
        manager_config::save(&new_cfg, &state.config_path)
            .map_err(ApiError::from)?;
        *cfg = new_cfg;
        previous
    };
    let servers = state.mcp_servers().await;
    let server = servers
        .iter()
        .find(|s| s.id == id)
        .expect("exposure was just persisted for a known server");
    if exposure == manager_config::McpExposure::Local
        && previous_exposure != manager_config::McpExposure::Local
    {
        let mut sup = state.supervisor().await.lock().await;
        manager_mcp::spawn_local(&mut sup, server)
            .await
            .map_err(ApiError::from)?;
    } else if previous_exposure == manager_config::McpExposure::Local
        && exposure != manager_config::McpExposure::Local
    {
        let mut sup = state.supervisor().await.lock().await;
        manager_mcp::stop_local(&mut sup, server)
            .await
            .map_err(ApiError::from)?;
    }
    Ok(json!({ "id": id, "exposure": exposure, "persisted": true }))
}

/// Create or update an MCP server in the persisted registry
/// (upsert by id). The argument is the `McpServerConfig` shape as
/// JSON (§3.1). Persistence goes through `manager_config::save`,
/// which validates the whole config first — a §3.2 violation is
/// returned as a command error and nothing is stored (neither on
/// disk nor in memory).
pub async fn op_mcp_save_server(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    server: manager_config::McpServerConfig,
) -> ApiResult {
    let id = server.id.clone();
    let mut cfg = state.config.write().await;
    let mut new_cfg = cfg.clone();
    match new_cfg.mcp.servers.iter_mut().find(|s| s.id == id) {
        Some(existing) => *existing = server,
        None => new_cfg.mcp.servers.push(server),
    }
    manager_config::save(&new_cfg, &state.config_path)
        .map_err(ApiError::from)?;
    *cfg = new_cfg;
    Ok(json!({ "id": id, "persisted": true }))
}

/// Remove an MCP server from the persisted registry. If it was
/// Local and running, it is stopped first; then the removal is
/// persisted through the validated save path.
pub async fn op_mcp_delete_server(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    let servers = state.mcp_servers().await;
    let server = servers
        .iter()
        .find(|s| s.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("mcp server {id}")))?;
    if server.mode == manager_mcp::McpMode::Local {
        let mut sup = state.supervisor().await.lock().await;
        manager_mcp::stop_local(&mut sup, server)
            .await
            .map_err(ApiError::from)?;
    }
    let mut cfg = state.config.write().await;
    let mut new_cfg = cfg.clone();
    new_cfg.mcp.servers.retain(|s| s.id != id);
    manager_config::save(&new_cfg, &state.config_path)
        .map_err(ApiError::from)?;
    *cfg = new_cfg;
    Ok(json!({ "id": id, "deleted": true, "persisted": true }))
}

/// Store an MCP server's credential in Windows Credential Manager
/// under the server's declared `cred_ref` target (MCP design §9:
/// the MCP view's credential Store action). The value goes straight
/// to the ops-layer `credential_write` — the same mechanism the
/// stored gateway master key uses — and is never logged, returned,
/// or persisted anywhere else.
pub async fn op_mcp_store_credential(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    value: String,
) -> ApiResult {
    if value.trim().is_empty() {
        return Err(ApiError::InvalidInput(
            "credential value must not be empty".to_string(),
        ));
    }
    let target = {
        let cfg = state.config.read().await;
        let server = cfg
            .mcp
            .servers
            .iter()
            .find(|s| s.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("mcp server {id}")))?;
        server
            .cred_ref
            .clone()
            .filter(|r| !r.trim().is_empty())
            .ok_or_else(|| {
                ApiError::InvalidInput(format!(
                    "mcp server {id} declares no credential target"
                ))
            })?
    };
    RealSystemOps::default()
        .credential_write(&target, &value)
        .await
        .map_err(ApiError::from)?;
    Ok(json!({ "id": id, "stored": true }))
}

/// Evaluate a backend's SAVED offload position against the GPU
/// inventory and the activation journal's standing overrides.
/// `None` when the backend's model is not offload-capable.
fn evaluate_backend_offload(
    cfg: &AppConfig,
    b: &manager_config::BackendConfig,
    gpus: &[GpuInfo],
    journal: &activation::ActivationJournal,
) -> Option<OffloadEvaluation> {
    let info = backend_offload_info(cfg, b)?;
    let (placed_ccs, cpu_only) = placed_offload_placement(b, gpus);
    Some(evaluate_offload_position(
        &info.restrictions,
        &b.id,
        info.layers,
        info.moe_layers,
        placed_ccs,
        cpu_only,
        journal,
    ))
}

/// Evaluate one offload position from its parts (the pure core of
/// [`evaluate_backend_offload`], shared with the apply path, which
/// evaluates the REQUESTED position rather than the saved one).
#[allow(clippy::too_many_arguments)]
fn evaluate_offload_position(
    restrictions: &[manager_models::OffloadRestriction],
    backend_id: &str,
    layers: u32,
    moe_layers: u32,
    placed_ccs: Vec<(u32, u32)>,
    cpu_only: bool,
    journal: &activation::ActivationJournal,
) -> OffloadEvaluation {
    let status = manager_models::offload_status(
        restrictions,
        layers,
        moe_layers,
        &placed_ccs,
        cpu_only,
    );
    let restricting_cc = placed_ccs.iter().copied().find(|cc| {
        restrictions
            .iter()
            .any(|r| r.compute_capability == *cc)
    });
    // The nearest allowed position is computed against the
    // restricting architecture; with no placed architecture it is
    // computed against the first restriction's, so an unknown
    // placement still offers the snap target for its own CC.
    let nearest_cc = restricting_cc.or_else(|| {
        if placed_ccs.is_empty() {
            restrictions.first().map(|r| r.compute_capability)
        } else {
            None
        }
    });
    let nearest_verified_layers = match nearest_cc {
        Some(cc) => manager_models::nearest_allowed(restrictions, cc, layers, moe_layers),
        None => layers,
    };
    let has_override =
        activation::offload_override_covers(journal, backend_id, layers, &placed_ccs);
    OffloadEvaluation {
        layers,
        moe_layers,
        status,
        restricting_cc,
        nearest_verified_layers,
        has_override,
        overridden: status == manager_models::OffloadStatus::Blocked && has_override,
        placed_ccs,
        cpu_only,
        restrictions: restrictions.to_vec(),
    }
}

/// The refusal reason for an offload position, when one applies:
/// `Some` iff the position is guardrail-blocked and no journaled
/// override covers it. Pure — the preflight, the apply path, and
/// the tests all go through this one function.
fn offload_block_reason(
    restrictions: &[manager_models::OffloadRestriction],
    layers: u32,
    moe_layers: u32,
    placed_ccs: &[(u32, u32)],
    cpu_only: bool,
    overridden: bool,
) -> Option<String> {
    if overridden {
        return None;
    }
    if manager_models::offload_status(restrictions, layers, moe_layers, placed_ccs, cpu_only)
        != manager_models::OffloadStatus::Blocked
    {
        return None;
    }
    // Blocked implies a placed CC matches a restriction; the first
    // such CC names the architecture in the message and drives the
    // nearest-allowed snap.
    let cc = placed_ccs.iter().copied().find(|cc| {
        restrictions
            .iter()
            .any(|r| r.compute_capability == *cc)
    })?;
    let nearest = manager_models::nearest_allowed(restrictions, cc, layers, moe_layers);
    Some(format!(
        "offload of {layers} of {moe_layers} layers is outside the verified range for this GPU (compute capability {}.{}) — set the offload to {nearest} of {moe_layers} layers in Edit Flags, or save the position with an explicit override",
        cc.0, cc.1
    ))
}

/// A backend's GPU placement for guardrail evaluation: the placed
/// GPUs' compute capabilities, and whether the launch is CPU-only.
/// Undetermined placement yields an empty set (architecture unknown).
fn placed_offload_placement(
    b: &manager_config::BackendConfig,
    gpus: &[GpuInfo],
) -> (Vec<(u32, u32)>, bool) {
    match backend_gpu_visibility(b) {
        GpuVisibility::CpuOnly => (Vec::new(), true),
        GpuVisibility::Gpus(indices) => (
            indices
                .iter()
                .filter_map(|i| {
                    gpus.iter()
                        .find(|g| g.index == *i)
                        .map(|g| g.compute_capability)
                })
                .collect(),
            false,
        ),
        GpuVisibility::Undetermined => (Vec::new(), false),
    }
}

/// Raw flags normalized for comparison: whitespace-split tokens
/// re-joined with single spaces; `None` when nothing remains.
fn token_normalized_flags(raw: Option<&str>) -> Option<String> {
    let joined = raw
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// Whether a saved offload change for `id` is still awaiting this
/// start: the activation journal carries an unresolved backend-scope
/// `backend_set_offload` entry targeting it.
async fn offload_change_pending(state: &AppState, cfg: &AppConfig, id: &str) -> bool {
    let journal = activation::load_journal(&state.data_dir);
    let started_at = backend_start_times(state).await;
    let configured: std::collections::HashSet<String> =
        cfg.backends.iter().map(|b| b.id.clone()).collect();
    activation::unresolved_entries_scoped(&state.data_dir, &journal, &started_at, &configured)
        .iter()
        .any(|e| {
            e.scope == "backend"
                && e.kind == "backend_set_offload"
                && e.targets.iter().any(|t| t == id)
        })
}

/// The offload probe context for a start of `id`, when the start
/// activates a pending offload change: (layers, moe_layers, port)
/// of the SAVED position. Captured before the spawn, after the
/// preflight passes — a start that the preflight refuses never
/// probes, and the position probed is the one being activated.
async fn offload_probe_context(
    state: &AppState,
    id: &str,
) -> Option<(u32, u32, u16)> {
    let cfg = state.config.read().await.clone();
    if !offload_change_pending(state, &cfg, id).await {
        return None;
    }
    let b = cfg.backends.iter().find(|b| b.id == id)?;
    let info = backend_offload_info(&cfg, b)?;
    Some((info.layers, info.moe_layers, b.port))
}

/// Whether the backend's supervised process is still alive (Running
/// or Starting). Reaps first, so a process that exited since the
/// last poll reads as gone here, not as its last live state: this
/// reap-plus-status check is how the probe detects process death.
async fn offload_probe_backend_alive(state: &AppState, id: &str) -> bool {
    let mut sup = state.supervisor().await.lock().await;
    sup.reap_exited();
    sup.status()
        .iter()
        .find(|s| s.id == id)
        .map(|s| matches!(s.state, ProcessState::Running | ProcessState::Starting))
        .unwrap_or(false)
}

/// Service-mode backend start: regenerate the definition from the
/// current spec, then start the service through the SCM. The
/// service reports Running once the host has spawned llama-server;
/// model-load progress is observed through `list_backends`, as in
/// process mode.
async fn scm_start_backend(state: &AppState, id: &str) -> Result<(), ApiError> {
    state
        .write_backend_service_definition(id)
        .await
        .map_err(ApiError::Services)?;
    let name = manager_services::backend_service_name(id);
    tokio::task::spawn_blocking(move || {
        use manager_services::ServiceControl;
        manager_services::scm::platform_control().start(&name)
    })
    .await
    .map_err(|e| ApiError::Services(format!("service start task failed: {e}")))?
    .map_err(|e| ApiError::Services(e.to_string()))
}

/// Service-mode backend stop: SCM stop, then wait for the stopped
/// state — the supervisor's stop returns once the process is down,
/// so the service path does too. (A service-mode stop terminates the
/// host's job object: the whole tree goes down at once. The graceful
/// drain window is process-mode behavior in increment 1.)
async fn scm_stop_backend(id: &str) -> Result<(), ApiError> {
    let name = manager_services::backend_service_name(id);
    tokio::task::spawn_blocking(move || {
        use manager_services::ServiceControl;
        let control = manager_services::scm::platform_control();
        control.stop(&name).map_err(|e| e.to_string())?;
        for _ in 0..300 {
            match control.query(&name).map_err(|e| e.to_string())?.state {
                manager_services::ServiceState::Stopped
                | manager_services::ServiceState::NotInstalled => return Ok(()),
                _ => std::thread::sleep(std::time::Duration::from_millis(100)),
            }
        }
        Err(format!("service '{name}' did not stop within 30 s"))
    })
    .await
    .map_err(|e| ApiError::Services(format!("service stop task failed: {e}")))?
    .map_err(ApiError::Services)
}

fn require_service_host() -> Result<std::path::PathBuf, ApiError> {
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
fn service_host_path() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("llm-service-host.exe")))
        .unwrap_or_else(|| std::path::PathBuf::from("llm-service-host.exe"))
}

/// The Gateway-exposure connection test (MCP design §8): the same
/// two calls the start-gateway step's post-start load check makes —
/// `GET /v1/mcp/server` (is the server registered with the
/// gateway?) and `GET /mcp-rest/tools/list?server_id=<id>` (does
/// its tools list answer without error?) — against the running
/// gateway, authenticated with the stored master key. Pass
/// requires both. The result carries the tool names when listed.
///
/// Phase 3 (§8's explicit-Test addition): when the tested server is
/// the curated `brave_search` AND the two-call test passes, this
/// explicit test — and only this explicit test; the start-time load
/// check never does it — performs ONE real tool call
/// (`POST /mcp-rest/tools/call`, fixed harmless arguments from
/// `manager_wizard::brave_search_test_call_body`) and the payload
/// gains `real_call_ok` / `real_call_detail` (the detail is a
/// capped excerpt, never the full search response). For
/// `web_fetch` a real call is NEVER performed, even here (§8): its
/// test stays registration + tools list, and its detail says so.
async fn mcp_test_gateway_connection(
    state: &AppState,
    entry: &manager_config::McpServerConfig,
) -> ApiResult {
    let id = entry.id.clone();
    let (host, port) = {
        let cfg = state.config.read().await;
        (
            manager_wizard::steps::gateway_probe_host(&cfg),
            cfg.gateway.port,
        )
    };
    let master = RealSystemOps::default()
        .credential_read(manager_wizard::LITELLM_MASTER_KEY_CRED_TARGET)
        .await
        .filter(|k| !k.is_empty());
    let Some(master) = master else {
        return Ok(json!({
            "id": id,
            "ok": false,
            "registered": false,
            "tools": [],
            "detail": "gateway master key not available in Windows Credential Manager; start the gateway once so the app stores one",
        }));
    };
    let master_for_call = master.clone();
    let (list, tools) = {
        let host = host.clone();
        let server_id = id.clone();
        tokio::task::spawn_blocking(move || {
            let list = gateway_http_get(&host, port, "/v1/mcp/server", &master);
            let tools = gateway_http_get(
                &host,
                port,
                &format!("/mcp-rest/tools/list?server_id={server_id}"),
                &master,
            );
            (list, tools)
        })
        .await
        .unwrap_or((None, None))
    };
    let mut problems: Vec<String> = Vec::new();
    let registered = match &list {
        Some((200, body)) => {
            if manager_wizard::parse_mcp_registered_server_ids(body)
                .iter()
                .any(|r| r == &id)
            {
                true
            } else {
                problems.push("not in the gateway's registered MCP server list".to_string());
                false
            }
        }
        Some((status, body)) => {
            let detail = manager_wizard::mcp_response_error(body)
                .unwrap_or_else(|| format!("HTTP {status}"));
            problems.push(format!("GET /v1/mcp/server failed: {detail}"));
            false
        }
        None => {
            problems.push(format!("gateway unreachable at http://{host}:{port}"));
            false
        }
    };
    let mut tool_names: Vec<String> = Vec::new();
    let tools_ok = match &tools {
        Some((200, body)) => match manager_wizard::mcp_response_error(body) {
            Some(e) => {
                problems.push(format!("tools list returned an error: {e}"));
                false
            }
            None => {
                tool_names = manager_wizard::parse_mcp_tool_names(body);
                true
            }
        },
        Some((status, body)) => {
            let detail = manager_wizard::mcp_response_error(body)
                .unwrap_or_else(|| format!("HTTP {status}"));
            problems.push(format!("tools list failed: {detail}"));
            false
        }
        None => {
            if list.is_some() {
                problems.push("tools list request failed: gateway unreachable".to_string());
            }
            false
        }
    };
    let ok = registered && tools_ok;
    let mut detail = if ok {
        format!(
            "registered with the gateway; {} tool(s) listed",
            tool_names.len()
        )
    } else {
        problems.join("; ")
    };
    let mut payload = json!({
        "id": id,
        "ok": ok,
        "registered": registered,
        "tools": tool_names,
        "detail": detail,
    });
    // Phase 3 real-call behaviour (§8) — explicit test only.
    if id == "web_fetch" {
        // NEVER a real call for web_fetch, even from this explicit
        // test: a model-chosen URL fetch is the §5.2 SSRF surface,
        // and the test's fixed arguments would still exercise it.
        detail = format!("{detail}; real call skipped for web_fetch by design");
        payload["detail"] = json!(detail);
        payload["real_call_skipped"] = json!(true);
    } else if id == "brave_search" && ok {
        // One real, metered call with fixed harmless arguments.
        // The body is built by the one constructor in
        // manager-wizard (see its doc for the verified REST shape).
        let body = manager_wizard::brave_search_test_call_body();
        let call = {
            let host = host.clone();
            let master = master_for_call.clone();
            tokio::task::spawn_blocking(move || {
                gateway_http_post(&host, port, "/mcp-rest/tools/call", &master, &body)
            })
            .await
            .unwrap_or(None)
        };
        let (call_ok, call_detail) = match &call {
            Some((status, resp_body)) => {
                manager_wizard::summarize_mcp_tool_call_response(*status, resp_body)
            }
            None => (
                false,
                "real tool call request failed: gateway unreachable".to_string(),
            ),
        };
        payload["real_call_ok"] = json!(call_ok);
        payload["real_call_detail"] = json!(call_detail);
    }
    Ok(payload)
}

/// The activation plan's current unresolved entries and drift flag —
/// the same computation `gateway_status` reports, shared by the
/// notification monitor and the activation hooks.
async fn activation_unresolved(state: &AppState) -> (Vec<activation::ActivationEntry>, bool) {
    let cfg = state.config.read().await;
    let journal = activation::load_journal(&state.data_dir);
    let started_at = backend_start_times(state).await;
    let configured: std::collections::HashSet<String> =
        cfg.backends.iter().map(|b| b.id.clone()).collect();
    let unresolved =
        activation::unresolved_entries_scoped(&state.data_dir, &journal, &started_at, &configured);
    let presence = mcp_credential_presence(&[&cfg]).await;
    let drift = activation::config_drift(&cfg, &state.data_dir, |t| presence.contains(t));
    (unresolved, drift)
}

/// Whether an activation plan is pending right now (any unresolved
/// entry, or live config drift).
async fn activation_plan_pending(state: &AppState) -> bool {
    let (unresolved, drift) = activation_unresolved(state).await;
    !unresolved.is_empty() || drift
}

/// Build the registry entry for [`model_add`]: every input rule and
/// the parameter-count resolution, decided before the config is
/// touched, so a rejected add leaves the in-memory config and the
/// file on disk exactly as they were. Pure — unit-tested without
/// Tauri state.
///
/// `params_b` (total parameters, in billions) resolves from the
/// caller's explicit value when positive, else from the curated
/// catalog ([`manager_models::catalog_params_b`]: id match, then
/// exact source-URL match). When no source yields a count the add
/// fails: config validation requires a positive `params_b`, and the
/// value feeds memory-fit estimation that nothing re-derives later,
/// so storing a placeholder would silently corrupt fit decisions.
fn model_add_entry(
    id: &str,
    source_url: &str,
    file_name: Option<String>,
    quant: Option<String>,
    params_b: Option<f32>,
    moe_layers: Option<u32>,
    default_offload_layers: Option<u32>,
) -> Result<manager_config::ModelConfig, ApiError> {
    let id = id.trim().to_string();
    if id.is_empty() {
        return Err(ApiError::InvalidInput(
            "model id is required".to_string(),
        ));
    }
    if !source_url.starts_with("https://") {
        return Err(ApiError::InvalidInput(
            "source URL must be an https:// URL for a GGUF file".to_string(),
        ));
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
        return Err(ApiError::InvalidInput(
            "file name must be a plain .gguf file name".to_string(),
        ));
    }
    manager_config::validate_moe_declaration(moe_layers, default_offload_layers)
        .map_err(ApiError::InvalidInput)?;
    let params_b = params_b
        .filter(|p| *p > 0.0)
        .or_else(|| manager_models::catalog_params_b(&id, source_url))
        .ok_or_else(|| {
            ApiError::InvalidInput(format!(
                "parameter count (params_b) is unknown for model '{id}': no value was supplied and the model is not in the curated catalog — supply the model's total parameter count in billions (e.g. 80 for an 80B model)"
            ))
        })?;
    Ok(manager_config::ModelConfig {
        gguf_path: PathBuf::from("models").join(&file_name),
        id,
        quant: quant
            .map(|q| q.trim().to_string())
            .filter(|q| !q.is_empty())
            .unwrap_or_else(|| "unknown".to_string()),
        params_b,
        sha256: String::new(),
        verified_at: None,
        verified_sha256: None,
        source_url: source_url.to_string(),
        assigned_backends: Vec::new(),
        enabled: true,
        display_name: None,
        moe_layers,
        default_offload_layers,
    })
}

/// One raw HTTP GET against the gateway, returning (status, body)
/// — the transport for the gateway-exposure MCP test below.
/// src-tauri has no HTTP-client dependency; `fetch_gateway_metrics`
/// is the in-repo precedent for hand-rolled gateway HTTP over a
/// TcpStream. `None` on any transport failure (gateway down,
/// malformed reply, chunked body — this helper's conservative
/// reading; the metrics fetcher de-chunks instead, see
/// [`decode_metrics_response`]).
fn gateway_http_get(host: &str, port: u16, path: &str, master_key: &str) -> Option<(u16, String)> {
    use std::io::{Read, Write};
    let ip: std::net::IpAddr = host.parse().ok()?;
    let addr = std::net::SocketAddr::new(ip, port);
    let mut stream =
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(800)).ok()?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(2)));
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {master_key}\r\nConnection: close\r\n\r\n"
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
    let status: u16 = head
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        return None;
    }
    Some((status, parts.next()?.to_string()))
}

/// One raw HTTP POST against the gateway, returning (status, body)
/// — the POST sibling of [`gateway_http_get`], used only by the
/// explicit MCP test's real tool call below. Same conservative
/// reading (chunked bodies are rejected; these JSON routes answer
/// with a content length — unlike /metrics, which streams chunked
/// and is de-chunked by [`metrics_http_get`]). The read timeout is
/// longer than the GET helper's:
/// a cold stdio server spawn plus a live Brave query legitimately
/// takes seconds.
fn gateway_http_post(
    host: &str,
    port: u16,
    path: &str,
    master_key: &str,
    body: &str,
) -> Option<(u16, String)> {
    use std::io::{Read, Write};
    let ip: std::net::IpAddr = host.parse().ok()?;
    let addr = std::net::SocketAddr::new(ip, port);
    let mut stream =
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(800)).ok()?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {master_key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
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
    let status: u16 = head
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        return None;
    }
    Some((status, parts.next()?.to_string()))
}

/// One `GET` against the gateway over a raw socket. The metrics
/// fetcher is hand-rolled to stay dependency-free, so it must handle
/// response parsing — and redirects (see `fetch_gateway_metrics`) —
/// itself. None on any transport or parse failure.
fn metrics_http_get(
    host: &str,
    port: u16,
    path: &str,
    master_key: &str,
) -> Option<MetricsHttpResponse> {
    use std::io::{Read, Write};
    let ip: std::net::IpAddr = host.parse().ok()?;
    let addr = std::net::SocketAddr::new(ip, port);
    let mut stream =
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(800)).ok()?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(2)));
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {master_key}\r\nConnection: close\r\n\r\n"
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
    decode_metrics_response(&raw)
}

/// Decode one raw HTTP response (status line, headers, body) read
/// off the gateway socket into status, `Location`, and the message
/// body with any chunked transfer encoding removed.
///
/// Strict on purpose: a chunked body must carry well-formed chunk
/// framing through its terminating zero chunk, and a
/// content-length body must be complete. A truncated or malformed
/// response is None, never a partial read — a partial exposition
/// parses as smaller counters, and the window math would read the
/// drop as a gateway restart and overcount (#58).
fn decode_metrics_response(raw: &[u8]) -> Option<MetricsHttpResponse> {
    let head_end = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
    let status: u16 = head.split_whitespace().nth(1)?.parse().ok()?;
    let header = |name: &str| -> Option<String> {
        head.lines().skip(1).find_map(|line| {
            let (k, v) = line.split_once(':')?;
            if k.trim().eq_ignore_ascii_case(name) {
                Some(v.trim().to_string())
            } else {
                None
            }
        })
    };
    let body_bytes = &raw[head_end + 4..];
    let decoded = if header("transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false)
    {
        dechunk(body_bytes)?
    } else if let Some(len) = header("content-length") {
        let len: usize = len.parse().ok()?;
        if body_bytes.len() < len {
            return None;
        }
        body_bytes[..len].to_vec()
    } else {
        // Close-delimited body (the request says Connection: close).
        body_bytes.to_vec()
    };
    Some(MetricsHttpResponse {
        status,
        location: header("location"),
        body: String::from_utf8(decoded).ok()?,
    })
}

/// Remove HTTP chunked transfer framing: repeated
/// `<hex size>[;extensions]\r\n<size bytes>\r\n`, ended by a zero
/// chunk (trailers may follow and are ignored). None on any
/// malformation — bad hex, a short chunk, a missing CRLF, or no
/// terminating zero chunk (i.e. the stream was cut short).
fn dechunk(mut rest: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = rest.windows(2).position(|w| w == b"\r\n")?;
        let line = std::str::from_utf8(&rest[..line_end]).ok()?;
        let size = usize::from_str_radix(line.split(';').next()?.trim(), 16).ok()?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        if rest.len() < size + 2 || &rest[size..size + 2] != b"\r\n" {
            return None;
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

/// Resolve a redirect `Location` against the path just requested,
/// returning the next path (with query) to GET on the same gateway
/// origin. Handles absolute URLs (only the path is kept — the
/// fetcher never leaves the origin it dialed), absolute paths, and
/// relative references.
fn metrics_redirect_path(current_path: &str, location: &str) -> Option<String> {
    let location = location.trim();
    if location.is_empty() {
        return None;
    }
    if let Some(rest) = location
        .strip_prefix("http://")
        .or_else(|| location.strip_prefix("https://"))
    {
        return Some(match rest.find('/') {
            Some(i) => rest[i..].to_string(),
            None => "/".to_string(),
        });
    }
    if location.starts_with('/') {
        return Some(location.to_string());
    }
    // Relative reference: resolve against the current path's
    // directory.
    let base = current_path.split('?').next().unwrap_or(current_path);
    let dir = base.rfind('/').map(|i| &base[..=i]).unwrap_or("/");
    Some(format!("{dir}{location}"))
}

/// Mark `id` as probing and spawn its background probe task.
fn spawn_offload_probe(
    sink: &Arc<dyn EventSink>,
    state: &Arc<AppState>,
    id: &str,
    layers: u32,
    moe_layers: u32,
    port: u16,
) {
    {
        let mut probes = state
            .offload_probes
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        probes.insert(
            id.to_string(),
            OffloadProbe {
                state: "probing".to_string(),
                layers,
                detail: None,
            },
        );
    }
    let probe_sink = Arc::clone(sink);
    let probe_state = Arc::clone(state);
    let probe_id = id.to_string();
    tokio::spawn(async move {
        run_offload_probe(probe_sink, probe_state, probe_id, layers, moe_layers, port).await;
    });
}

/// Record a failed offload probe and toast it. The detail names the
/// position and the verified alternative, or the request error —
/// never a claim that any position is certified.
async fn fail_offload_probe(
    sink: &dyn EventSink,
    state: &AppState,
    backend_id: &str,
    layers: u32,
    detail: String,
) {
    {
        let mut probes = state
            .offload_probes
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        probes.insert(
            backend_id.to_string(),
            OffloadProbe {
                state: "failed".to_string(),
                layers,
                detail: Some(detail.clone()),
            },
        );
    }
    notify::notify(
        sink,
        state,
        notify::Category::Failures,
        "Offload probe failed",
        &detail,
    )
    .await;
}

/// Timing knobs for the offload probe's HTTP sequence. Production
/// values come from [`OffloadProbeTiming::production`]; tests shrink
/// them so the sequence runs in milliseconds.
#[derive(Debug, Clone, Copy)]
struct OffloadProbeTiming {
    /// How long to wait for the backend to report ready (GET
    /// /health 2xx) before concluding the probe cannot run.
    ready_budget: Duration,
    /// Delay between readiness polls.
    ready_poll: Duration,
    /// After readiness, how long transient probe-request failures
    /// (HTTP 503, transport errors) are retried before the failure
    /// is reported as terminal.
    retry_window: Duration,
    /// Delay between probe-request retries.
    retry_poll: Duration,
}

impl OffloadProbeTiming {
    fn production() -> Self {
        Self {
            ready_budget: Duration::from_secs(180),
            ready_poll: Duration::from_secs(2),
            retry_window: Duration::from_secs(60),
            retry_poll: Duration::from_secs(3),
        }
    }
}

/// What the offload probe's HTTP sequence concluded. Process death
/// is detected through the alive check, never inferred from HTTP
/// traffic, so a crashed backend is never reclassified as "not
/// ready".
#[derive(Debug, Clone, PartialEq, Eq)]
enum OffloadProbeHttpOutcome {
    /// The backend reported ready and the probe request succeeded.
    Passed,
    /// The supervised process exited (or vanished) at some point
    /// during the sequence.
    BackendStopped,
    /// The backend never reported ready within the readiness
    /// budget, so the probe request was never sent. This is not
    /// evidence about the offload position.
    NotReady,
    /// The backend reported ready and the probe request then
    /// failed terminally: a non-transient HTTP status, or a
    /// transient failure that persisted past the retry window.
    /// Carries the failure text (e.g. "HTTP 500").
    FailedAfterReady(String),
}

/// Liveness check consulted between readiness polls and probe
/// attempts: true while the backend's process is alive.
type OffloadProbeAlive<'a> = dyn FnMut() -> std::pin::Pin<
        Box<dyn std::future::Future<Output = bool> + Send + 'a>,
    > + Send
    + 'a;

/// The offload probe's HTTP sequence, split from
/// [`run_offload_probe`] so tests can drive it against a stub
/// backend: wait for the app's readiness signal (GET /health 2xx —
/// the definition the supervisor's health probe and the tool-call
/// probe both use), then send the long probe prompt, retrying
/// transient failures for a bounded window.
///
/// The readiness gate must NOT be a chat request: `test_request`
/// returns Ok for any JSON-parseable response regardless of status,
/// and llama-server answers chat completions with HTTP 503 (with a
/// JSON error body) the whole time the model is loading. A
/// status-blind gate therefore passes the moment the listener is
/// up — minutes before the backend can serve — and the probe then
/// fails on that same 503. That was the v0.1.38 field failure
/// behind tracker #46.
async fn offload_probe_http_sequence(
    base_url: &str,
    timing: OffloadProbeTiming,
    alive: &mut OffloadProbeAlive<'_>,
) -> OffloadProbeHttpOutcome {
    // Readiness phase: poll GET /health until it reports 2xx.
    let health = HttpHealthProbe::new(
        format!("{}/health", base_url.trim_end_matches('/')),
        Duration::from_secs(2),
    );
    let ready_deadline = std::time::Instant::now() + timing.ready_budget;
    loop {
        if !alive().await {
            return OffloadProbeHttpOutcome::BackendStopped;
        }
        if matches!(
            health.probe().await,
            Ok(manager_supervisor::HealthStatus::Healthy)
        ) {
            break;
        }
        if std::time::Instant::now() >= ready_deadline {
            return OffloadProbeHttpOutcome::NotReady;
        }
        tokio::time::sleep(timing.ready_poll).await;
    }
    // Probe phase: one long prompt (about 4,000 tokens of prompt
    // processing, exercised across many 512-token ubatches) at the
    // newly activated offload position. HTTP 503 is llama-server's
    // not-ready/busy answer and a transport error can race the last
    // of the startup work, so both retry within the window; any
    // other failure — or a transient one that outlasts the window —
    // is terminal.
    let prompt = "The quick brown fox jumps over the lazy dog. ".repeat(450);
    let retry_deadline = std::time::Instant::now() + timing.retry_window;
    loop {
        let result = test_request(base_url, &prompt).await;
        // Process death outranks a request error: a dead backend's
        // last request also fails, and the stopped detail is the
        // actionable one.
        if !alive().await {
            return OffloadProbeHttpOutcome::BackendStopped;
        }
        let (failure, transient) = match result {
            Ok(r) if r.status < 400 => return OffloadProbeHttpOutcome::Passed,
            Ok(r) => (format!("HTTP {}", r.status), r.status == 503),
            Err(e) => (e.to_string(), true),
        };
        if !transient || std::time::Instant::now() >= retry_deadline {
            return OffloadProbeHttpOutcome::FailedAfterReady(failure);
        }
        tokio::time::sleep(timing.retry_poll).await;
    }
}

/// The background offload probe: wait for the backend to report
/// ready, then send one long prompt (about 4,000 tokens of prompt
/// processing, exercised across many 512-token ubatches) at the
/// newly activated offload position, confirming the process is
/// still alive throughout. A passed probe means only that the
/// probe completed.
async fn run_offload_probe(
    sink: Arc<dyn EventSink>,
    state: Arc<AppState>,
    backend_id: String,
    layers: u32,
    moe_layers: u32,
    port: u16,
) {
    let base_url = format!("http://127.0.0.1:{port}");
    let stopped_detail = format!(
        "the backend stopped during the offload probe at the {layers} of {moe_layers} layers offload position — this position is unverified on this GPU; full offload ({moe_layers} of {moe_layers} layers) is the verified position"
    );
    let timing = OffloadProbeTiming::production();
    let mut alive = || {
        let state = Arc::clone(&state);
        let backend_id = backend_id.clone();
        Box::pin(async move { offload_probe_backend_alive(&state, &backend_id).await })
            as std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
    };
    let outcome = offload_probe_http_sequence(&base_url, timing, &mut alive).await;
    match outcome {
        OffloadProbeHttpOutcome::Passed => {
            let mut probes = state
                .offload_probes
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            probes.insert(
                backend_id.clone(),
                OffloadProbe {
                    state: "passed".to_string(),
                    layers,
                    detail: None,
                },
            );
        }
        OffloadProbeHttpOutcome::BackendStopped => {
            fail_offload_probe(sink.as_ref(), &state, &backend_id, layers, stopped_detail).await;
        }
        OffloadProbeHttpOutcome::NotReady => {
            fail_offload_probe(
                sink.as_ref(),
                &state,
                &backend_id,
                layers,
                format!(
                    "the backend did not become ready within {}s, so the offload probe could not run — no conclusion about the {layers} of {moe_layers} layers offload position; this position is unverified on this GPU; full offload ({moe_layers} of {moe_layers} layers) is the verified position",
                    timing.ready_budget.as_secs()
                ),
            )
            .await;
        }
        OffloadProbeHttpOutcome::FailedAfterReady(failure) => {
            fail_offload_probe(
                sink.as_ref(),
                &state,
                &backend_id,
                layers,
                format!(
                    "the offload probe request failed at the {layers} of {moe_layers} layers offload position: {failure}"
                ),
            )
            .await;
        }
    }
}
