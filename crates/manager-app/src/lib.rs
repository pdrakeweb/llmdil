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
// Glob-imported by the test modules (`use super::*`); the lib itself
// keeps its atomics inside the `state` subsystems.
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use manager_config::{
    self, AppConfig, BackendPatch, ComponentUpdate, ConfigDiff, GatewayConfig, MxcMode,
};
use manager_models::{self};
use manager_mxc::{self, Policy};
use manager_supervisor::{
    test_request, test_tool_call, BackoffPolicy, HealthProbe, HttpHealthProbe, LogLevelFilter,
    LogRecord, LogStream, ProcessSpec, ProcessState, Supervisor,
};
use manager_telemetry::{
    DriverModel, GpuInfo, GpuSample, NvidiaSmiSource, PollCadence, Poller, TelemetryStore,
};
use manager_wizard::{
    ops::{prev_log_path, RealSystemOps},
    steps::{continue_config_yaml, merge_env_contents, VscodeExtensions},
    RunMode, StepOutcome, StepState, SystemOps, Wizard, WizardContext, WizardReport,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::{Mutex, OnceCell, RwLock};

pub mod activation;
pub mod activation_apply;
pub mod activation_cancel;
pub mod activation_history;
pub mod app_update;
pub mod backend_lifecycle;
pub mod bench;
mod bench_client;
#[cfg(test)]
mod gateway_ops_tests;
pub mod gateway_stats;
#[cfg(test)]
mod golden_tests;
pub mod gpu_history;
pub mod hf_forms;
pub mod lifecycle;
pub mod lifecycle_monitor;
pub mod log_receiver;
pub mod logging;
pub mod logship;
mod mcp_local;
pub mod notify;
pub mod openrouter_spend;
pub mod optimize;
pub mod optimize_apply;
pub mod optimize_eval;
pub mod optimize_experiments;
mod optimize_stream;
pub mod optimize_tuning;
pub mod single_instance;

pub use activation_apply::op_activation_apply;
pub use app_update::{
    op_app_update_check, op_app_update_download, op_app_update_restart, op_app_update_set_auto,
    op_app_update_set_checking, op_app_update_status,
};
pub use bench::{
    op_bench_prompt_delete, op_bench_prompt_generate, op_bench_prompt_generate_cancel,
    op_bench_prompt_save, op_bench_prompts_list, op_bench_result_delete, op_bench_results_list,
    op_bench_run_cancel, op_bench_run_start,
};
pub use gpu_history::op_get_gpu_history;
pub use hf_forms::{op_model_hf_header, op_model_hf_lookup};
pub use log_receiver::{
    op_log_receiver_deploy, op_log_receiver_flip_endpoint, op_log_receiver_remove,
    op_log_receiver_restart, op_log_receiver_start, op_log_receiver_status, op_log_receiver_stop,
};
pub use optimize::{
    op_measurement_delete, op_measurement_lookup, op_optimize_cancel_consult,
    op_optimize_cancel_job, op_optimize_consult, op_optimize_discard_plan, op_optimize_discover,
    op_optimize_estimate_backend, op_optimize_get_state, op_optimize_measure_backend,
    op_optimize_save_purpose, op_optimize_set_item_outcome, op_optimize_set_item_state,
    op_optimize_validate_plan,
};
pub use optimize_apply::op_optimize_apply_plan;
pub use optimize_eval::op_optimize_run_baseline;
pub use optimize_experiments::{
    op_optimize_estimate_experiments, op_optimize_run_experiments, op_optimize_set_experiment_state,
};
pub use optimize_tuning::{
    op_optimize_tuning_adopt, op_optimize_tuning_preview, op_optimize_tuning_run,
};
pub mod event_bus;
pub mod registry;
pub(crate) mod tasks;
pub mod updates;

mod ops;
mod precheck;
mod state;
#[cfg(test)]
mod test_support;

pub use ops::backend_config::*;
pub(crate) use ops::backend_info::*;
pub(crate) use ops::backend_spec::*;
pub use ops::backend_tuning::*;
pub use ops::backends::*;
pub use ops::config::*;
pub use ops::gateway::*;
pub use ops::gateway_activation::*;
pub use ops::logs::*;
pub use ops::mcp::*;
pub use ops::models::*;
pub use ops::offload::*;
pub use ops::placement::*;
pub use ops::platform::*;
pub(crate) use ops::preflight::*;
pub use ops::services::*;
pub use ops::telemetry::*;
pub use ops::wizard::*;
pub use precheck::{precheck_operation, Refusal};
pub use state::*;
pub use tasks::{wait_task_settled, with_task_link, TaskLink};

// ---------------------------------------------------------------------------
// Events (Rust -> UI, spec §14)
// ---------------------------------------------------------------------------

/// Frontend event names. Every name has a producer; [`event_bus`]
/// carries them all (with their origin) to Admin API subscribers.
/// SERVICE_LIFECYCLE (and, for backends, BACKEND_STATUS) is emitted by
/// the shared lifecycle helpers ([`lifecycle::emit`]).
pub mod events {
    pub const TELEMETRY_UPDATE: &str = "telemetry-update";
    pub const WIZARD_PROGRESS: &str = "wizard-progress";
    pub const DOWNLOAD_PROGRESS: &str = "download-progress";
    // Backend start/stop/restart: `{id, event, state, origin, error?}`.
    // Emitted beside SERVICE_LIFECYCLE for backend keys (compatibility
    // and log shipping).
    pub const BACKEND_STATUS: &str = "backend-status";
    /// Every lifecycle transition of every managed service (gateway,
    /// postgres, log-receiver, backend:<id>): emitted when an operation
    /// begins and ends and on every monitor-observed change. Payload
    /// `{key, kind, id?, event, state, origin, detail?, at}`, where
    /// `event` is requested | starting | stopping | restarting | ready |
    /// started | stopped | restarted | failed | crashed | adopted and
    /// `state` is the shared `LifecycleState` wire string.
    pub const SERVICE_LIFECYCLE: &str = "service-lifecycle";
    // Produced for the app event bus (see `event_bus`): log-line,
    // telemetry-tick and vram-changed by its background producers
    // (only while subscribed), task-progress by the task registry,
    // alert-raised by the notification monitor, gateway-request and
    // config-changed after a successful dispatch.
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
    /// Another lifecycle operation holds the service (e.g. "gateway
    /// restart already in progress (started by activation 12 s ago)").
    /// REST maps it to 409 / `BUSY`.
    Busy(String),
}

impl ApiError {
    /// The variant's name: the machine-readable error kind.
    pub fn kind(&self) -> &'static str {
        match self {
            ApiError::Config(_) => "Config",
            ApiError::Supervisor(_) => "Supervisor",
            ApiError::Wizard(_) => "Wizard",
            ApiError::Telemetry(_) => "Telemetry",
            ApiError::Gateway(_) => "Gateway",
            ApiError::Models(_) => "Models",
            ApiError::Mxc(_) => "Mxc",
            ApiError::Mcp(_) => "Mcp",
            ApiError::WinMl(_) => "WinMl",
            ApiError::Services(_) => "Services",
            ApiError::NotFound(_) => "NotFound",
            ApiError::InvalidInput(_) => "InvalidInput",
            ApiError::ContractGap(_) => "ContractGap",
            ApiError::Unsupported(_) => "Unsupported",
            ApiError::Io(_) => "Io",
            ApiError::Json(_) => "Json",
            ApiError::Busy(_) => "Busy",
        }
    }
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
            ApiError::Busy(e) => write!(f, "{e}"),
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
    /// When the work began (a task starts when it is recorded).
    #[serde(default)]
    pub started_ms: u64,
    /// When the task reached its terminal state.
    #[serde(default)]
    pub finished_ms: Option<u64>,
    /// The error text of a failed task (also in `detail.error`).
    #[serde(default)]
    pub error: Option<String>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
