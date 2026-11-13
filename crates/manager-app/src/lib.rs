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
    test_request, test_tool_call, BackoffPolicy, HealthProbe, HttpHealthProbe, LogLevelFilter,
    LogRecord, LogStream, ProcessSpec, ProcessState, Supervisor,
};
use manager_telemetry::{
    DriverModel, GpuInfo, GpuSample, NvidiaSmiSource, PollCadence, Poller, TelemetryStore,
};
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
pub mod activation_apply;
pub mod activation_cancel;
pub mod activation_history;
pub mod app_update;
pub mod backend_lifecycle;
pub mod bench;
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
mod state;
#[cfg(test)]
mod test_support;

pub use ops::config::*;
pub use ops::gateway::*;
pub use ops::gateway_activation::*;
pub use ops::logs::*;
pub use ops::mcp::*;
pub use ops::models::*;
pub use ops::platform::*;
pub use ops::services::*;
pub use ops::telemetry::*;
pub use ops::wizard::*;
pub use state::*;

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
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

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
    let owned = manager_models::split_raw_flags(raw);
    let tokens: Vec<&str> = owned.iter().map(String::as_str).collect();
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
    let owned = manager_models::split_raw_flags(raw);
    let tokens: Vec<&str> = owned.iter().map(String::as_str).collect();
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
        // Tokenized like a shell would for the operator's quotes —
        // `'…'`/`"…"` group and are removed — but with every
        // backslash literal (regexes in `-ot` need them); there is
        // no shell, so whitespace-splitting alone handed
        // llama-server literal quote characters
        // (see manager_models::split_raw_flags for the full rule).
        let owned = manager_models::split_raw_flags(raw);
        let tokens: Vec<&str> = owned.iter().map(String::as_str).collect();
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
        // llama-server backends spawn below normal priority: a
        // backend thrashing memory must yield the operator's
        // desktop session (Part B of the start-preflight build).
        low_priority: true,
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
///
/// Never adopted (SCM-3): a backend whose Windows service owns it (the
/// service is registered and live, or registered in service mode — its
/// llama-server is the service host's child, and adopting it would turn
/// an SCM stop into a phantom crash), and a backend with a lifecycle
/// operation in flight (its old process may still be draining on the
/// port). Each adoption writes the backend's "adopted" marker and
/// records intent Running and readiness (an adopted process is already
/// serving). Returns the adoptions as `(id, pid, port)` so callers that
/// hold an event sink can emit them ([`emit_adopted`]). Reaping exits
/// is the monitor's job, not this function's.
async fn adopt_orphan_backends(state: &AppState) -> Vec<(String, u32, u16)> {
    ensure_backend_specs(state).await;
    let (ports, service_mode): (Vec<(String, u16)>, bool) = {
        let cfg = state.config.read().await;
        (
            cfg.backends
                .iter()
                .map(|b| (b.id.clone(), b.port))
                .collect(),
            cfg.supervision.mode == manager_config::SupervisionMode::Service,
        )
    };
    let live: std::collections::HashSet<String> = {
        let sup = state.supervisor().await.lock().await;
        sup.status()
            .into_iter()
            .filter(|s| s.pid.is_some())
            .map(|s| s.id)
            .collect()
    };
    let scm = lifecycle::scm_snapshot(state, Duration::from_secs(5)).await;
    let mut candidates: Vec<(String, u32, u16)> = Vec::new();
    for (id, port) in ports {
        if live.contains(&id) {
            continue;
        }
        let key = lifecycle::backend_key(&id);
        if state.lifecycle.transition(&key).is_some() {
            continue;
        }
        if let Some(s) = scm.get(&manager_services::backend_service_name(&id)) {
            if lifecycle::registered(s) && (service_mode || lifecycle::scm_live(s)) {
                continue;
            }
        }
        if let Some(pid) = manager_supervisor::port_owner_pid(port) {
            if manager_supervisor::process_image_is(pid, "llama-server.exe") {
                candidates.push((id, pid, port));
            }
        }
    }
    if candidates.is_empty() {
        return Vec::new();
    }
    let mut adopted = Vec::new();
    {
        let mut sup = state.supervisor().await.lock().await;
        for (id, pid, port) in candidates {
            if sup.adopt(&id, pid).is_ok() {
                adopted.push((id, pid, port));
            }
        }
    }
    for (id, pid, port) in &adopted {
        let key = lifecycle::backend_key(id);
        state.lifecycle.set_intent(&key, lifecycle::Intent::Running);
        state.lifecycle.set_ready(&key, *pid);
        lifecycle::mark(
            &state.data_dir,
            &key,
            "adopted",
            Some(&format!(
                "pid {pid} on port {port} (started by a previous session)"
            )),
            Some(lifecycle::Origin::Startup),
        );
    }
    adopted
}

/// Emit the `adopted` lifecycle event for each adoption
/// [`adopt_orphan_backends`] made.
fn emit_adopted(sink: &dyn EventSink, adopted: &[(String, u32, u16)]) {
    for (id, pid, port) in adopted {
        lifecycle::emit(
            sink,
            &lifecycle::backend_key(id),
            "adopted",
            lifecycle::LifecycleState::Running,
            Some(lifecycle::Origin::Startup),
            Some(&format!("pid {pid} on port {port}")),
        );
    }
}

/// Wire string of a supervisor state: the shared lifecycle vocabulary's
/// spelling (`LifecycleState::as_str`). Rows read their state from the
/// liveness view now; kept for the vocabulary test.
#[cfg(test)]
fn process_state_str(s: ProcessState) -> &'static str {
    lifecycle::LifecycleState::from(s).as_str()
}

/// Gates a start/restart: the backend must be configured and enabled,
/// its model must not be mid-download or mid-verification, must not
/// carry a recorded SHA-256 verification failure, and — when it has a
/// real pinned hash — must have passed verification at least once
/// (`verified_at` set). The llama-server binary and model file must
/// exist. Errors name the missing piece and the fix, so a first Start
/// after a skipped wizard step explains itself.
async fn backend_start_preflight(
    state: &AppState,
    id: &str,
    allow_wddm_overcommit: bool,
) -> Result<(), ApiError> {
    let cfg = state.config.read().await.clone();
    let b = cfg
        .backends
        .iter()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    backend_start_preflight_for(state, &cfg, b, allow_wddm_overcommit).await
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
    allow_wddm_overcommit: bool,
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
        if state
            .verifying_downloads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&m.id)
        {
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
    vram_start_gate(state, cfg, b, allow_wddm_overcommit).await
}

/// One placed GPU's measured memory state, as the start gate sees
/// it: a single telemetry snapshot taken at start time.
struct VramGateReading {
    index: u32,
    total_mib: u64,
    free_mib: u64,
    /// The GPU was absent from the latest successful telemetry
    /// poll after having been reported before: the store retains
    /// it flagged down with last-known readings. A down GPU is
    /// not a full GPU — it is a device that stopped responding.
    down: bool,
    /// The GPU's Windows driver model from the same telemetry
    /// snapshot, when reported: WDDM GPUs can page an over-total
    /// requirement to system RAM; TCC GPUs cannot (Pete, Oct 23,
    /// 2026). `None` (unreported) is judged as hard-bounded.
    driver_model: Option<DriverModel>,
}

/// Why the start gate refuses one GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VramGateClass {
    /// The requirement exceeds the card's total memory: no state
    /// of the machine lets this configuration start on this GPU
    /// at this context size.
    ExceedsTotal,
    /// The requirement fits the card but not its measured free
    /// memory.
    InsufficientFree,
    /// The device itself appears offline or not responding: it
    /// is flagged down in telemetry, missing from a snapshot
    /// that reads other GPUs, or reporting zero total memory.
    /// Distinct from a memory shortage — the remedy is the GPU,
    /// not the configuration.
    GpuUnavailable,
}

/// One GPU on which a backend cannot start.
struct VramGateFailure {
    index: u32,
    class: VramGateClass,
    required_mib: u64,
    /// The free MiB the gate compared against: measured free plus
    /// the starting backend's own credited residency when it is
    /// already running (a restart frees its current weights before
    /// the new spawn allocates, so its own hold is not a shortage).
    /// Zero for a GPU with no usable reading.
    free_mib: u64,
    total_mib: u64,
    /// The failing GPU's driver model, when telemetry reported
    /// one: an [`VramGateClass::ExceedsTotal`] failure on a WDDM
    /// GPU is confirmable (see [`failures_are_wddm_overcommit`]).
    driver_model: Option<DriverModel>,
    /// MiB that will page to system RAM: the over-total excess
    /// on a WDDM GPU; 0 otherwise.
    paged_mib: u64,
}

/// Whether every failure is an over-total excess on a WDDM GPU —
/// the one failure shape an operator confirmation can allow:
/// WDDM overcommits VRAM by paging the excess to system RAM,
/// while TCC allocations are hard-bounded by physical VRAM
/// (Pete, Oct 23, 2026). Any other failure (a TCC or unreported
/// GPU over total, insufficient free memory under the total, an
/// unavailable GPU) makes the set non-confirmable as a whole.
fn failures_are_wddm_overcommit(failures: &[VramGateFailure]) -> bool {
    !failures.is_empty()
        && failures.iter().all(|f| {
            f.class == VramGateClass::ExceedsTotal && f.driver_model == Some(DriverModel::Wddm)
        })
}

/// The requirement's itemized breakdown in the gate's refusal
/// text — weights, KV cache, state cache, expert cache — so the
/// numbers a refusal cites are the numbers the fit surfaces
/// itemize (the expert cache included, when one is set). Terms
/// with no figure are omitted, never zero-filled.
fn requirement_breakdown(req: &manager_models::MemoryRequirement) -> String {
    let mut terms: Vec<String> = vec![format!("weights {} MiB", req.gpu_weights_mib)];
    if let Some(kv) = req.kv_mib {
        terms.push(format!("KV cache {kv} MiB"));
    }
    if let Some(rs) = req.rs_mib {
        terms.push(format!("state cache {rs} MiB"));
    }
    if req.moe_cache_mib > 0 {
        terms.push(format!("expert cache {} MiB", req.moe_cache_mib));
    }
    terms.join(" + ")
}

/// The pure start-gate math. Every placed GPU is judged first
/// for availability: a GPU flagged down, reporting zero total
/// memory, or missing from a non-empty reading set is
/// unavailable — no memory figure from it can be trusted,
/// whatever the requirement. (An empty reading set means
/// telemetry has nothing at all; the caller does not invoke the
/// gate then, and this function judges nothing either.) A
/// measurable GPU then fails on its requirement share exceeding
/// its effective free memory (measured free plus any credited
/// residency for that GPU); a zero requirement never fails the
/// memory math.
fn vram_gate_failures(
    placed: &[u32],
    required: &[(u32, u64)],
    readings: &[VramGateReading],
    credit: &[(u32, u64)],
) -> Vec<VramGateFailure> {
    let mut out = Vec::new();
    for &index in placed {
        let req = required
            .iter()
            .find(|(i, _)| *i == index)
            .map(|(_, v)| *v)
            .unwrap_or(0);
        let reading = readings.iter().find(|r| r.index == index);
        let unavailable = match reading {
            Some(r) => r.down || r.total_mib == 0,
            // Missing from a snapshot that reads other GPUs:
            // unavailable. Missing because telemetry has nothing
            // at all: not judged (the skip posture).
            None => !readings.is_empty(),
        };
        if unavailable {
            out.push(VramGateFailure {
                index,
                class: VramGateClass::GpuUnavailable,
                required_mib: req,
                free_mib: 0,
                total_mib: reading.map(|r| r.total_mib).unwrap_or(0),
                driver_model: reading.and_then(|r| r.driver_model),
                paged_mib: 0,
            });
            continue;
        }
        let Some(r) = reading else {
            continue;
        };
        if req == 0 {
            continue;
        }
        let credited = credit
            .iter()
            .find(|(i, _)| *i == index)
            .map(|(_, v)| *v)
            .unwrap_or(0);
        let free = r.free_mib.saturating_add(credited);
        if req > free {
            let class = if req > r.total_mib {
                VramGateClass::ExceedsTotal
            } else {
                VramGateClass::InsufficientFree
            };
            out.push(VramGateFailure {
                index,
                class,
                required_mib: req,
                free_mib: free,
                total_mib: r.total_mib,
                driver_model: r.driver_model,
                paged_mib: if class == VramGateClass::ExceedsTotal
                    && r.driver_model == Some(DriverModel::Wddm)
                {
                    req - r.total_mib
                } else {
                    0
                },
            });
        }
    }
    out
}

/// Measured free-VRAM gate (planner-moe load-failure diagnosis,
/// Oct 20, 2026): refuse a start whose per-GPU memory requirement
/// — the same requirement the backend cards display
/// ([`backend_memory_requirement`]), distributed over the
/// placement by the configured tensor split
/// ([`manager_optimize::residency_per_gpu`], the distribution the
/// fit evaluator uses) — exceeds the free VRAM measured on a
/// placed GPU at start time. Before this gate the preflight read
/// no memory figures at all, so an oversized backend started and
/// burned ~10 minutes in llama.cpp's fit-retry loop before
/// failing. The check is a single snapshot; nothing is polled or
/// cached across starts.
///
/// Skips when nothing can be measured or judged: CPU-only or
/// undetermined placement, an unresolvable memory requirement,
/// or a wholly empty telemetry snapshot (the fit evaluator's
/// "no telemetry, no fit claim" posture). A placed GPU missing
/// from a snapshot that does read other GPUs is NOT a skip: the
/// device was enumerated before and is gone, or never answered —
/// the gate refuses it as unavailable (see [`VramGateClass`]).
async fn vram_start_gate(
    state: &AppState,
    cfg: &AppConfig,
    b: &manager_config::BackendConfig,
    allow_wddm_overcommit: bool,
) -> Result<(), ApiError> {
    let placed: Vec<u32> = match backend_gpu_visibility(b) {
        GpuVisibility::Gpus(indices) => indices,
        _ => return Ok(()),
    };
    let Some(req) = backend_memory_requirement(cfg, b) else {
        return Ok(());
    };
    // Telemetry snapshot first, supervisor status second; neither
    // lock is held while the other is taken (the order the other
    // preflight readers follow).
    let mut samples = {
        let store = state.telemetry().await;
        let snapshot = lock_telemetry(&store).snapshot();
        snapshot
    };
    samples.sort_by_key(|s| s.index);
    if samples.is_empty() {
        return Ok(());
    }
    // Per-GPU requirement shares, aligned to the snapshot order
    // (split fraction position ↔ snapshot position is the
    // placement projection's convention).
    let per_gpu = manager_optimize::residency_per_gpu(&req, &b.flags.tensor_split, samples.len());
    let share_of = |index: u32| -> u64 {
        samples
            .iter()
            .position(|s| s.index == index)
            .map(|pos| per_gpu[pos])
            .unwrap_or(0)
    };
    let required: Vec<(u32, u64)> = placed.iter().map(|&i| (i, share_of(i))).collect();
    // Availability is judged even when every share is zero (a
    // stale split distributes nothing, but the spawn still
    // selects the placed devices): the readings carry each
    // sample's down flag through for the gate math to classify.
    let readings: Vec<VramGateReading> = samples
        .iter()
        .map(|s| VramGateReading {
            index: s.index,
            total_mib: s.vram_total_mib,
            free_mib: s.free_vram_mib(),
            down: s.down,
            driver_model: s.driver_model,
        })
        .collect();
    let statuses: Vec<(String, ProcessState)> = {
        let sup = state.supervisor().await.lock().await;
        sup.status().into_iter().map(|s| (s.id, s.state)).collect()
    };
    let running: std::collections::HashSet<&str> = statuses
        .iter()
        .filter(|(_, st)| matches!(st, ProcessState::Running | ProcessState::Starting))
        .map(|(id, _)| id.as_str())
        .collect();
    // A restart's own current residency is credited back: the
    // running process frees its weights before the replacement
    // allocates. The credit basis is the configured split applied
    // to the current requirement — the placement projection's
    // credit basis, for the same reason (no launched-split record
    // exists separate from the configured one).
    let credit: Vec<(u32, u64)> = if running.contains(b.id.as_str()) {
        required.clone()
    } else {
        Vec::new()
    };
    let failures = vram_gate_failures(&placed, &required, &readings, &credit);
    if failures.is_empty() {
        return Ok(());
    }
    // WDDM overcommit (Pete, Oct 23, 2026): when every failure is
    // an over-total excess on a WDDM GPU, the start is allowed
    // once the operator has confirmed it (the card's two-click
    // Start) — the excess pages to system RAM. Without the
    // confirmation the gate refuses, naming the paged amount, so
    // an unconfirmed caller (REST, a stale UI) never starts a
    // paging backend silently. TCC and unreported GPUs never
    // reach this branch: their failures are not all-WDDM.
    if failures_are_wddm_overcommit(&failures) {
        if allow_wddm_overcommit {
            return Ok(());
        }
        let breakdown = requirement_breakdown(&req);
        let parts: Vec<String> = failures
            .iter()
            .map(|f| {
                format!(
                    "GPU {} needs {} MiB ({}), which exceeds the GPU's total memory ({} MiB; {} MiB free) at this context size — {} MiB will page to system RAM on this WDDM GPU",
                    f.index, f.required_mib, breakdown, f.total_mib, f.free_mib, f.paged_mib
                )
            })
            .collect();
        return Err(ApiError::InvalidInput(format!(
            "backend {} cannot start without confirmation: {} — confirm the start to page the excess to system RAM, or reduce the context size or the model footprint before starting",
            b.id,
            parts.join("; ")
        )));
    }
    // Name the running app-managed backends placed on a failing
    // GPU: the known consumers of the memory that is not free.
    let consumers_on = |index: u32| -> Vec<String> {
        cfg.backends
            .iter()
            .filter(|o| o.id != b.id && running.contains(o.id.as_str()))
            .filter(|o| {
                matches!(backend_gpu_visibility(o), GpuVisibility::Gpus(ref idxs) if idxs.contains(&index))
            })
            .map(|o| o.id.clone())
            .collect()
    };
    let mut parts: Vec<String> = Vec::new();
    for f in &failures {
        match f.class {
            VramGateClass::GpuUnavailable => {
                parts.push(format!(
                    "GPU {} appears offline or not responding — telemetry has no usable reading for it",
                    f.index
                ));
            }
            VramGateClass::ExceedsTotal => {
                parts.push(format!(
                    "GPU {} needs {} MiB ({}), which exceeds the GPU's total memory ({} MiB; {} MiB free) at this context size",
                    f.index,
                    f.required_mib,
                    requirement_breakdown(&req),
                    f.total_mib,
                    f.free_mib
                ));
            }
            VramGateClass::InsufficientFree => {
                let mut part = format!(
                    "GPU {} needs {} MiB but only {} MiB is free (of {} MiB) — insufficient free memory on the GPU",
                    f.index, f.required_mib, f.free_mib, f.total_mib
                );
                let consumers = consumers_on(f.index);
                if consumers.len() == 1 {
                    part += &format!("; backend '{}' is running on this GPU", consumers[0]);
                } else if !consumers.is_empty() {
                    let named: Vec<String> = consumers.iter().map(|c| format!("'{c}'")).collect();
                    part += &format!("; backends {} are running on this GPU", named.join(", "));
                }
                parts.push(part);
            }
        }
    }
    let any_unavailable = failures
        .iter()
        .any(|f| f.class == VramGateClass::GpuUnavailable);
    let remedy = if any_unavailable {
        "check the GPU (power, seating, driver) and try again; if the GPU is healthy, reduce the context size or the model footprint, or free the GPU, before starting"
    } else {
        "reduce the context size or the model footprint, or free the GPU, before starting"
    };
    Err(ApiError::InvalidInput(format!(
        "backend {} cannot start: {} — {}",
        b.id,
        parts.join("; "),
        remedy
    )))
}

/// All backends with their launch ports, models, and live supervisor
/// runtime state (process state, pid, restarts, uptime). Configured
/// backends carry `configured: true`; the list also appends
/// supervisor-only rows (`configured: false`, `enabled: false`) for
/// backend processes still live under ids that left the config, so they
/// stay visible and stoppable instead of silently holding their port.
/// MCP server processes share the Supervisor but are never listed here.
pub async fn op_list_backends(state: &Arc<AppState>, sink: &Arc<dyn EventSink>) -> ApiResult {
    let adopted = adopt_orphan_backends(state).await;
    emit_adopted(sink.as_ref(), &adopted);
    // Clone and drop the config guard here: the offload guardrail
    // block below needs the telemetry inventory and the activation
    // journal, and the telemetry accessor reads the config while
    // initializing — it must never be called while this command
    // holds the config guard (the rule ensure_backend_specs follows).
    let cfg = state.config.read().await.clone();
    // Every row's lifecycle fields come from the one shared liveness
    // view (supervisor + SCM + in-flight transitions + readiness), so
    // this list, the dashboard, the activation plan and bench/optimize
    // agree in both run modes. Reaping exits is the monitor's job.
    let liveness = lifecycle::backend_liveness(state).await;
    // Orphan (removed-but-live) rows need their spawn spec's port and
    // model: one short supervisor block, released before any other
    // await below (the telemetry, direct-slots and gateway-stats locks
    // are never taken under the supervisor lock here).
    let configured_ids: std::collections::HashSet<&str> =
        cfg.backends.iter().map(|b| b.id.as_str()).collect();
    let orphan_specs: HashMap<String, (u16, String)> = {
        let sup = state.supervisor().await.lock().await;
        liveness
            .keys()
            .filter(|id| !configured_ids.contains(id.as_str()))
            .filter_map(|id| {
                let spec = sup.spec(id)?;
                let model_file = spec
                    .args
                    .iter()
                    .position(|a| a == "-m")
                    .and_then(|i| spec.args.get(i + 1).cloned())
                    .unwrap_or_default();
                Some((id.clone(), (spec.port.unwrap_or(0), model_file)))
            })
            .collect()
    };
    let mut backends: Vec<Value> = cfg
        .backends
        .iter()
        .map(|b| {
            let mut row = json!({
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
                // The persisted tool-call probe (MCP design §7.2):
                // {outcome: verified|unparsed|failed, at_ms} or null
                // when the backend has never been probed — the UI
                // renders null as "not measured", never as a state.
                "tool_call_probe": serde_json::to_value(b.tool_call_probe).unwrap_or(Value::Null),
            });
            apply_liveness(&mut row, liveness.get(&b.id));
            row
        })
        .collect();
    // Memory requirements per configured backend (GPU-resident
    // weights, system-RAM weights under expert offload, KV at the
    // configured context, recurrent-state cache at the effective
    // slot count, and the derivation basis). Absent when
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
            row["rs_ram_mib"] = json!(req.rs_mib);
            // The expert-cache term of the requirement (0 when
            // unset): the card itemizes it beside KV/state so its
            // tiles carry the same components the start gate sums.
            row["moe_cache_mib"] = json!(req.moe_cache_mib);
            row["memory_basis"] = serde_json::to_value(req.basis).unwrap_or(Value::Null);
        }
    }
    // Three-state fit per configured backend, from the same
    // requirement model and per-GPU distribution the start gate
    // judges (residency_per_gpu over the telemetry inventory):
    // each placed GPU's required share against its total VRAM and
    // driver model. Absent when placement or inventory cannot
    // judge the backend — no fit claim then, never a default.
    {
        let inventory: Vec<GpuInfo> = {
            let store = state.telemetry().await;
            let inventory = lock_telemetry(&store).inventory();
            inventory
        };
        if !inventory.is_empty() {
            let n = inventory.len();
            for row in backends.iter_mut() {
                let Some(id) = row["id"].as_str().map(str::to_string) else {
                    continue;
                };
                let Some(b) = cfg.backends.iter().find(|b| b.id == id) else {
                    continue;
                };
                let placed: Vec<u32> = match backend_gpu_visibility(b) {
                    GpuVisibility::Gpus(indices) => indices,
                    _ => continue,
                };
                let Some(req) = backend_memory_requirement(&cfg, b) else {
                    continue;
                };
                let per_gpu = manager_optimize::residency_per_gpu(&req, &b.flags.tensor_split, n);
                let mut states: Vec<manager_models::FitState> = Vec::new();
                let mut per: Vec<Value> = Vec::new();
                for &index in &placed {
                    let Some(pos) = inventory.iter().position(|g| g.index == index) else {
                        continue;
                    };
                    let gpu = &inventory[pos];
                    let (st, paged) = manager_models::fit_state(
                        per_gpu[pos],
                        gpu.total_vram_mib,
                        gpu.driver_model == Some(DriverModel::Wddm),
                    );
                    states.push(st);
                    per.push(json!({
                        "gpu": index,
                        "required_mib": per_gpu[pos],
                        "total_mib": gpu.total_vram_mib,
                        "driver_model": gpu.driver_model.map(|d| match d {
                            DriverModel::Wddm => "wddm",
                            DriverModel::Tcc => "tcc",
                        }),
                        "state": st.as_str(),
                        "paged_mib": paged,
                    }));
                }
                if per.is_empty() {
                    continue;
                }
                let worst = manager_models::worst_fit_state(&states);
                let paged_total: u64 = per
                    .iter()
                    .map(|p| p["paged_mib"].as_u64().unwrap_or(0))
                    .sum();
                row["fit"] = json!({
                    "state": worst.as_str(),
                    "paged_mib": paged_total,
                    "per_gpu": per,
                });
            }
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
    // Expert-cache control state per offload-capable backend
    // (see backend_moe_cache_info): attaches only where the
    // offload slider's own source resolves, so the control
    // appears for precisely the backends that can offload.
    {
        let moe_cache: HashMap<String, MoeCacheInfo> = cfg
            .backends
            .iter()
            .filter_map(|b| backend_moe_cache_info(&cfg, b).map(|m| (b.id.clone(), m)))
            .collect();
        for row in backends.iter_mut() {
            let Some(id) = row["id"].as_str() else {
                continue;
            };
            let Some(m) = moe_cache.get(id) else { continue };
            row["moe_cache"] = json!({
                "mib": m.mib,
                "custom": m.custom,
                "offload_layers": m.offload_layers,
                "moe_layers": m.moe_layers,
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
    // KV cache / flash-attention state per configured backend:
    // the flags form's cache controls render for every backend,
    // so — like the parallel state — this attaches to every
    // configured row.
    {
        for row in backends.iter_mut() {
            let Some(id) = row["id"].as_str() else {
                continue;
            };
            let Some(b) = cfg.backends.iter().find(|b| b.id == id) else {
                continue;
            };
            row["cache"] = cache_row_json(&backend_cache_info(b));
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
                    idxs.iter()
                        .map(|i| i.to_string())
                        .collect::<Vec<_>>()
                        .join("+")
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
    // the store has a window for the backend, and the LIVE
    // throughput rate where the store measured one. Absent fields
    // render as "—" in the UI — no stats yet is not zero stats.
    // Throughput is the recent-window live rate, NOT the
    // trailing-hour tokens/window_secs the card carried before
    // Oct 22, 2026: that quotient read "Throughput 0 tok/s" while
    // a backend was actively generating (an hour's denominator
    // over seconds of counted tokens, and no counted tokens at
    // all while a request is still in flight — LiteLLM counts a
    // request's tokens only when it completes).
    {
        let pairs: Vec<(String, u16)> = cfg
            .backends
            .iter()
            .map(|b| (b.id.clone(), b.port))
            .collect();
        // Backends an Optimize operation is currently driving
        // DIRECTLY (consultation / measure / experiment traffic
        // goes to the backend's own port, never through the
        // gateway): the gateway sampler cannot see that
        // generation, so its rate for the backend is not a
        // measurement of the backend's current throughput —
        // omit the gateway fallback while the operation holds
        // the backend instead of reporting the gateway's idle
        // figure. The direct /slots rate below is NOT omitted:
        // it polls the backend itself, so it sees direct-drive
        // generation exactly as it sees gateway traffic.
        let direct_ops: std::collections::HashSet<String> = state
            .optimize_running_ops
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        // Direct /slots live rates first: the direct store is
        // read (and its lock released) before the gateway store
        // is locked, so this block never holds both locks in
        // the opposite order anywhere else.
        let direct_rates: std::collections::HashMap<String, Option<f64>> = {
            let direct = state.direct_slots.lock().await;
            pairs
                .iter()
                .map(|(id, _)| (id.clone(), direct.live_rate(id)))
                .collect()
        };
        let mut stats = state.gateway_stats.lock().await;
        let windows = stats.window_stats(&pairs);
        let series = stats.series_stats(&pairs);
        let live = stats.live_throughput(&pairs);
        for row in backends.iter_mut() {
            let Some(id) = row["id"].as_str().map(str::to_string) else {
                continue;
            };
            if let Some(w) = windows.as_ref().and_then(|m| m.get(&id)) {
                row["requests_1h"] = json!(w.requests);
                row["errors_1h"] = json!(w.errors);
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
            // Tile precedence (Nov 3, 2026 throughput fix):
            // the direct /slots rate when it was measured, the
            // gateway live rate as fallback, absent otherwise.
            // A missed token attribution leaves the field
            // absent — unknown is not zero (design principle
            // 12). During a direct-drive Optimize op the
            // gateway fallback is skipped (the gateway cannot
            // see that traffic), but the direct rate still
            // attaches: it polls the backend itself.
            let live_rate = match direct_rates.get(&id).copied().flatten() {
                Some(rate) => Some(rate),
                None if direct_ops.contains(&id) => None,
                None => live.as_ref().and_then(|m| m.get(&id)).and_then(|l| l.rate),
            };
            if let Some(rate) = live_rate {
                row["throughput_toks"] = json!((rate * 10.0).round() / 10.0);
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
    // Processes still live (or mid-operation) under ids that left the
    // config — their specs survive pruning while live: surface them so
    // they stay visible and stoppable instead of silently holding their
    // port.
    let mut orphan_ids: Vec<&String> = liveness
        .keys()
        .filter(|id| !configured_ids.contains(id.as_str()))
        .collect();
    orphan_ids.sort();
    for id in orphan_ids {
        let Some((port, model_file)) = orphan_specs.get(id) else {
            continue;
        };
        // The spec argv carries the data-dir-resolved absolute path;
        // configured rows show the config-relative path. Strip the
        // data-dir prefix so orphan rows display the same form.
        let model_file = match std::path::Path::new(model_file).strip_prefix(&cfg.data_dir) {
            Ok(rel) => rel.to_string_lossy().into_owned(),
            Err(_) => model_file.clone(),
        };
        let mut row = json!({
            "id": id,
            "enabled": false,
            "port": port,
            "model_file": model_file,
            "configured": false,
        });
        apply_liveness(&mut row, liveness.get(id));
        backends.push(row);
    }
    // Leftover backend services (SCM-18): a registered, live
    // `llm-backend-<id>` service whose id left the config keeps running
    // invisibly unless listed. Rows come from the definitions the app
    // wrote under <data_dir>/services/; they are stoppable (the stop
    // route reaches a live registered service in either mode).
    let listed: std::collections::HashSet<String> = backends
        .iter()
        .filter_map(|r| r["id"].as_str().map(str::to_string))
        .collect();
    for (id, port, model_file) in orphan_service_definitions(&state.data_dir, &listed) {
        let name = manager_services::backend_service_name(&id);
        let Some(status) = lifecycle::scm_status(state, &name, Duration::from_secs(1)).await else {
            continue;
        };
        if !lifecycle::registered(&status) || !lifecycle::scm_live(&status) {
            continue;
        }
        let key = lifecycle::backend_key(&id);
        let record = state.lifecycle.record(&key);
        let lifecycle_state = record
            .transition
            .as_ref()
            .map(|t| lifecycle::LifecycleState::from(t.kind))
            .unwrap_or_else(|| status.lifecycle_state());
        backends.push(json!({
            "id": id,
            "enabled": false,
            "port": port,
            "model_file": model_file,
            "configured": false,
            "service_managed": true,
            "state": lifecycle_state.as_str(),
            "lifecycle": lifecycle_state.as_str(),
            "pid": status.pid,
            "child_pid": status.child_pid,
            "adopted": false,
            "restarts": 0,
            "uptime_secs": Value::Null,
            "failure_reason": status.failure_reason(),
            "transition": transition_json(record.transition.as_ref()),
        }));
    }
    Ok(json!({ "backends": backends }))
}

/// The `transition` field of a backend row: `{kind, origin, since}` of
/// the operation in flight, or null.
fn transition_json(t: Option<&lifecycle::Transition>) -> Value {
    match t {
        Some(t) => json!({
            "kind": t.kind,
            "origin": t.origin,
            "since": t.since.to_rfc3339(),
        }),
        None => Value::Null,
    }
}

/// Write a backend row's lifecycle fields from the shared liveness view
/// (`state` and `lifecycle` carry the same wire string). A backend the
/// view does not know reads stopped.
fn apply_liveness(row: &mut Value, live: Option<&lifecycle::BackendLive>) {
    let state = live
        .map(|l| l.state)
        .unwrap_or(lifecycle::LifecycleState::Stopped);
    row["state"] = json!(state.as_str());
    row["lifecycle"] = json!(state.as_str());
    row["pid"] = json!(live.and_then(|l| l.pid));
    row["child_pid"] = json!(live.and_then(|l| l.child_pid));
    row["adopted"] = json!(live.is_some_and(|l| l.adopted));
    row["restarts"] = json!(live.map(|l| l.restarts).unwrap_or(0));
    row["uptime_secs"] = json!(live.and_then(|l| l.uptime_secs));
    // The recorded reason for the last spawn/load failure (e.g. the
    // device-unavailable fail-fast verdict, or the service's exit-code
    // reason), or null. The card's failed state surfaces it verbatim.
    row["failure_reason"] = json!(live.and_then(|l| l.failure_reason.clone()));
    row["service_managed"] = json!(live.is_some_and(|l| l.service_managed));
    row["transition"] = transition_json(live.and_then(|l| l.transition.as_ref()));
}

/// Backend service definitions under `<data_dir>/services/` whose id is
/// not in `listed`: `(id, port, model_file)` from each definition's
/// argv (`--port`, `-m`).
fn orphan_service_definitions(
    data_dir: &std::path::Path,
    listed: &std::collections::HashSet<String>,
) -> Vec<(String, u16, String)> {
    let prefix = manager_services::backend_service_name("");
    let Ok(entries) = std::fs::read_dir(manager_services::services_dir(data_dir)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(id) = stem.strip_prefix(&prefix) else {
            continue;
        };
        if id.is_empty() || listed.contains(id) {
            continue;
        }
        let Ok(def) = manager_services::read_definition(&path) else {
            continue;
        };
        let arg_after = |flag: &str| {
            def.args
                .iter()
                .position(|a| a == flag)
                .and_then(|i| def.args.get(i + 1).cloned())
        };
        let port = arg_after("--port")
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(0);
        let model_file = arg_after("-m").unwrap_or_default();
        out.push((id.to_string(), port, model_file));
    }
    out.sort();
    out
}

/// The origin of an op invocation: the `origin` argument when given
/// (`activation`, `dashboard`, ...), else `ui` (desktop calls; the REST
/// layer passes its own).
fn op_origin(origin: Option<&str>) -> lifecycle::Origin {
    origin
        .map(lifecycle::Origin::parse)
        .unwrap_or(lifecycle::Origin::Ui)
}

/// Start a backend (supervisor spawn, or SCM start in service mode
/// when the backend is registered as a service). Returns a task id.
/// `allow_wddm_overcommit` is the operator's two-click confirmation
/// for a start whose requirement exceeds a WDDM GPU's total VRAM
/// (the excess pages to system RAM); it never overrides a TCC
/// refusal — see [`vram_start_gate`]. A thin wrapper over
/// [`backend_lifecycle::start_backend`].
pub async fn op_start_backend(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    id: String,
    allow_wddm_overcommit: Option<bool>,
    origin: Option<String>,
) -> ApiResult {
    let opts = backend_lifecycle::BackendOpOpts {
        allow_wddm_overcommit: allow_wddm_overcommit == Some(true),
        ..backend_lifecycle::BackendOpOpts::new(op_origin(origin.as_deref()))
    };
    backend_lifecycle::start_backend(state, sink, &id, opts).await
}

/// Stop a backend, gracefully (drain) or immediately. A thin wrapper
/// over [`backend_lifecycle::stop_backend`].
pub async fn op_stop_backend(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    id: String,
    graceful: bool,
    origin: Option<String>,
) -> ApiResult {
    let opts = backend_lifecycle::BackendOpOpts {
        graceful,
        ..backend_lifecycle::BackendOpOpts::new(op_origin(origin.as_deref()))
    };
    backend_lifecycle::stop_backend(state, sink, &id, opts).await
}

/// Restart a backend: stop + start inside one `restarting` transition
/// ([`backend_lifecycle::restart_backend`]). `wait_ready_secs` makes the
/// call return only once the backend answers `/health` (the activation
/// plan uses it so "applied" means live).
pub async fn op_restart_backend(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    id: String,
    graceful: bool,
    allow_wddm_overcommit: Option<bool>,
    origin: Option<String>,
    wait_ready_secs: Option<u64>,
) -> ApiResult {
    let opts = backend_lifecycle::BackendOpOpts {
        graceful,
        allow_wddm_overcommit: allow_wddm_overcommit == Some(true),
        wait_ready: wait_ready_secs.filter(|s| *s > 0).map(Duration::from_secs),
        ..backend_lifecycle::BackendOpOpts::new(op_origin(origin.as_deref()))
    };
    match backend_lifecycle::restart_backend(state, sink, &id, opts).await {
        Ok(v) => Ok(v),
        Err(e) => {
            // A restart the pending activation plan depends on just
            // failed: that is the plan failing to apply. A restart
            // failure with no plan entry for this backend is a plain
            // command error the UI already shows — no toast. A busy
            // refusal is not a failure of the restart in flight.
            if !matches!(e, ApiError::Busy(_)) {
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
        let diff =
            manager_config::apply_config_diff(&mut new_cfg, &id, &patch).map_err(ApiError::from)?;
        let mut journaled = record_gateway_change(
            state,
            &pre_cfg,
            &new_cfg,
            "backend_apply_config",
            format!("Backend `{id}` settings changed"),
            false,
        )
        .await?;
        // The same save is also a backend activation: whatever the
        // gateway does with it, the backend process itself only
        // picks its settings up at its next (re)start.
        if !diff.changes.is_empty() {
            let backend_record = record_backend_change(
                state,
                &pre_cfg,
                &new_cfg,
                "backend_apply_config",
                format!("Backend `{id}` settings changed"),
                &id,
            )
            .await;
            chain_journaled(state, &mut journaled, backend_record)?;
        }
        save_journaled(state, &new_cfg, &journaled)?;
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
    let target = info.offload_for(layers);
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
    cache_k: Option<String>,
    cache_v: Option<String>,
    flash_attn: Option<String>,
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
        return Err(ApiError::InvalidInput("port must be non-zero".to_string()));
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
    // KV cache / flash attention: validated by the same rules
    // backend_set_cache applies, when the add form chose a
    // selection (the predicted-best starting point it renders,
    // or the operator's change to it). All three values together
    // or none; the invalid quantized-V + FA-off pairing is
    // refused by the shared resolver.
    let cache_sel = resolve_cache_selection(cache_k, cache_v, flash_attn)?;
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
        raw_flags: model_default_raw_flags_at(&cfg.data_dir, &model),
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
    // A chosen cache selection is written into the constructed
    // backend's raw flags last (it rewrites only its own tokens),
    // so the creation save carries it with everything else. An
    // absent choice writes no cache tokens: creation stays
    // byte-identical to the pre-feature shape, at llama.cpp's
    // defaults.
    if let Some(sel) = cache_sel {
        backend.raw_flags =
            manager_models::set_cache_selection(backend.raw_flags.as_deref(), Some(sel));
    }
    new_cfg.backends.push(backend);
    if let Some(m) = new_cfg.models.iter_mut().find(|m| m.id == model_id) {
        if !m.assigned_backends.contains(&id) {
            m.assigned_backends.push(id.clone());
        }
    }
    manager_config::validate(&new_cfg).map_err(ApiError::from)?;
    let journaled = record_gateway_change(
        state,
        &pre_cfg,
        &new_cfg,
        "backend_add",
        format!("Backend `{id}` added to group `{group}`"),
        false,
    )
    .await?;
    save_journaled(state, &new_cfg, &journaled)?;
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
    n_ctx: Option<u32>,
    parallel: Option<u32>,
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
        raw_flags: model_default_raw_flags_at(&cfg.data_dir, &model),
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
    // The cache preview for the backend creation would build:
    // the predicted footprint and predicted-best ladder at the
    // form's context/slot draft (the creation defaults when the
    // form sends none), so the add form renders its KV cache
    // controls from the same payload the Edit Flags form renders
    // from, with the predicted-best rung as the starting point.
    // Null when the model's layout cannot be resolved.
    let cache = resolve_weight_layout(&cfg, &backend).map(|layout| {
        let targets = cache_fit_targets(&backend, &gpu_inventory);
        cache_preview_json(
            &layout,
            manager_models::expert_offload(backend.raw_flags.as_deref()),
            manager_models::cache_selection(backend.raw_flags.as_deref()),
            n_ctx.unwrap_or(backend.flags.n_ctx),
            parallel
                .map(|p| p.max(1))
                .unwrap_or_else(|| manager_models::effective_slots(backend.raw_flags.as_deref())),
            manager_models::moe_cache_mib(backend.raw_flags.as_deref()).unwrap_or(0),
            targets.as_ref(),
        )
    });
    let memory = add_preview_memory_json(&cfg, &backend, n_ctx, parallel, &gpu_inventory);
    Ok(json!({
        "model_id": model_id,
        "offload": offload,
        "cache": cache,
        "tensor_split": tensor_split,
        "split_mode": split_mode_str(split_mode),
        "memory": memory,
    }))
}

/// The add form's requirement summary (add-backend audit Q8): the
/// transient backend creation would build, sized at the form's
/// context/slot draft, priced by `backend_memory_requirement` —
/// the figures list_backends rows carry as `gpu_ram_mib` /
/// `sys_ram_mib` / `kv_ram_mib` / `rs_ram_mib` / `moe_cache_mib` /
/// `memory_basis`, so the add form's GPU RAM / System RAM tiles
/// mean what the backend card's tiles mean — plus `per_gpu`: the
/// split share and the residency the start gate would judge
/// (`residency_per_gpu` over the telemetry inventory) for every
/// GPU the previewed placement uses. An unsplit placement loads on
/// the first GPU (share 1). `per_gpu` is empty without telemetry.
/// Null when the model's layout cannot be resolved — never a
/// zero-filled requirement.
fn add_preview_memory_json(
    cfg: &AppConfig,
    backend: &manager_config::BackendConfig,
    n_ctx: Option<u32>,
    parallel: Option<u32>,
    gpu_inventory: &[GpuInfo],
) -> Value {
    let mut sized = backend.clone();
    if let Some(c) = n_ctx {
        sized.flags.n_ctx = c;
    }
    if let Some(p) = parallel {
        sized.raw_flags = manager_models::set_parallel_slots(
            sized.raw_flags.as_deref(),
            Some(i64::from(p.max(1))),
        );
    }
    let Some(req) = backend_memory_requirement(cfg, &sized) else {
        return Value::Null;
    };
    let split = &sized.flags.tensor_split;
    let n = gpu_inventory.len();
    let residency = manager_optimize::residency_per_gpu(&req, split, n);
    let per_gpu: Vec<Value> = gpu_inventory
        .iter()
        .enumerate()
        .filter_map(|(pos, g)| {
            let share = if split.is_empty() {
                if pos == 0 {
                    1.0
                } else {
                    0.0
                }
            } else {
                split.get(pos).copied().unwrap_or(0.0)
            };
            if share <= 0.0 {
                return None;
            }
            Some(json!({
                "gpu": g.index,
                "share": share,
                "required_mib": residency[pos],
                "total_mib": g.total_vram_mib,
            }))
        })
        .collect();
    json!({
        "gpu_ram_mib": req.gpu_weights_mib,
        "sys_ram_mib": req.sys_weights_mib,
        "kv_ram_mib": req.kv_mib,
        "rs_ram_mib": req.rs_mib,
        "moe_cache_mib": req.moe_cache_mib,
        "basis": serde_json::to_value(req.basis).unwrap_or(Value::Null),
        "per_gpu": per_gpu,
    })
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
    manager_config::validate(&new_cfg).map_err(ApiError::from)?;
    let journaled = if changed {
        record_backend_change(
            state,
            &pre_cfg,
            &new_cfg,
            "backend_flags",
            format!("Backend `{id}` context/batch changed"),
            &id,
        )
        .await?
    } else {
        activation::JournalRecords::none()
    };
    save_journaled(state, &new_cfg, &journaled)?;
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
    catalog_default_raw_flags_mapped(model_id, None)
}

/// [`catalog_default_raw_flags`] with the model's MoE block map:
/// the catalog pin counts MoE LAYERS, so a partial pin is written
/// at the `--n-cpu-moe` block-index bound that moves exactly that
/// many (code review v0.1.60 item 11). Without a map (the file is
/// not downloaded or not parsed) the pin is written as is — exact
/// for models whose MoE blocks start at block 0, an Estimate
/// otherwise.
fn catalog_default_raw_flags_mapped(
    model_id: &str,
    map: Option<&manager_models::MoeBlockMap>,
) -> Option<String> {
    manager_models::curated_models()
        .into_iter()
        .find(|s| s.id == model_id)
        .and_then(|s| {
            let offload = match manager_models::catalog_default_offload(&s) {
                manager_models::ExpertOffload::CpuMoeLayers(n) => {
                    manager_models::offload_for_moe_layers(n, s.moe_layers.unwrap_or(n), map)
                }
                other => other,
            };
            manager_models::offload_flag_string(offload)
        })
}

/// The MoE block map of `model`'s GGUF, when the file is present
/// and its header parses with an expert tensor index.
fn model_moe_block_map(
    data_dir: &std::path::Path,
    model: &manager_config::ModelConfig,
) -> Option<manager_models::MoeBlockMap> {
    let path = resolve_under_data_dir(data_dir, &model.gguf_path);
    let layout = gguf_layout_cached(&path)?;
    manager_models::MoeBlockMap::from_tensors(&layout.expert_tensors)
}

/// [`model_default_raw_flags`] resolved against the model's GGUF
/// (when present under `data_dir`): MoE-layer defaults are written
/// at their block-index bound (see
/// [`catalog_default_raw_flags_mapped`]).
fn model_default_raw_flags_at(
    data_dir: &std::path::Path,
    model: &manager_config::ModelConfig,
) -> Option<String> {
    model_default_raw_flags_mapped(model, model_moe_block_map(data_dir, model).as_ref())
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
/// (No block map: see [`model_default_raw_flags_at`].)
#[cfg(test)]
fn model_default_raw_flags(model: &manager_config::ModelConfig) -> Option<String> {
    model_default_raw_flags_mapped(model, None)
}

fn model_default_raw_flags_mapped(
    model: &manager_config::ModelConfig,
    map: Option<&manager_models::MoeBlockMap>,
) -> Option<String> {
    if let Some(flags) = catalog_default_raw_flags_mapped(&model.id, map) {
        return Some(flags);
    }
    let layers = model.moe_layers?;
    let default = model.default_offload_layers?;
    if layers == 0 {
        return None;
    }
    manager_models::offload_flag_string(manager_models::offload_for_moe_layers(
        default.min(layers),
        layers,
        map,
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
    let (eval, block_map) = {
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
        let eval = evaluate_offload_position(
            &info.restrictions,
            &id,
            layers,
            info.moe_layers,
            placed_ccs,
            cpu_only,
            &journal,
        );
        (eval, info.block_map)
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
    // `layers` is a MoE-layer count; the flag is written at the
    // block-index bound that moves exactly that many (item 11).
    let target = manager_models::offload_for_moe_layers(layers, moe_layers, block_map.as_ref());
    let new_raw = manager_models::set_expert_offload(b.raw_flags.as_deref(), target);
    let changed = new_raw != b.raw_flags;
    b.raw_flags = new_raw.clone();
    if changed {
        // A raw-flags change invalidates the recorded tool-call
        // probe (MCP design §7.2).
        b.tool_call_probe = None;
    }
    if changed {
        manager_config::validate(&new_cfg).map_err(ApiError::from)?;
        let summary = match target {
            manager_models::ExpertOffload::None => {
                format!("Backend `{id}` expert offload turned off")
            }
            manager_models::ExpertOffload::AllExperts => {
                format!("Backend `{id}` expert offload set to all {moe_layers} MoE layers")
            }
            manager_models::ExpertOffload::CpuMoeLayers(_) => {
                format!("Backend `{id}` expert offload set to {layers} of {moe_layers} MoE layers")
            }
            // offload_for_moe_layers never selects operator patterns.
            manager_models::ExpertOffload::TensorOverrides => {
                format!("Backend `{id}` expert offload set by tensor overrides")
            }
        };
        let journaled = record_backend_change(
            state,
            &pre_cfg,
            &new_cfg,
            "backend_set_offload",
            summary,
            &id,
        )
        .await?;
        save_journaled(state, &new_cfg, &journaled)?;
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

/// Parse one placed compute-capability string ("6.0") for the
/// guardrail-override operations. The override key names the placed
/// set in exactly this spelling (the spelling `list_backends`
/// emits in the guardrail's `placed_ccs`), so a malformed entry is
/// refused rather than normalized into a different key.
fn parse_placed_cc(text: &str) -> Result<(u32, u32), ApiError> {
    let trimmed = text.trim();
    let mut parts = trimmed.split('.');
    let (Some(major), Some(minor), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(ApiError::InvalidInput(format!(
            "placed compute capability {text:?} is not in \"major.minor\" form"
        )));
    };
    let parse = |part: &str| {
        part.parse::<u32>().map_err(|_| {
            ApiError::InvalidInput(format!(
                "placed compute capability {text:?} is not in \"major.minor\" form"
            ))
        })
    };
    Ok((parse(major)?, parse(minor)?))
}

fn format_placed_ccs(ccs: &[(u32, u32)]) -> Vec<String> {
    ccs.iter().map(|cc| format!("{}.{}", cc.0, cc.1)).collect()
}

/// Evaluate a backend's SAVED offload position for the deliberate
/// guardrail-override operations: resolve the backend and its
/// offload capability from a config read, fetch the GPU inventory
/// without holding the config lock (the telemetry accessor reads
/// the config while initializing), and evaluate against the
/// activation journal's standing overrides.
async fn evaluate_saved_offload_for_override(
    state: &Arc<AppState>,
    id: &str,
) -> Result<OffloadEvaluation, ApiError> {
    let cfg = state.config.read().await.clone();
    let b = cfg
        .backends
        .iter()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    let gpus = {
        let store = state.telemetry().await;
        let inventory = lock_telemetry(&store).inventory();
        inventory
    };
    let journal = activation::load_journal(&state.data_dir);
    evaluate_backend_offload(&cfg, b, &gpus, &journal).ok_or_else(|| {
        ApiError::InvalidInput(format!(
            "backend {id}'s model does not support expert offload"
        ))
    })
}

/// Record a deliberate guardrail override for a backend's SAVED
/// offload position (guardrail-override UI; tracker #37 follow-on).
///
/// The override is a standing activation-journal record — the
/// mechanism #37 shipped — keyed to exactly (backend, layers,
/// placed compute-capability set). This operation writes no config
/// and no flags: it authorizes the position the backend already
/// carries, which is the case the slider path cannot reach (a
/// blocked position that arrived by hand-edited flags or a model
/// reassignment, where there is no pending slider save to attach
/// the override to). Because there is no config mutation, the
/// clone-validate-save discipline applies to the journal record
/// itself: the key is derived and validated from the live config,
/// placement, and catalog restrictions before the journal's
/// locked load-modify-save records it, and the write is read back
/// for coverage before success is reported.
///
/// Exactness is enforced at this boundary, never widened: the
/// caller names the key it confirmed (`layers` + `placed_ccs`, as
/// rendered on the backend card), and any mismatch with the saved
/// position or the current placement refuses — an override granted
/// for one layers/CC key covers no other, and a placement change
/// since the confirmation requires a fresh confirmation. A
/// position that is not guardrail-blocked refuses as well: an
/// override is meaningful only for a blocked position, and
/// recording one for a verified position would leave a standing
/// authorization whose scope no reader could see.
pub async fn op_backend_set_offload_override(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    layers: u32,
    placed_ccs: Vec<String>,
) -> ApiResult {
    let mut expected_ccs: Vec<(u32, u32)> = Vec::new();
    for text in &placed_ccs {
        expected_ccs.push(parse_placed_cc(text)?);
    }
    let eval = evaluate_saved_offload_for_override(state, &id).await?;
    if layers != eval.layers {
        return Err(ApiError::InvalidInput(format!(
            "override layers {layers} do not match backend {id}'s saved offload position \
             ({} of {} layers) — an override covers exactly the saved position; \
             re-read the backend and confirm the position shown",
            eval.layers, eval.moe_layers
        )));
    }
    let expected_set: std::collections::BTreeSet<(u32, u32)> =
        expected_ccs.iter().copied().collect();
    let placed_set: std::collections::BTreeSet<(u32, u32)> =
        eval.placed_ccs.iter().copied().collect();
    if expected_set != placed_set {
        return Err(ApiError::InvalidInput(format!(
            "override placed compute-capability set [{}] does not match backend {id}'s \
             current placement [{}] — the placement changed since the confirmation; \
             re-read the backend and confirm the placement shown",
            format_placed_ccs(&expected_ccs).join(", "),
            format_placed_ccs(&eval.placed_ccs).join(", ")
        )));
    }
    if eval.status != manager_models::OffloadStatus::Blocked {
        return Err(ApiError::InvalidInput(format!(
            "backend {id}'s saved offload position ({} of {} layers) is not \
             guardrail-blocked on its placed GPUs — no override applies",
            eval.layers, eval.moe_layers
        )));
    }
    activation::record_offload_override(&state.data_dir, &id, layers, eval.placed_ccs.clone())
        .map_err(ApiError::Io)?;
    let journal = activation::load_journal(&state.data_dir);
    if !activation::offload_override_covers(&journal, &id, layers, &eval.placed_ccs) {
        return Err(ApiError::Io(format!(
            "the offload override for backend {id} was recorded but does not read back as covering its position"
        )));
    }
    Ok(json!({
        "backend_id": id,
        "layers": layers,
        "moe_layers": eval.moe_layers,
        "placed_ccs": format_placed_ccs(&eval.placed_ccs),
        "has_override": true,
        "overridden": true,
        "persisted": true,
    }))
}

/// Clear a backend's standing guardrail override (guardrail-
/// override UI; tracker #37 follow-on). After a clear, the saved
/// position evaluates exactly as it did before the override was
/// recorded: a blocked position is blocked again at launch
/// preflight. Clearing is idempotent — clearing a backend with no
/// standing override succeeds and reports `cleared: false` — and
/// touches only the activation journal: no config, no flags.
pub async fn op_backend_clear_offload_override(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    {
        let cfg = state.config.read().await;
        if !cfg.backends.iter().any(|b| b.id == id) {
            return Err(ApiError::NotFound(format!("backend {id}")));
        }
    }
    let had_override = activation::load_journal(&state.data_dir)
        .offload_overrides
        .iter()
        .any(|o| o.backend_id == id);
    if had_override {
        activation::clear_offload_overrides(&state.data_dir, &id).map_err(ApiError::Io)?;
    }
    // Report the position's standing after the clear when the
    // backend is offload-capable; a backend whose model carries no
    // offload simply has nothing left to override.
    let eval = evaluate_saved_offload_for_override(state, &id).await.ok();
    Ok(json!({
        "backend_id": id,
        "cleared": had_override,
        "layers": eval.as_ref().map(|e| e.layers),
        "moe_layers": eval.as_ref().map(|e| e.moe_layers),
        "placed_ccs": eval.as_ref().map(|e| format_placed_ccs(&e.placed_ccs)),
        "has_override": false,
        "overridden": false,
        "persisted": true,
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
        manager_config::validate(&new_cfg).map_err(ApiError::from)?;
        let summary = match n_max {
            Some(n) => format!("Backend `{id}` MTP draft set to {n} tokens"),
            None => format!("Backend `{id}` MTP draft turned off"),
        };
        let journaled =
            record_backend_change(state, &pre_cfg, &new_cfg, "backend_set_mtp", summary, &id)
                .await?;
        save_journaled(state, &new_cfg, &journaled)?;
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

/// Resolve the Auto expert-cache size for a backend: the
/// largest cache its cache-bearing GPUs can price right now,
/// less the standing reserve
/// ([`manager_models::MOE_CACHE_AUTO_RESERVE_MIB`]). Priced free
/// space per GPU is the telemetry snapshot's measured free VRAM
/// minus every other backend's accounted residency and this
/// backend's own non-cache requirement share — the fit
/// evaluator's accounting, so Auto never promises space the
/// start gate would then refuse. Errors carry the numbers when
/// placement or telemetry cannot price a cache, or when no
/// positive size survives the reserve.
async fn resolve_auto_moe_cache_mib(
    state: &Arc<AppState>,
    cfg: &AppConfig,
    b: &manager_config::BackendConfig,
    offload_layers: u32,
    moe_layers: u32,
) -> Result<u64, ApiError> {
    let placed: Vec<u32> = match backend_gpu_visibility(b) {
        GpuVisibility::Gpus(indices) => indices,
        _ => {
            return Err(ApiError::InvalidInput(format!(
            "Auto cannot price an expert cache for backend {}: it is not placed on specific GPUs",
            b.id
        )))
        }
    };
    let Some(mut req) = backend_memory_requirement(cfg, b) else {
        return Err(ApiError::InvalidInput(format!(
            "Auto cannot price an expert cache for backend {}: its memory requirement does not resolve",
            b.id
        )));
    };
    // Price the backend's own NON-cache requirement: an existing
    // token is being replaced, not added to.
    req.moe_cache_mib = 0;
    let mut samples = {
        let store = state.telemetry().await;
        let snapshot = lock_telemetry(&store).snapshot();
        snapshot
    };
    samples.sort_by_key(|s| s.index);
    if samples.is_empty() {
        return Err(ApiError::InvalidInput(format!(
            "Auto cannot price an expert cache for backend {}: no GPU telemetry is available",
            b.id
        )));
    }
    let n = samples.len();
    let own = manager_optimize::residency_per_gpu(&req, &b.flags.tensor_split, n);
    let mut others = vec![0u64; n];
    for ob in cfg.backends.iter().filter(|ob| ob.id != b.id) {
        if let Some(or) = backend_memory_requirement(cfg, ob) {
            let per = manager_optimize::residency_per_gpu(&or, &ob.flags.tensor_split, n);
            for (acc, v) in others.iter_mut().zip(per.iter()) {
                *acc = acc.saturating_add(*v);
            }
        }
    }
    // The cache lands only on cache-bearing GPUs (see
    // moe_cache_shares); a probe budget reads the bearing set and
    // each GPU's fraction of it.
    const PROBE_MIB: u64 = 1_000_000;
    let probe = manager_models::moe_cache_shares(
        PROBE_MIB,
        &b.flags.tensor_split,
        offload_layers,
        moe_layers,
        n,
    );
    let mut bearing: Vec<(u64, f64)> = Vec::new();
    let mut tightest: Option<(u32, u64, u64, u64)> = None;
    for (pos, sample) in samples.iter().enumerate() {
        if probe[pos] == 0 || !placed.contains(&sample.index) {
            continue;
        }
        let free = sample.free_vram_mib();
        let available = free.saturating_sub(others[pos]).saturating_sub(own[pos]);
        bearing.push((available, probe[pos] as f64 / PROBE_MIB as f64));
        let tighter = match tightest {
            None => true,
            Some((_, a, _, _)) => available < a,
        };
        if tighter {
            tightest = Some((
                sample.index,
                available,
                free,
                others[pos].saturating_add(own[pos]),
            ));
        }
    }
    match manager_models::auto_moe_cache_budget(
        &bearing,
        manager_models::MOE_CACHE_AUTO_RESERVE_MIB,
    ) {
        Some(mib) => Ok(mib),
        None => {
            let detail = match tightest {
                Some((index, available, free, accounted)) => format!(
                    "GPU {index} has {free} MiB free, of which {accounted} MiB is accounted by other backends and this backend's own requirement, leaving {available} MiB"
                ),
                None => "no placed GPU bears offloaded layers".to_string(),
            };
            Err(ApiError::InvalidInput(format!(
                "Auto found no room for an expert cache on backend {}: {detail}; the standing reserve is {} MiB",
                b.id,
                manager_models::MOE_CACHE_AUTO_RESERVE_MIB
            )))
        }
    }
}

/// Set a backend's MoE expert cache (`--moe-cache-mib`): rewrite
/// only that token in the backend's raw flags — `mib` writes the
/// canonical token, `None` (with `auto` unset) turns the cache
/// off, and `auto: true` resolves the size from priced free VRAM
/// (see [`resolve_auto_moe_cache_mib`]) and writes the resolved
/// value. Every other flag is preserved. Eligibility is the
/// offload slider's exactly ([`backend_moe_cache_info`]): a dense
/// model has no experts to cache. A nonzero cache additionally
/// requires a saved offload position above 0 — with no experts
/// in system RAM there is nothing to cache — and refuses a row
/// (tensor) split, which the pinned llama.cpp rejects the cache
/// under; turning the cache off is always allowed, so a stranded
/// token can always be cleared. The size is priced into the
/// backend's memory requirement, so a start whose cache cannot
/// fit is refused by the start gate with the numbers. Saved
/// through the same clone-validate-save path as the other
/// backend mutations and journaled as a backend-scope activation
/// change with `applies: "next start"`; no restart happens here.
pub async fn op_backend_set_moe_cache(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    mib: Option<u32>,
    auto: Option<bool>,
) -> ApiResult {
    let want_auto = auto == Some(true);
    if want_auto && mib.is_some() {
        return Err(ApiError::InvalidInput(
            "moe-cache takes either an explicit mib or auto, not both".to_string(),
        ));
    }
    // Eligibility and the static refusals resolve against the
    // current config before the write lock, and re-resolve under
    // it below (a concurrent model or placement change can alter
    // every one of them — the set_offload/set_mtp shape).
    let (offload_layers, moe_layers) = {
        let cfg = state.config.read().await;
        let b = cfg
            .backends
            .iter()
            .find(|b| b.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
        ensure_backend_not_advanced(b)?;
        let info = backend_moe_cache_info(&cfg, b).ok_or_else(|| {
            ApiError::InvalidInput(format!(
                "backend {id}'s model does not support an expert cache (it is not an offload-capable MoE model)"
            ))
        })?;
        check_moe_cache_request(b, mib, want_auto, info.offload_layers)?;
        (info.offload_layers, info.moe_layers)
    };
    // Auto resolves between the config reads: telemetry access
    // must never hold the config lock (see op_backend_set_offload).
    let resolved = if want_auto {
        let cfg = state.config.read().await;
        let b = cfg
            .backends
            .iter()
            .find(|b| b.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
        let mib = resolve_auto_moe_cache_mib(state, &cfg, b, offload_layers, moe_layers).await?;
        Some(u32::try_from(mib).map_err(|_| {
            ApiError::InvalidInput(format!(
                "Auto resolved an expert cache of {mib} MiB for backend {id}, beyond the control's range"
            ))
        })?)
    } else {
        None
    };
    let target: Option<u32> = if want_auto { resolved } else { mib };
    let mut cfg = state.config.write().await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    {
        // Re-validate against the config actually being mutated.
        let b = new_cfg
            .backends
            .iter()
            .find(|b| b.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
        ensure_backend_not_advanced(b)?;
        let info = backend_moe_cache_info(&new_cfg, b).ok_or_else(|| {
            ApiError::InvalidInput(format!(
                "backend {id}'s model does not support an expert cache (it is not an offload-capable MoE model)"
            ))
        })?;
        check_moe_cache_request(b, target, false, info.offload_layers)?;
    }
    let b = new_cfg
        .backends
        .iter_mut()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    let new_raw = manager_models::set_moe_cache(b.raw_flags.as_deref(), target);
    let changed = new_raw != b.raw_flags;
    b.raw_flags = new_raw.clone();
    if changed {
        // A raw-flags change invalidates the recorded tool-call
        // probe (MCP design §7.2).
        b.tool_call_probe = None;
    }
    if changed {
        manager_config::validate(&new_cfg).map_err(ApiError::from)?;
        let summary = match target {
            Some(n) if want_auto => {
                format!("Backend `{id}` expert cache set to {n} MiB (Auto)")
            }
            Some(n) => format!("Backend `{id}` expert cache set to {n} MiB"),
            None => format!("Backend `{id}` expert cache turned off"),
        };
        let journaled = record_backend_change(
            state,
            &pre_cfg,
            &new_cfg,
            "backend_set_moe_cache",
            summary,
            &id,
        )
        .await?;
        save_journaled(state, &new_cfg, &journaled)?;
        *cfg = new_cfg;
    }
    drop(cfg);
    if changed {
        ensure_backend_specs(state).await;
    }
    state.sync_backend_service_definition(&id).await;
    Ok(json!({
        "backend_id": id,
        "mib": manager_models::moe_cache_mib(new_raw.as_deref()).filter(|n| *n > 0),
        "auto": want_auto,
        "resolved_mib": resolved,
        "raw_flags": new_raw,
        "changed": changed,
        "persisted": true,
        "applies": "next start",
    }))
}

/// The expert-cache request's static refusals, shared by the
/// op's pre-lock and under-lock passes: a nonzero size (explicit,
/// or any Auto request, which resolves to a nonzero size by
/// construction) requires a saved offload position above 0 and a
/// non-tensor split. Turning the cache off passes always.
fn check_moe_cache_request(
    b: &manager_config::BackendConfig,
    mib: Option<u32>,
    auto: bool,
    offload_layers: u32,
) -> Result<(), ApiError> {
    let nonzero = auto || mib.is_some_and(|n| n > 0);
    if !nonzero {
        return Ok(());
    }
    if offload_layers == 0 {
        return Err(ApiError::InvalidInput(format!(
            "backend {} offloads 0 MoE layers: the expert cache holds the offloaded experts in GPU memory, and there are none in system RAM to cache — raise the offload above 0 first",
            b.id
        )));
    }
    if b.flags.split_mode == manager_config::SplitMode::Row {
        return Err(ApiError::InvalidInput(format!(
            "backend {} uses a row (tensor) split: the pinned llama.cpp refuses the expert cache under tensor parallelism",
            b.id
        )));
    }
    Ok(())
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
    let new_raw = manager_models::set_parallel_slots(b.raw_flags.as_deref(), slots.map(i64::from));
    let changed = new_raw != b.raw_flags;
    b.raw_flags = new_raw.clone();
    if changed {
        // A raw-flags change invalidates the recorded tool-call
        // probe (MCP design §7.2).
        b.tool_call_probe = None;
    }
    if changed {
        manager_config::validate(&new_cfg).map_err(ApiError::from)?;
        let summary = match slots {
            Some(n) => format!("Backend `{id}` parallel slots set to {n}"),
            None => {
                format!("Backend `{id}` parallel slots returned to the llama.cpp default (auto)")
            }
        };
        let journaled = record_backend_change(
            state,
            &pre_cfg,
            &new_cfg,
            "backend_set_parallel",
            summary,
            &id,
        )
        .await?;
        save_journaled(state, &new_cfg, &journaled)?;
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

/// Parse one cache-control value from the wire: a cache type
/// spelling the pinned build accepts among the types v1 offers.
fn parse_cache_type(value: &str, field: &str) -> Result<manager_models::CacheType, ApiError> {
    manager_models::CacheType::parse(value).ok_or_else(|| {
        ApiError::InvalidInput(format!(
            "{field} must be one of f16, q8_0, q4_0; got {value:?}"
        ))
    })
}

/// Parse the flash-attention value from the wire.
fn parse_flash_attn(value: &str) -> Result<manager_models::FlashAttn, ApiError> {
    manager_models::FlashAttn::parse(value).ok_or_else(|| {
        ApiError::InvalidInput(format!(
            "flash_attn must be one of auto, on, off; got {value:?}"
        ))
    })
}

/// Resolve a full cache selection from the three wire values:
/// all three present parses each; all three absent is the
/// cleared state (`None` — no cache tokens); a partial set is
/// refused, because the writer owns the selection as a whole and
/// a partial write would silently freeze the unmentioned members
/// at their current values.
fn resolve_cache_selection(
    cache_k: Option<String>,
    cache_v: Option<String>,
    flash_attn: Option<String>,
) -> Result<Option<manager_models::CacheSelection>, ApiError> {
    match (cache_k, cache_v, flash_attn) {
        (None, None, None) => Ok(None),
        (Some(k), Some(v), Some(f)) => {
            let sel = manager_models::CacheSelection {
                k: parse_cache_type(&k, "cache_k")?,
                v: parse_cache_type(&v, "cache_v")?,
                flash_attn: parse_flash_attn(&f)?,
            };
            if sel.v_quantized_fa_off() {
                return Err(ApiError::InvalidInput(format!(
                    "a quantized V cache ({}) requires flash attention: llama.cpp refuses \
                     it with --flash-attn off — choose flash_attn auto or on",
                    sel.v.as_str()
                )));
            }
            Ok(Some(sel))
        }
        _ => Err(ApiError::InvalidInput(
            "cache_k, cache_v, and flash_attn are set together (or all omitted to clear)"
                .to_string(),
        )),
    }
}

/// A cache fit's per-GPU targets, in placement order: the split
/// shares, the GPUs' total VRAM, the placed GPU indices, and
/// their driver models (the three-state fit's paging basis).
type CacheFitTargets = (Vec<f32>, Vec<u64>, Vec<u32>, Vec<Option<DriverModel>>);

/// The targets a cache fit is priced against for a backend: the
/// GPUs its placement puts it on (per [`backend_gpu_visibility`]),
/// their total VRAM, and the tensor-split shares over exactly
/// those GPUs, in placement order. `None` when no GPU target can
/// be named (CPU-only placement, or a placed GPU absent from the
/// inventory): no fit claim is made then. An unsplit placement
/// yields a single target and an empty split (the whole total on
/// it, [`manager_models::predicted_fits`]' convention).
fn cache_fit_targets(
    b: &manager_config::BackendConfig,
    gpus: &[GpuInfo],
) -> Option<CacheFitTargets> {
    let placed = match backend_gpu_visibility(b) {
        GpuVisibility::Gpus(indices) => indices,
        _ => return None,
    };
    if placed.is_empty() {
        return None;
    }
    let mut totals: Vec<u64> = Vec::new();
    let mut drivers: Vec<Option<DriverModel>> = Vec::new();
    for i in &placed {
        let g = gpus
            .iter()
            .find(|g| g.index == *i)
            .or_else(|| gpus.get(*i as usize))?;
        totals.push(g.total_vram_mib);
        drivers.push(g.driver_model);
    }
    // Split shares over the placed GPUs: the configured split is
    // in inventory order, and the placed set's positions in it
    // are the placed indices (the placement projection's
    // convention). Shares renormalize over the placed set; a
    // split that puts nothing on a placed GPU — or no split at
    // all — is the unsplit case for a single placed GPU and
    // distributes nothing (fits nothing) for several.
    let raw: Vec<f32> = placed
        .iter()
        .map(|i| {
            b.flags
                .tensor_split
                .get(*i as usize)
                .copied()
                .unwrap_or(0.0)
        })
        .collect();
    let sum: f32 = raw.iter().sum();
    let split: Vec<f32> = if sum > 0.0 {
        raw.iter().map(|f| f / sum).collect()
    } else if placed.len() == 1 {
        Vec::new()
    } else {
        raw
    };
    Some((split, totals, placed, drivers))
}

/// The cache preview payload shared by backend_cache_preview and
/// backend_add_preview: the predicted footprint of `selection`
/// at `n_ctx` / `n_seq` under the backend's layout and offload,
/// the full predicted-best ladder priced against the targets,
/// and the best rung. KV figures are absent (null) when the
/// model's KV geometry is unknown — never priced as zero.
fn cache_preview_json(
    layout: &WeightLayout,
    offload: manager_models::ExpertOffload,
    selection: manager_models::CacheSelection,
    n_ctx: u32,
    n_seq: u32,
    moe_cache_mib: u32,
    targets: Option<&CacheFitTargets>,
) -> Value {
    let kv_typed = layout
        .kv_geometry
        .map(|g| g.bytes_per_token_typed(selection.k, selection.v));
    let req = manager_models::memory::memory_requirement_with_tensors(
        layout.total_bytes,
        layout.expert_bytes,
        layout.expert_bytes_by_block.as_deref(),
        layout.basis,
        offload,
        kv_typed.or(layout.kv_per_token),
        layout.moe_layers,
        n_ctx,
        layout.rs_bytes_per_seq,
        n_seq,
        layout.expert_tensors.as_deref(),
        layout.raw_flags.as_deref(),
    );
    // The expert cache prices into the preview's totals the way
    // it prices into the requirement: the whole MiB figure joins
    // the total, and per-target shares distribute it by the
    // cache's own placement (the offload-bearing GPUs), not by
    // the plain tensor split.
    let base_total_mib = req.gpu_weights_mib + req.kv_mib.unwrap_or(0) + req.rs_mib.unwrap_or(0);
    let total_mib = base_total_mib + u64::from(moe_cache_mib);
    let (ladder, best) = match (layout.kv_geometry, targets) {
        (Some(kv), Some((split, totals, _, _))) => {
            let inputs = manager_models::CacheFitInputs {
                kv,
                gpu_weights_mib: req.gpu_weights_mib,
                rs_mib: req.rs_mib,
                n_ctx,
                split: split.clone(),
                gpu_total_mib: totals.clone(),
                moe_cache_mib: u64::from(moe_cache_mib),
            };
            let ladder = manager_models::price_ladder(&inputs);
            let best = ladder.iter().find(|r| r.fits).copied();
            (ladder, best)
        }
        _ => (Vec::new(), None),
    };
    let rung_json = |r: &manager_models::CacheRung| {
        json!({
            "k": r.selection.k.as_str(),
            "v": r.selection.v.as_str(),
            "flash_attn": r.selection.flash_attn.as_str(),
            "kv_mib": r.kv_mib,
            "total_mib": r.total_mib,
            "fits": r.fits,
        })
    };
    json!({
        "selection": {
            "k": selection.k.as_str(),
            "v": selection.v.as_str(),
            "flash_attn": selection.flash_attn.as_str(),
        },
        "v_quantized_fa_off": selection.v_quantized_fa_off(),
        "weights_mib": req.gpu_weights_mib,
        "sys_weights_mib": req.sys_weights_mib,
        "kv_mib": req.kv_mib,
        "rs_mib": req.rs_mib,
        "moe_cache_mib": moe_cache_mib,
        "total_mib": total_mib,
        "basis": serde_json::to_value(req.basis).unwrap_or(Value::Null),
        "targets": targets.map(|(split, totals, placed, drivers)| {
            // Each target carries its share of the total — the
            // same distribution predicted_fits judges — and its
            // own verdict, so the forms render one capacity row
            // per GPU instead of one line against joined totals.
            // The expert-cache share rides its own distribution
            // (the offload-bearing GPUs), added to the base
            // total's plain-split share.
            let shares = manager_models::predicted_shares(base_total_mib, split, totals);
            let cache_shares = manager_models::moe_cache_shares(
                u64::from(moe_cache_mib),
                split,
                layout.offloaded_moe_layers(offload),
                layout.moe_layers.unwrap_or(0),
                placed.len(),
            );
            placed
                .iter()
                .zip(totals.iter())
                .enumerate()
                .map(|(pos, (i, t))| {
                    let required = shares.as_ref().map(|s| s[pos] + cache_shares[pos]);
                    // The three-state classification against the
                    // GPU's TOTAL (the start gate's basis), beside
                    // the headroom `fits` the ladder judges by: a
                    // WDDM GPU over total is a paged fit, not a
                    // hard refusal.
                    let (state, paged) = match required {
                        Some(r) => {
                            let (st, pg) = manager_models::fit_state(
                                r,
                                *t,
                                drivers.get(pos).copied().flatten()
                                    == Some(DriverModel::Wddm),
                            );
                            (Some(st.as_str()), pg)
                        }
                        None => (None, 0),
                    };
                    json!({
                        "gpu": i,
                        "total_mib": t,
                        "required_mib": required,
                        "fits": required.map(|r| {
                            r <= t * (100 - manager_models::HEADROOM_RESERVE_PERCENT) / 100
                        }),
                        "driver_model": drivers.get(pos).copied().flatten().map(|d| match d {
                            DriverModel::Wddm => "wddm",
                            DriverModel::Tcc => "tcc",
                        }),
                        "fit_state": state,
                        "paged_mib": paged,
                    })
                })
                .collect::<Vec<_>>()
        }),
        "headroom_reserve_percent": manager_models::HEADROOM_RESERVE_PERCENT,
        "fits_current": targets.map(|(split, totals, placed, _drivers)| {
            // The same combined distribution the targets carry:
            // base shares by the plain split, cache shares by
            // the cache's own placement — judged under the same
            // headroom rule predicted_fits applies.
            let cache_shares = manager_models::moe_cache_shares(
                u64::from(moe_cache_mib),
                split,
                layout.offloaded_moe_layers(offload),
                layout.moe_layers.unwrap_or(0),
                placed.len(),
            );
            match manager_models::predicted_shares(base_total_mib, split, totals) {
                Some(shares) => shares
                    .iter()
                    .zip(totals.iter())
                    .enumerate()
                    .all(|(pos, (share, t))| {
                        share + cache_shares[pos]
                            <= t * (100 - manager_models::HEADROOM_RESERVE_PERCENT) / 100
                    }),
                None => false,
            }
        }),
        "ladder": ladder.iter().map(rung_json).collect::<Vec<_>>(),
        "best": best.map(|r| rung_json(&r)),
    })
}

/// Preview a backend's cache configuration (the flags form's KV
/// cache / flash-attention controls): the predicted footprint of
/// the given selection at the given context and slot count, and
/// the predicted-best ladder for the backend's placement. Pure
/// preview — nothing is written. The context/slot/selection
/// arguments default to the backend's saved values when omitted,
/// so the form can price its current draft by sending only what
/// changed. The selection defaults likewise (each member
/// independently here, unlike the set op's all-or-nothing rule:
/// a preview freezes nothing).
#[allow(clippy::too_many_arguments)] // argument list is the registry contract
pub async fn op_backend_cache_preview(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    n_ctx: Option<u32>,
    slots: Option<u32>,
    cache_k: Option<String>,
    cache_v: Option<String>,
    flash_attn: Option<String>,
    moe_cache_mib: Option<u32>,
) -> ApiResult {
    // The layout resolves under the config read; the telemetry
    // inventory is fetched after the guard drops (the telemetry
    // accessor reads the config while initializing — it must
    // never be called while this command holds a config guard).
    let (layout, backend) = {
        let cfg = state.config.read().await;
        let b = cfg
            .backends
            .iter()
            .find(|b| b.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
        let layout = resolve_weight_layout(&cfg, b).ok_or_else(|| {
            ApiError::InvalidInput(format!(
                "backend {id}'s model cannot be resolved; its footprint cannot be priced"
            ))
        })?;
        (layout, b.clone())
    };
    let gpus = {
        let store = state.telemetry().await;
        let inventory = lock_telemetry(&store).inventory();
        inventory
    };
    let offload = manager_models::expert_offload(backend.raw_flags.as_deref());
    let saved = manager_models::cache_selection(backend.raw_flags.as_deref());
    let eff_ctx = n_ctx.unwrap_or(backend.flags.n_ctx);
    let eff_slots =
        slots.unwrap_or_else(|| manager_models::effective_slots(backend.raw_flags.as_deref()));
    let targets = cache_fit_targets(&backend, &gpus);
    if let Some(s) = slots {
        validate_parallel_slots(s)?;
    }
    let selection = manager_models::CacheSelection {
        k: match &cache_k {
            Some(v) => parse_cache_type(v, "cache_k")?,
            None => saved.k,
        },
        v: match &cache_v {
            Some(v) => parse_cache_type(v, "cache_v")?,
            None => saved.v,
        },
        flash_attn: match &flash_attn {
            Some(v) => parse_flash_attn(v)?,
            None => saved.flash_attn,
        },
    };
    // The expert-cache size prices as a DRAFT when the caller
    // sends one (the flags form sends its Expert cache control's
    // current state on every preview, so toggling or resizing the
    // cache re-prices the fit block immediately — before this
    // parameter existed the preview read only the SAVED token,
    // and a cache change alone never moved the figures until an
    // unrelated field happened to re-trigger a preview; Pete's
    // Oct 23, 2026 report). Omitted prices the saved token.
    let eff_moe_cache_mib = moe_cache_mib.unwrap_or_else(|| {
        manager_models::moe_cache_mib(backend.raw_flags.as_deref()).unwrap_or(0)
    });
    let mut payload = cache_preview_json(
        &layout,
        offload,
        selection,
        eff_ctx,
        eff_slots.max(1),
        eff_moe_cache_mib,
        targets.as_ref(),
    );
    payload["backend_id"] = json!(id);
    Ok(payload)
}

/// Set a backend's KV cache types and flash-attention mode (the
/// flags form's cache controls): rewrite only the cache tokens
/// in the backend's raw flags to the canonical
/// `--cache-type-k <t> --cache-type-v <t> --flash-attn <m>` —
/// every other flag is preserved. All three values are set
/// together; all three omitted clears the tokens, returning the
/// backend to llama.cpp's defaults with no cache tokens (its
/// pre-feature state). A quantized V cache with flash attention
/// off is refused: llama.cpp fails context creation for that
/// pairing, so persisting it would save a backend that cannot
/// start. Saved through the same clone-validate-save path as the
/// other backend mutations and journaled as a backend-scope
/// activation change with `applies: "next start"`; no restart
/// happens here.
pub async fn op_backend_set_cache(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    cache_k: Option<String>,
    cache_v: Option<String>,
    flash_attn: Option<String>,
) -> ApiResult {
    let target = resolve_cache_selection(cache_k, cache_v, flash_attn)?;
    let mut cfg = state.config.write().await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    let b = new_cfg
        .backends
        .iter_mut()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    // Advanced flags mode (tracker #60): the controls rewrite
    // raw-flag tokens the operator owns as text in the mode.
    ensure_backend_not_advanced(b)?;
    let new_raw = manager_models::set_cache_selection(b.raw_flags.as_deref(), target);
    let changed = new_raw != b.raw_flags;
    b.raw_flags = new_raw.clone();
    if changed {
        // A raw-flags change invalidates the recorded tool-call
        // probe (MCP design §7.2).
        b.tool_call_probe = None;
    }
    if changed {
        manager_config::validate(&new_cfg).map_err(ApiError::from)?;
        let summary = match target {
            Some(sel) => format!(
                "Backend `{id}` KV cache set to {}/{}, flash attention {}",
                sel.k.as_str(),
                sel.v.as_str(),
                sel.flash_attn.as_str()
            ),
            None => format!("Backend `{id}` KV cache controls cleared (llama.cpp defaults)"),
        };
        let journaled =
            record_backend_change(state, &pre_cfg, &new_cfg, "backend_set_cache", summary, &id)
                .await?;
        save_journaled(state, &new_cfg, &journaled)?;
        *cfg = new_cfg;
    }
    drop(cfg);
    if changed {
        ensure_backend_specs(state).await;
    }
    state.sync_backend_service_definition(&id).await;
    Ok(json!({
        "backend_id": id,
        "selection": target.map(|sel| json!({
            "k": sel.k.as_str(),
            "v": sel.v.as_str(),
            "flash_attn": sel.flash_attn.as_str(),
        })),
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
        manager_config::validate(&new_cfg).map_err(ApiError::from)?;
        manager_config::save(&new_cfg, &state.config_path).map_err(ApiError::from)?;
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
/// the response, but any string saves (except one llama-server is
/// certain to refuse at startup, [`manager_models::raw_flag_errors`])
/// — an unrecognized flag may
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
    // The one exception to warn-never-block: a string llama-server
    // is certain to refuse at startup (an `-ot` pattern with a
    // comma inside `{m,n}`, a class or a group — llama-server
    // splits -ot on every comma) is refused here, with a message
    // that suggests an alternation instead.
    if let Some(err) = manager_models::raw_flag_errors(new_raw.as_deref())
        .into_iter()
        .next()
    {
        return Err(ApiError::InvalidInput(err.message));
    }
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
        manager_config::validate(&new_cfg).map_err(ApiError::from)?;
        let journaled = record_backend_change(
            state,
            &pre_cfg,
            &new_cfg,
            "backend_set_raw_flags",
            format!("Backend `{id}` raw flags changed"),
            &id,
        )
        .await?;
        save_journaled(state, &new_cfg, &journaled)?;
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
    sink: &Arc<dyn EventSink>,
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
    if !state
        .config
        .read()
        .await
        .backends
        .iter()
        .any(|b| b.id == id)
    {
        return Err(ApiError::NotFound(format!("backend {id}")));
    }
    // A running Windows service of this backend would otherwise keep
    // serving, invisible once the id leaves the config (SCM-18): stop
    // it through the shared lifecycle path first (markers, events,
    // intent). The registration itself stays the operator's to remove.
    let service_name = manager_services::backend_service_name(&id);
    let service_stopped =
        match lifecycle::scm_status(state, &service_name, Duration::from_secs(1)).await {
            Some(s) if lifecycle::registered(&s) && lifecycle::scm_live(&s) => {
                backend_lifecycle::stop_backend(
                    state,
                    sink,
                    &id,
                    backend_lifecycle::BackendOpOpts::new(lifecycle::Origin::Ui),
                )
                .await?;
                true
            }
            _ => false,
        };
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
    manager_config::validate(&new_cfg).map_err(ApiError::from)?;
    let journaled = record_gateway_change(
        state,
        &pre_cfg,
        &new_cfg,
        "backend_delete",
        format!("Backend `{id}` removed"),
        false,
    )
    .await?;
    save_journaled(state, &new_cfg, &journaled)?;
    *cfg = new_cfg;
    drop(cfg);
    ensure_backend_specs(state).await;
    if service_stopped {
        lifecycle::mark(
            &state.data_dir,
            &lifecycle::backend_key(&id),
            "deleted from the configuration",
            Some("its running service was stopped first; the registration remains"),
            Some(lifecycle::Origin::Ui),
        );
    }
    // A deleted backend's service registration is the operator's to
    // remove (Settings → Supervision) — a config delete must not fire
    // a UAC prompt from a list action. Record the leftover so it is
    // discoverable rather than silent.
    if state.service_route_active(&service_name).await {
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
/// once per poll. Successful parses are cached, and so are
/// definitive parse failures — bytes the parser rejects outright
/// (`Invalid`, `UnsupportedType`, or `Incomplete` on the complete
/// file) will not parse on the next poll either, and re-reading a
/// few MiB per poll to learn that again is waste. Read failures
/// are NOT cached: a transient I/O error (the file locked or
/// mid-replacement by a download, a briefly unavailable mount)
/// says nothing about the file's contents, so the next poll
/// retries and the backend self-heals to the Exact basis instead
/// of sitting on the catalog fallback for the process lifetime
/// (the Oct 20, 2026 planner-mtp incident: one transient parse
/// failure, memoized as `None`, dropped the backend's KV figure
/// until restart). Metadata failures are not cached either, for
/// the same reason.
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
    gguf_layout_cached_with(path, &GGUF_PREFIX_RUNGS)
}

/// The prefix lengths [`gguf_layout_cached`] reads, shortest first.
const GGUF_PREFIX_RUNGS: [usize; 3] = [8 * 1024 * 1024, 64 * 1024 * 1024, 256 * 1024 * 1024];

/// [`gguf_layout_cached`] over an explicit prefix ladder (tests use
/// a short one).
fn gguf_layout_cached_with(path: &PathBuf, rungs: &[usize]) -> Option<manager_models::GgufLayout> {
    let meta = match std::fs::metadata(path) {
        Ok(m) if m.is_file() => m,
        Ok(_) => return None,
        Err(e) => {
            tracing::debug!(
                "gguf layout: metadata read failed for {} (error class: io): {e}",
                path.display()
            );
            return None;
        }
    };
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
    // Every exit from this loop that does not return early is
    // definitive and cached: a layout, a rejection of the complete
    // bytes, or (P-4) a header still `Incomplete` at the largest
    // rung — the next poll would re-read and re-allocate every rung
    // only to learn the same thing. The (len, mtime) key drops the
    // entry as soon as the file changes.
    for &prefix in rungs {
        let want = (meta.len() as usize).min(prefix);
        let mut buf = vec![0u8; want];
        if let Err(e) =
            std::fs::File::open(path).and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
        {
            // Transient by definition: nothing about the file's
            // contents was learned. Log and leave the cache alone
            // so the next poll retries.
            tracing::debug!(
                "gguf layout: read failed for {} (error class: io): {e}",
                path.display()
            );
            return None;
        }
        match manager_models::parse_gguf_layout(&buf) {
            Ok(l) => {
                layout = Some(l);
                break;
            }
            Err(manager_models::GgufError::Incomplete) if want < meta.len() as usize => continue,
            Err(e) => {
                let class = match e {
                    manager_models::GgufError::Incomplete => "incomplete header",
                    manager_models::GgufError::Invalid(_) => "invalid header",
                    manager_models::GgufError::UnsupportedType(_) => "unsupported tensor type",
                };
                tracing::debug!(
                    "gguf layout: parse failed for {} (error class: {class}): {e:?}",
                    path.display()
                );
                break;
            }
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
/// (Catalog — expert bytes, MoE layer count, and KV bytes per
/// token are all pinned, so a partial offload and the KV figure
/// resolve before download), else a
/// parameter-count estimate (Estimate). None when the backend's
/// model cannot be resolved.
struct WeightLayout {
    total_bytes: u64,
    expert_bytes: Option<u64>,
    /// Per-MoE-block expert bytes from the GGUF parse, when the
    /// layout came from one — partial offload is priced from the
    /// true first-N block sums, not a uniform share.
    expert_bytes_by_block: Option<Vec<u64>>,
    /// The routed-expert tensor index (names and sizes) from the
    /// GGUF parse, when the layout came from one and the model has
    /// experts: operator `-ot` patterns and `--n-cpu-moe`'s
    /// block-index rule are priced against it (P-5).
    expert_tensors: Option<std::sync::Arc<[manager_models::gguf::ExpertTensor]>>,
    /// The backend raw flags this layout was resolved for — the
    /// override list `expert_tensors` is evaluated under.
    raw_flags: Option<String>,
    moe_layers: Option<u32>,
    /// The GGUF KV geometry, when the layout came from a parse —
    /// the input cache-type-aware KV pricing needs (each side
    /// priced at its own type). `kv_per_token` below is the same
    /// geometry at the f16 default.
    kv_geometry: Option<manager_models::KvGeometry>,
    kv_per_token: Option<u64>,
    /// Recurrent-state bytes per sequence from the GGUF header's
    /// SSM geometry (hybrid architectures), same provenance as
    /// `kv_per_token`.
    rs_bytes_per_seq: Option<u64>,
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

impl WeightLayout {
    /// The model's MoE block map (which block indices carry
    /// experts), when the layout came from a GGUF parse with an
    /// expert tensor index: what converts the slider's MoE-layer
    /// count to and from `--n-cpu-moe`'s block-index bound (code
    /// review v0.1.60 item 11). `None` for catalog-only and
    /// estimated layouts, where the conversion is the identity.
    fn moe_block_map(&self) -> Option<manager_models::MoeBlockMap> {
        self.expert_tensors
            .as_deref()
            .and_then(manager_models::MoeBlockMap::from_tensors)
    }

    /// MoE layers on system RAM under `offload`, through the block
    /// map (see [`manager_models::ExpertOffload::moe_layers_offloaded`]).
    fn offloaded_moe_layers(&self, offload: manager_models::ExpertOffload) -> u32 {
        offload.moe_layers_offloaded(self.moe_layers.unwrap_or(0), self.moe_block_map().as_ref())
    }
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
    let (
        total_bytes,
        expert_bytes,
        expert_bytes_by_block,
        kv_geometry,
        kv_per_token,
        rs_bytes_per_seq,
        moe_layers,
        basis,
    ) = match (&layout, &catalog) {
        (Some(l), _) => (
            l.tensor_bytes,
            Some(l.expert_bytes),
            Some(l.expert_bytes_by_block.clone()),
            l.kv,
            l.kv.map(|k| k.bytes_per_token()),
            l.rs.map(|g| g.bytes_per_seq()),
            l.moe_block_count,
            manager_models::MemoryBasis::Exact,
        ),
        (None, Some(c)) => (
            c.size_bytes,
            c.expert_bytes,
            None,
            None,
            c.kv_bytes_per_token,
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
    // The tensor index rides only with a parse that found experts:
    // a dense file prices offload exactly as before.
    let expert_tensors = layout
        .as_ref()
        .map(|l| l.expert_tensors.clone())
        .filter(|t| !t.is_empty());
    Some(WeightLayout {
        total_bytes,
        expert_bytes,
        expert_bytes_by_block,
        expert_tensors,
        raw_flags: b.raw_flags.clone(),
        moe_layers,
        kv_geometry,
        kv_per_token,
        rs_bytes_per_seq,
        basis,
        catalog,
        green_limit_layers,
    })
}

/// A backend's memory requirement (GPU-resident weights, system-RAM
/// weights, KV at its configured context, recurrent-state cache at
/// its effective slot count) under its current serving
/// configuration — see [`manager_models::memory_requirement`]. None
/// when the backend's model cannot be resolved.
fn backend_memory_requirement(
    cfg: &AppConfig,
    b: &manager_config::BackendConfig,
) -> Option<manager_models::MemoryRequirement> {
    let layout = resolve_weight_layout(cfg, b)?;
    let offload = manager_models::expert_offload(b.raw_flags.as_deref());
    let mut req = manager_models::memory::memory_requirement_with_tensors(
        layout.total_bytes,
        layout.expert_bytes,
        layout.expert_bytes_by_block.as_deref(),
        layout.basis,
        offload,
        kv_bytes_per_token_for(&layout, b.raw_flags.as_deref()),
        layout.moe_layers,
        b.flags.n_ctx,
        layout.rs_bytes_per_seq,
        manager_models::effective_slots(b.raw_flags.as_deref()),
        layout.expert_tensors.as_deref(),
        b.raw_flags.as_deref(),
    );
    // The expert cache rides the requirement: the decoded token
    // plus the layer geometry the requirement's per-GPU
    // distribution needs to price it (see
    // `manager_models::moe_cache_shares`), so every consumer of
    // the requirement — the start gate, residency, fit, the
    // placement projection — prices the cache the server will
    // allocate. A backend with no token prices exactly as before.
    req.moe_cache_mib = manager_models::moe_cache_mib(b.raw_flags.as_deref()).unwrap_or(0) as u64;
    req.moe_cache_offload_layers = layout.offloaded_moe_layers(offload);
    req.moe_cache_moe_layers = layout.moe_layers.unwrap_or(0);
    Some(req)
}

/// KV bytes per token for a backend under its SELECTED cache
/// types: the layout's KV geometry priced at the types decoded
/// from the backend's raw flags (see
/// [`manager_models::cache_selection`]), so the requirement —
/// and with it the start gate and every footprint figure the app
/// shows — prices the configuration actually selected, not the
/// f16 default. A backend with no cache tokens decodes to the
/// f16 default and prices exactly as before this feature. When
/// the layout carries no geometry, the f16 figure (if any) is
/// all that exists and is used unchanged.
fn kv_bytes_per_token_for(layout: &WeightLayout, raw_flags: Option<&str>) -> Option<u64> {
    match layout.kv_geometry {
        Some(geo) => {
            let sel = manager_models::cache_selection(raw_flags);
            Some(geo.bytes_per_token_typed(sel.k, sel.v))
        }
        None => layout.kv_per_token,
    }
}

struct CacheInfo {
    /// The decoded (effective) cache selection.
    selection: manager_models::CacheSelection,
    /// Whether any cache token is present in the saved flags —
    /// false for a pre-feature backend, whose row renders the
    /// controls at the decoded defaults without implying a saved
    /// setting.
    present: bool,
    /// True when the saved spelling is not the canonical form
    /// for its state (a `-ctk` or `=` spelling, an
    /// absent-member selection, ...). The same comparison as the
    /// MTP control's `custom`.
    custom: bool,
}

/// A backend's cache control state. Unlike the offload and MTP
/// states this is not capability gated: the cache controls render
/// for every configured backend, so every configured row carries
/// it (the Parallel slots field's posture).
fn backend_cache_info(b: &manager_config::BackendConfig) -> CacheInfo {
    let selection = manager_models::cache_selection(b.raw_flags.as_deref());
    let present = manager_models::cache_tokens_present(b.raw_flags.as_deref());
    let canonical = if present {
        manager_models::set_cache_selection(b.raw_flags.as_deref(), Some(selection))
    } else {
        b.raw_flags.clone()
    };
    let custom = token_normalized_flags(b.raw_flags.as_deref())
        != token_normalized_flags(canonical.as_deref());
    CacheInfo {
        selection,
        present,
        custom,
    }
}

/// The `cache` object a backend row carries: the decoded
/// selection, its presence, and the custom-spelling marker.
fn cache_row_json(c: &CacheInfo) -> Value {
    json!({
        "k": c.selection.k.as_str(),
        "v": c.selection.v.as_str(),
        "flash_attn": c.selection.flash_attn.as_str(),
        "present": c.present,
        "custom": c.custom,
    })
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
    /// The model's MoE block map when the GGUF was parsed: `layers`
    /// and every position written back convert through it
    /// ([`OffloadInfo::offload_for`]). `None` = identity (catalog
    /// or estimated layout).
    block_map: Option<manager_models::MoeBlockMap>,
}

impl OffloadInfo {
    /// The offload a slider position (`layers` MoE layers on
    /// system RAM) writes: `--n-cpu-moe` at the block-index bound
    /// that moves exactly that many MoE layers (see
    /// [`manager_models::offload_for_moe_layers`]).
    fn offload_for(&self, layers: u32) -> manager_models::ExpertOffload {
        manager_models::offload_for_moe_layers(layers, self.moe_layers, self.block_map.as_ref())
    }

    /// The `--n-cpu-moe` value each position `0..=moe_layers`
    /// writes (the identity without a block map).
    fn n_cpu_moe_by_position(&self) -> Vec<u32> {
        (0..=self.moe_layers)
            .map(|k| match &self.block_map {
                Some(map) => map.bound_for_layers(k),
                None => k,
            })
            .collect()
    }
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
    // The slider counts MoE layers; the saved `--n-cpu-moe` value
    // is a block-index bound. With the GGUF's block map the
    // position reads back as the MoE blocks the bound moves (code
    // review v0.1.60 item 11); without one it is the value itself.
    let block_map = layout.moe_block_map();
    let layers = offload.moe_layers_offloaded(moe_layers, block_map.as_ref());
    // Custom spelling: rewriting the detected position back through
    // the canonical writer must reproduce the saved tokens exactly
    // (after token normalization, so spacing alone never counts).
    // With a block map the canonical form is the one the slider
    // writes for the position, so a bound between MoE blocks (or
    // past the last one) reads as custom.
    let canonical_offload = match (offload, &block_map) {
        (manager_models::ExpertOffload::CpuMoeLayers(_), Some(map)) => {
            manager_models::offload_for_moe_layers(layers, moe_layers, Some(map))
        }
        _ => offload,
    };
    let canonical = manager_models::set_expert_offload(b.raw_flags.as_deref(), canonical_offload);
    let custom = token_normalized_flags(b.raw_flags.as_deref())
        != token_normalized_flags(canonical.as_deref());
    let restrictions = layout
        .catalog
        .as_ref()
        .map(|c| c.offload_restrictions.clone())
        .unwrap_or_default();
    let sys_bytes_by_position = manager_models::memory::sys_bytes_by_position_with_tensors(
        expert_bytes,
        layout.expert_bytes_by_block.as_deref(),
        moe_layers,
        layout.expert_tensors.as_deref(),
    );
    Some(OffloadInfo {
        moe_layers,
        layers,
        block_map,
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
                // The placed compute-capability set the override
                // key names (guardrail-override UI): the card's
                // confirmation and the set/clear operations carry
                // exactly this set.
                "placed_ccs": format_placed_ccs(&eval.placed_ccs),
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
                "placed_ccs": format_placed_ccs(&eval.placed_ccs),
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
        // The `--n-cpu-moe` value each slider position writes
        // (index = MoE layers on system RAM): the block-index bound
        // that moves exactly that many MoE layers (review item 11).
        // Endpoints are written as no tokens / `-ot exps=CPU`; their
        // entries are the bounds for reference only. Additive.
        "n_cpu_moe_by_position": o.n_cpu_moe_by_position(),
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
/// Capability is the EFFECTIVE capability resolved live (see
/// [`manager_models::effective_mtp`]): the curated catalog's pin
/// where the catalog knows the model (by id or source URL), else
/// the operator's declaration on the model record — never
/// inferred from a model's name or file — so a backend whose
/// capability is unknown gets no control rather than a
/// speculative one.
fn backend_mtp_info(cfg: &AppConfig, b: &manager_config::BackendConfig) -> Option<MtpInfo> {
    let model = model_for_backend(cfg, b)?;
    if !manager_models::effective_mtp(&model.id, &model.source_url, model.mtp) {
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

struct MoeCacheInfo {
    /// The decoded `--moe-cache-mib` value, or `None` when the
    /// token is absent (off). A decoded 0 — the pin's own
    /// disabled value, never written by the control — also reads
    /// as `None`.
    mib: Option<u32>,
    /// True when the saved spelling is not the canonical form
    /// for its state (an `=` spelling, a decoded 0 token). The
    /// same comparison as the MTP control's `custom`.
    custom: bool,
    /// The backend's saved offload position (MoE layers on
    /// system RAM): the UI disables the control at 0, when there
    /// are no host-resident experts for the cache to hold.
    offload_layers: u32,
    /// The model's MoE layer count (the offload slider's
    /// maximum).
    moe_layers: u32,
}

/// A backend's expert-cache control state. Eligibility is the
/// offload slider's exactly: the state exists iff
/// [`backend_offload_info`] resolves (a resolvable layout with
/// expert bytes and a MoE layer count), so the control appears
/// for precisely the backends that can offload — MoE models only,
/// with effectiveness resolved from the same live layout facts.
fn backend_moe_cache_info(
    cfg: &AppConfig,
    b: &manager_config::BackendConfig,
) -> Option<MoeCacheInfo> {
    let offload = backend_offload_info(cfg, b)?;
    let mib = manager_models::moe_cache_mib(b.raw_flags.as_deref()).filter(|n| *n > 0);
    let canonical = manager_models::set_moe_cache(b.raw_flags.as_deref(), mib);
    let custom = token_normalized_flags(b.raw_flags.as_deref())
        != token_normalized_flags(canonical.as_deref());
    Some(MoeCacheInfo {
        mib,
        custom,
        offload_layers: offload.layers,
        moe_layers: offload.moe_layers,
    })
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
/// `weights_only` — true only when no backend is in scope (the
/// projection then covers model weights only, with no KV-cache or
/// compute allowance, so "fits" is not a load guarantee); with a
/// backend in scope the projection prices its full requirement
/// and `weights_only` is false.
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
                kv_bytes_per_token: None,
                default_offload_layers: m.default_offload_layers,
                offload_restrictions: Vec::new(),
                mtp: false,
            }
        }
    };
    // Backend-scoped residency: when the caller names the serving
    // backend (the backend card's Placement form does), the
    // projection prices that backend's FULL memory requirement —
    // weights, KV cache, state cache, and the expert cache
    // distributed by its own rule — so the panel's Required
    // figures are the start gate's numbers (before Oct 23, 2026
    // it projected weights only and could report "Fits" for a
    // configuration the gate refused). The system-RAM share rides
    // along in the payload. The config read finishes before
    // telemetry is touched (the telemetry accessor reads the
    // config while initializing).
    let (full_req, resident_bytes, sys_bytes, credit_cfg_split) = match &backend_id {
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
                Some((req, split_cfg)) => {
                    let resident = if req.sys_weights_mib > 0 {
                        Some(req.gpu_weights_mib * manager_models::MIB)
                    } else {
                        None
                    };
                    let sys = req.sys_weights_mib * manager_models::MIB;
                    (Some(req), resident, sys, Some(split_cfg))
                }
                None => (None, None, 0, None),
            }
        }
        None => (None, None, 0, None),
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
    // With a backend in scope the projection is the full
    // requirement under the PROPOSED split (the expert-cache
    // shares follow the proposed split's offloaded-layer holders,
    // exactly as residency_per_gpu distributes them for the
    // start gate); the credit is the same requirement under the
    // configured split, when the backend is running. Without a
    // backend the legacy weights-only projection stands.
    let (projection, weights_only) = match &full_req {
        Some(req) => {
            let n = samples.len();
            let required = manager_optimize::residency_per_gpu(req, &split, n);
            let current: Vec<u64> = match (&current_split, credit_active) {
                (Some(cur), true) => manager_optimize::residency_per_gpu(req, cur, n),
                _ => vec![0; n],
            };
            let totals: Vec<u64> = samples.iter().map(|s| s.vram_total_mib).collect();
            let is_wddm: Vec<bool> = samples
                .iter()
                .map(|s| s.driver_model == Some(DriverModel::Wddm))
                .collect();
            (
                manager_models::propose_placement_full(
                    &spec,
                    &split,
                    &free_mib,
                    &totals,
                    &is_wddm,
                    &required,
                    &current,
                    req.gpu_weights_mib,
                    req.kv_mib.unwrap_or(0),
                    req.rs_mib,
                    req.moe_cache_mib,
                    sys_bytes,
                )
                .map_err(ApiError::from)?,
                false,
            )
        }
        None => (
            manager_models::propose_placement_resident(
                &spec,
                &split,
                &free_mib,
                resident_bytes,
                sys_bytes,
                current_split.as_deref(),
            )
            .map_err(ApiError::from)?,
            true,
        ),
    };
    let mut payload = serde_json::to_value(&projection)?;
    payload["snapshot_age_ms"] = json!(snapshot_age_ms);
    payload["gpu_indices"] = json!(gpu_indices);
    payload["down_indices"] = json!(down_indices);
    payload["weights_only"] = json!(weights_only);
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
            manager_models::apply_placement(backend, &split).map_err(ApiError::from)?;
        }
        // Record the (possibly derived) assignment on the model.
        if let Some(m) = new_cfg.models.iter_mut().find(|m| m.id == model_id) {
            for bid in &backend_ids {
                if !m.assigned_backends.contains(bid) {
                    m.assigned_backends.push(bid.clone());
                }
            }
        }
        manager_config::validate(&new_cfg).map_err(ApiError::from)?;
        // Journal before the save (ordering rule): each target
        // backend activates the split at its next (re)start — a
        // backend-scope plan entry per target (the gateway YAML
        // does not carry splits, so no gateway entry is created
        // for this change).
        let mut journaled = activation::JournalRecords::none();
        for bid in &backend_ids {
            let record = record_backend_change(
                state,
                &pre_cfg,
                &new_cfg,
                "backend_placement",
                format!("Backend `{bid}` tensor split changed"),
                bid,
            )
            .await;
            chain_journaled(state, &mut journaled, record)?;
        }
        // Persist — without the save, the applied split silently
        // vanished on app restart while the command reported success.
        save_journaled(state, &new_cfg, &journaled)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    /// P-4 (docs/code-review-v0.1.60.md): a header still `Incomplete`
    /// at the largest prefix rung is cached as a failure (keyed by
    /// length + mtime, so a changed file is parsed afresh).
    #[test]
    fn gguf_layout_incomplete_at_last_rung_is_cached() {
        let dir = unique_test_dir("llm-mgr-p4-gguf");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("crafted.gguf");
        // GGUF v3, 0 tensors, 1 metadata pair whose key claims 1000
        // bytes: incomplete at every prefix of this 64-byte file.
        let mut bytes = b"GGUF".to_vec();
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&1000u64.to_le_bytes());
        bytes.resize(64, b'k');
        std::fs::write(&path, &bytes).unwrap();
        let entries_for = |p: &PathBuf| -> Vec<u64> {
            GGUF_LAYOUT_CACHE
                .get()
                .map(|c| {
                    c.lock()
                        .unwrap()
                        .iter()
                        .filter(|((kp, _, _), v)| kp == p && v.is_none())
                        .map(|((_, len, _), _)| *len)
                        .collect()
                })
                .unwrap_or_default()
        };
        assert!(gguf_layout_cached_with(&path, &[16, 32]).is_none());
        assert_eq!(entries_for(&path), vec![64]);
        // A changed file (new length) is a new key, parsed afresh.
        bytes.resize(80, b'k');
        std::fs::write(&path, &bytes).unwrap();
        assert!(gguf_layout_cached_with(&path, &[16, 32]).is_none());
        let mut lens = entries_for(&path);
        lens.sort_unstable();
        assert_eq!(lens, vec![64, 80]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// PS-12 (docs/code-review-v0.1.60.md): a poisoned
    /// `verifying_downloads` lock must not panic the start preflight;
    /// the gate still reports the in-flight verification.
    #[tokio::test]
    async fn start_preflight_tolerates_poisoned_verifying_lock() {
        let mut cfg = test_cfg();
        cfg.backends
            .iter_mut()
            .find(|b| b.id == "coder")
            .unwrap()
            .enabled = true;
        let (state, dir) = advanced_test_state(cfg);
        // Seed the catalog so the coder backend's model is a known row.
        ensure_catalog_models(&state).await;
        let cfg = state.config.read().await.clone();
        let b = cfg
            .backends
            .iter()
            .find(|b| b.id == "coder")
            .unwrap()
            .clone();
        let target = resolve_under_data_dir(&cfg.data_dir, &b.model_file);
        let model_id = cfg
            .models
            .iter()
            .find(|m| resolve_under_data_dir(&cfg.data_dir, &m.gguf_path) == target)
            .expect("the coder backend's model is in the catalog")
            .id
            .clone();
        state
            .verifying_downloads
            .lock()
            .unwrap()
            .insert(model_id.clone());
        let poisoner = Arc::clone(&state);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.verifying_downloads.lock().unwrap();
            panic!("poison the verifying_downloads lock");
        })
        .join();
        assert!(state.verifying_downloads.is_poisoned());
        let err = backend_start_preflight_for(&state, &cfg, &b, false)
            .await
            .expect_err("a model under verification refuses the start");
        assert!(
            err.to_string().contains("verification is still running"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
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

    /// Prediction-refresh precondition (the backend card's
    /// Refresh control mirrors it): the measure op — the
    /// Measure profile, whose entries feed the prediction —
    /// refuses a backend that is not running.
    #[tokio::test]
    async fn optimize_measure_backend_refuses_a_stopped_backend() {
        let (state, dir) = advanced_test_state(test_cfg());
        let sink = test_sink();
        let r = op_optimize_measure_backend(&state, &sink, "planner".to_string()).await;
        match r {
            Err(ApiError::InvalidInput(msg)) => {
                assert!(msg.contains("not running"), "{msg}");
                assert!(msg.contains("measurements"), "{msg}");
            }
            other => panic!("expected the not-running refusal, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Prediction-refresh precondition: the shared GPU-job
    /// slot admits one job at a time, so a measure requested
    /// while another measurement or experiment job runs is
    /// refused — the refusal the card surfaces instead of
    /// failing silently.
    #[tokio::test]
    async fn optimize_measure_backend_refuses_while_a_gpu_job_runs() {
        let (state, dir) = advanced_test_state(test_cfg());
        let sink = test_sink();
        let (tx, _rx) = tokio::sync::oneshot::channel::<()>();
        *state.optimize_job_abort.lock().await = Some(tx);
        let r = op_optimize_measure_backend(&state, &sink, "planner".to_string()).await;
        match r {
            Err(ApiError::InvalidInput(msg)) => {
                assert!(msg.contains("already running"), "{msg}")
            }
            other => panic!("expected the job-slot refusal, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The backend card's Predicted throughput field reads the
    /// list_backends row: once a benchmark entry exists under a
    /// backend's current fingerprint — what a completed Measure
    /// (the card's Refresh) appends to the store — the row
    /// resolves it as `predicted`, and a backend with no entry
    /// carries no `predicted` key (the card renders its empty
    /// state for the absent field; the payload never sends a
    /// null-as-zero).
    #[tokio::test]
    async fn list_backends_resolves_predicted_from_a_stored_measurement() {
        let (state, dir) = advanced_test_state(test_cfg());
        // Hermetic telemetry: the fingerprint below is computed with
        // no GPUs, so list_backends must not see live host GPUs.
        let _ = state
            .telemetry
            .set(Arc::new(std::sync::Mutex::new(TelemetryStore::new())));
        let sink = test_sink();
        let cfg = state.config.read().await.clone();
        let planner = cfg
            .backends
            .iter()
            .find(|b| b.id == "planner")
            .expect("default config seeds planner");
        let fp = manager_optimize::measurement_fingerprint(&optimize::fingerprint_inputs(
            &cfg,
            planner,
            &[],
        ));
        let bench = |gen: f64| manager_optimize::BenchResult {
            prompt_per_second: Some(900.0),
            predicted_per_second: Some(gen),
            ttft_ms: Some(200.0),
            runs: Vec::new(),
        };
        let mut store = manager_optimize::MeasurementStore::default();
        store.upsert(manager_optimize::MeasurementEntry {
            fingerprint: fp,
            backend_id: "planner".to_string(),
            model_id: "model".to_string(),
            measured_at: "2026-10-20T00:00:00Z".to_string(),
            app_version: "test".to_string(),
            short: bench(22.5),
            long: bench(21.0),
            source: Some(manager_optimize::MeasurementSource::Benchmark),
            origin_backend_id: None,
            model_file: None,
            model_sha256: None,
            model_size_bytes: None,
            inputs: None,
            driver_version: Vec::new(),
            llama_build: None,
            residency: None,
            peak_temp_c: None,
        });
        store.save(&optimize::measurements_path(&state)).unwrap();
        let r = op_list_backends(&state, &sink).await.unwrap();
        let rows = r["backends"].as_array().unwrap();
        let prow = rows
            .iter()
            .find(|row| row["id"] == json!("planner"))
            .expect("planner row present");
        assert_eq!(prow["predicted"]["predicted_per_second"], json!(22.5));
        assert_eq!(prow["predicted"]["source"], json!("benchmark"));
        let other = cfg
            .backends
            .iter()
            .map(|b| b.id.clone())
            .find(|id| id != "planner")
            .expect("a second configured backend exists");
        let orow = rows
            .iter()
            .find(|row| row["id"] == json!(other))
            .expect("second backend row present");
        assert!(
            orow.get("predicted").is_none(),
            "row for {other} must omit predicted: {orow}"
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
        assert_eq!(
            coder.raw_flags.as_deref(),
            Some("--n-cpu-moe=46  --threads 8")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn raw_flags_save_refuses_an_override_comma_llama_server_would_split() {
        let (state, dir) = advanced_test_state(test_cfg());
        let sink = test_sink();
        op_backend_set_flags_mode(&state, &sink, "planner".to_string(), true)
            .await
            .unwrap();
        let err = op_backend_set_raw_flags(
            &state,
            &sink,
            "planner".to_string(),
            Some(r#"-ot "blk\.[0-9]{1,2}\.ffn_.*_exps=CPU""#.to_string()),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&err, ApiError::InvalidInput(m) if m.contains("alternation")),
            "{err}"
        );
        // Nothing was saved.
        let cfg = state.config.read().await.clone();
        let b = cfg.backends.iter().find(|b| b.id == "planner").unwrap();
        assert!(b.raw_flags.as_deref().is_none_or(|r| !r.contains("{1,2}")));
        // The alternation spelling saves.
        let r = op_backend_set_raw_flags(
            &state,
            &sink,
            "planner".to_string(),
            Some(r#"-ot "blk\.([0-9]|[1-9][0-9])\.ffn_.*_exps=CPU""#.to_string()),
        )
        .await
        .unwrap();
        assert_eq!(r["changed"], true);
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
        let r =
            op_backend_set_raw_flags(&state, &sink, "planner".to_string(), Some(RAW.to_string()))
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
        let r =
            op_backend_set_raw_flags(&state, &sink, "planner".to_string(), Some(RAW.to_string()))
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
        let r = op_backend_set_raw_flags(
            &state,
            &sink,
            "planner".to_string(),
            Some("   ".to_string()),
        )
        .await
        .unwrap();
        assert_eq!(r["changed"], true);
        assert_eq!(r["raw_flags"], Value::Null);
        let saved = manager_config::load(&state.config_path).unwrap();
        assert_eq!(
            saved
                .backends
                .iter()
                .find(|b| b.id == "planner")
                .unwrap()
                .raw_flags,
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
        let r = op_update_config(
            &state,
            &sink,
            json!({ "supervision": { "mode": "process" } }),
        )
        .await
        .unwrap();
        assert_eq!(r["ok"], true);
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
        let r =
            op_backend_set_raw_flags(&state, &sink, "planner".to_string(), Some(RAW.to_string()))
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
        assert_eq!(
            manager_models::parallel_slots(b.raw_flags.as_deref()),
            Some(1)
        );
        let info = backend_mtp_info(&saved, b).expect("planner-mtp is MTP-capable");
        assert!(info.enabled);
        assert_eq!(info.n_max, Some(2));
        assert_eq!(info.parallel, Some(1));
        let _ = std::fs::remove_dir_all(&dir);
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
        let eval =
            evaluate_backend_offload(&cfg, &b, &gpus, &journal).expect("coder backend evaluates");
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
        let eval =
            evaluate_backend_offload(&cfg, &b, &gpus, &journal).expect("coder backend evaluates");
        assert!(eval.has_override);
        assert!(eval.overridden);

        // Same position on an unrestricted GPU: no restriction
        // matches, nothing is blocked.
        let gpus = test_gpus();
        let eval =
            evaluate_backend_offload(&cfg, &b, &gpus, &journal).expect("coder backend evaluates");
        assert_eq!(eval.restricting_cc, None);
        assert_ne!(eval.status, manager_models::OffloadStatus::Blocked);
    }

    /// State for the guardrail-override op tests: the `coder`
    /// backend serves Coder-Next at a raw-flags position, with the
    /// telemetry inventory pinned so GPU 0 is a Pascal (6,0) card —
    /// the #37 field configuration.
    fn guardrail_override_test_state(raw_flags: &str) -> (Arc<AppState>, PathBuf) {
        let mut cfg = test_cfg();
        serve_model(
            &mut cfg,
            "coder",
            "qwen3-coder-30b",
            "models/Qwen3-Coder-Next-UD-Q4_K_M.gguf",
            Some(raw_flags),
        );
        // The preflight refuses a disabled backend before it ever
        // reaches the guardrail gate; the test fleet's coder ships
        // disabled, so enable it here.
        cfg.backends
            .iter_mut()
            .find(|b| b.id == "coder")
            .unwrap()
            .enabled = true;
        let (state, dir) = advanced_test_state(cfg);
        let mut gpus = test_gpus();
        gpus[0].compute_capability = (6, 0);
        let store = state
            .telemetry
            .get()
            .expect("advanced_test_state pins telemetry");
        lock_telemetry(store).set_inventory(gpus);
        (state, dir)
    }

    /// The launch preflight's verdict on the guardrail alone, for
    /// the saved `coder` backend: `Some(reason)` when the start is
    /// refused for the guardrail reason. Later preflight gates
    /// (llama-server presence, the model file) fail in this test
    /// environment for unrelated reasons, so the assertion is on
    /// the guardrail reason's presence, never on overall success.
    async fn guardrail_preflight_reason(state: &Arc<AppState>) -> Option<String> {
        let cfg = state.config.read().await.clone();
        let b = cfg
            .backends
            .iter()
            .find(|b| b.id == "coder")
            .unwrap()
            .clone();
        match backend_start_preflight_for(state, &cfg, &b, false).await {
            Err(e) => {
                let msg = e.to_string();
                msg.contains("outside the verified range").then_some(msg)
            }
            Ok(()) => None,
        }
    }

    #[tokio::test]
    async fn offload_override_ops_round_trip_set_then_clear() {
        // The deliberate-override round trip (guardrail-override
        // UI): a saved blocked position refuses starts; setting the
        // override for exactly that position lets the preflight's
        // guardrail gate pass; clearing it blocks starts again.
        let (state, dir) = guardrail_override_test_state("--n-cpu-moe 46");
        let sink = test_sink();
        assert!(
            guardrail_preflight_reason(&state).await.is_some(),
            "46 of 48 on (6,0) refuses starts before any override"
        );
        let r = op_backend_set_offload_override(
            &state,
            &sink,
            "coder".to_string(),
            46,
            vec!["6.0".to_string()],
        )
        .await
        .unwrap();
        assert_eq!(r["backend_id"], "coder");
        assert_eq!(r["layers"], 46);
        assert_eq!(r["moe_layers"], 48);
        assert_eq!(r["placed_ccs"], json!(["6.0"]));
        assert_eq!(r["has_override"], true);
        assert_eq!(r["overridden"], true);
        // The journal record's shape: exactly the #37 key plus its
        // RFC 3339 timestamp, and nothing else was recorded.
        let journal = activation::load_journal(&dir);
        assert_eq!(journal.offload_overrides.len(), 1);
        let record = &journal.offload_overrides[0];
        assert_eq!(record.backend_id, "coder");
        assert_eq!(record.layers, 46);
        assert_eq!(record.placed_ccs, vec![(6, 0)]);
        assert!(
            chrono::DateTime::parse_from_rfc3339(&record.ts).is_ok(),
            "override timestamp is RFC 3339: {}",
            record.ts
        );
        // The op writes no config: the backend's flags are
        // untouched in the live config (and no config save was
        // needed to record the override).
        let cfg_now = state.config.read().await.clone();
        let b = cfg_now.backends.iter().find(|b| b.id == "coder").unwrap();
        assert_eq!(b.raw_flags.as_deref(), Some("--n-cpu-moe 46"));
        assert!(
            guardrail_preflight_reason(&state).await.is_none(),
            "the overridden position passes the preflight guardrail gate"
        );
        let r = op_backend_clear_offload_override(&state, &sink, "coder".to_string())
            .await
            .unwrap();
        assert_eq!(r["cleared"], true);
        assert_eq!(r["has_override"], false);
        assert_eq!(r["overridden"], false);
        let journal = activation::load_journal(&dir);
        assert!(journal.offload_overrides.is_empty());
        assert!(
            guardrail_preflight_reason(&state).await.is_some(),
            "after the clear the position is blocked again"
        );
        // Clearing with nothing standing is an idempotent success.
        let r = op_backend_clear_offload_override(&state, &sink, "coder".to_string())
            .await
            .unwrap();
        assert_eq!(r["cleared"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn offload_override_set_refuses_key_mismatch() {
        // An override is keyed to exactly (backend, layers, placed
        // CC set): naming any other key at the set boundary refuses
        // and records nothing, and a recorded key covers no other.
        let (state, dir) = guardrail_override_test_state("--n-cpu-moe 46");
        let sink = test_sink();
        let r = op_backend_set_offload_override(
            &state,
            &sink,
            "coder".to_string(),
            45,
            vec!["6.0".to_string()],
        )
        .await;
        match r {
            Err(ApiError::InvalidInput(msg)) => {
                assert!(msg.contains("do not match"), "{msg}")
            }
            other => panic!("expected the layers-mismatch refusal, got {other:?}"),
        }
        let r = op_backend_set_offload_override(
            &state,
            &sink,
            "coder".to_string(),
            46,
            vec!["8.6".to_string()],
        )
        .await;
        match r {
            Err(ApiError::InvalidInput(msg)) => {
                assert!(msg.contains("does not match"), "{msg}")
            }
            other => panic!("expected the placed-CC mismatch refusal, got {other:?}"),
        }
        // A superset placement is a different key, not a wider one.
        let r = op_backend_set_offload_override(
            &state,
            &sink,
            "coder".to_string(),
            46,
            vec!["6.0".to_string(), "8.6".to_string()],
        )
        .await;
        assert!(matches!(r, Err(ApiError::InvalidInput(_))), "{r:?}");
        for bad in ["6", "6.0.1", "x.y", ""] {
            let r = op_backend_set_offload_override(
                &state,
                &sink,
                "coder".to_string(),
                46,
                vec![bad.to_string()],
            )
            .await;
            assert!(
                matches!(r, Err(ApiError::InvalidInput(_))),
                "{bad:?}: {r:?}"
            );
        }
        let journal = activation::load_journal(&dir);
        assert!(
            journal.offload_overrides.is_empty(),
            "refused sets record nothing"
        );
        // The exact key records; neighboring keys do not cover.
        op_backend_set_offload_override(
            &state,
            &sink,
            "coder".to_string(),
            46,
            vec!["6.0".to_string()],
        )
        .await
        .unwrap();
        let journal = activation::load_journal(&dir);
        assert!(activation::offload_override_covers(
            &journal,
            "coder",
            46,
            &[(6, 0)]
        ));
        assert!(!activation::offload_override_covers(
            &journal,
            "coder",
            45,
            &[(6, 0)]
        ));
        assert!(!activation::offload_override_covers(
            &journal,
            "coder",
            46,
            &[(8, 6)]
        ));
        assert!(!activation::offload_override_covers(
            &journal,
            "coder",
            46,
            &[(6, 0), (8, 6)]
        ));
        assert!(!activation::offload_override_covers(
            &journal,
            "planner",
            46,
            &[(6, 0)]
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn offload_override_set_refuses_non_blocked_and_unknown() {
        let sink = test_sink();
        // An endpoint position is verified on (6,0): there is
        // nothing to override, and the op refuses rather than
        // leaving a standing record no reader could see.
        let (state, dir) = guardrail_override_test_state("-ot exps=CPU");
        let r = op_backend_set_offload_override(
            &state,
            &sink,
            "coder".to_string(),
            48,
            vec!["6.0".to_string()],
        )
        .await;
        match r {
            Err(ApiError::InvalidInput(msg)) => {
                assert!(msg.contains("not guardrail-blocked"), "{msg}")
            }
            other => panic!("expected the non-blocked refusal, got {other:?}"),
        }
        let journal = activation::load_journal(&dir);
        assert!(journal.offload_overrides.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
        // Unknown backend: NotFound from both operations.
        let (state, dir) = guardrail_override_test_state("--n-cpu-moe 46");
        let r = op_backend_set_offload_override(
            &state,
            &sink,
            "nope".to_string(),
            46,
            vec!["6.0".to_string()],
        )
        .await;
        assert!(matches!(r, Err(ApiError::NotFound(_))), "{r:?}");
        let r = op_backend_clear_offload_override(&state, &sink, "nope".to_string()).await;
        assert!(matches!(r, Err(ApiError::NotFound(_))), "{r:?}");
        // A backend whose model is not offload-capable has no
        // position to override.
        let r = op_backend_set_offload_override(
            &state,
            &sink,
            "planner".to_string(),
            1,
            vec!["6.0".to_string()],
        )
        .await;
        match r {
            Err(ApiError::InvalidInput(msg)) => {
                assert!(msg.contains("does not support expert offload"), "{msg}")
            }
            other => panic!("expected the dense-backend refusal, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn offload_guardrail_payload_names_placed_ccs() {
        // The card's confirmation and the override operations carry
        // the placed-CC set the payload names.
        let mut cfg = test_cfg();
        let b = serve_model(
            &mut cfg,
            "coder",
            "qwen3-coder-30b",
            "models/Qwen3-Coder-Next-UD-Q4_K_M.gguf",
            Some("--n-cpu-moe 46"),
        );
        let mut gpus = test_gpus();
        gpus[0].compute_capability = (6, 0);
        let journal = activation::ActivationJournal::default();
        let eval =
            evaluate_backend_offload(&cfg, &b, &gpus, &journal).expect("coder backend evaluates");
        let info = backend_offload_info(&cfg, &b).expect("coder info");
        let row = offload_row_json(&info, Some(&eval));
        let guardrail = &row["guardrail"];
        assert_eq!(guardrail["status"], "blocked");
        assert_eq!(guardrail["placed_ccs"], json!(["6.0"]));
        assert_eq!(guardrail["has_override"], false);
        assert_eq!(guardrail["overridden"], false);
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
            err.to_string()
                .contains("parallel slots must be between 1 and 1024"),
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

    fn vram_reading(index: u32, total_mib: u64, free_mib: u64) -> VramGateReading {
        VramGateReading {
            index,
            total_mib,
            free_mib,
            down: false,
            driver_model: None,
        }
    }

    fn vram_reading_down(index: u32, total_mib: u64, free_mib: u64) -> VramGateReading {
        VramGateReading {
            index,
            total_mib,
            free_mib,
            down: true,
            driver_model: None,
        }
    }

    fn vram_reading_driver(
        index: u32,
        total_mib: u64,
        free_mib: u64,
        driver_model: DriverModel,
    ) -> VramGateReading {
        VramGateReading {
            index,
            total_mib,
            free_mib,
            down: false,
            driver_model: Some(driver_model),
        }
    }

    #[test]
    fn vram_gate_passes_when_requirement_fits_free() {
        // 12 GiB required on GPU 1, 15 GiB free of 16 GiB: fits.
        let failures = vram_gate_failures(
            &[1],
            &[(1, 12 * 1024)],
            &[vram_reading(1, 16 * 1024, 15 * 1024)],
            &[],
        );
        assert!(failures.is_empty());
    }

    #[test]
    fn vram_gate_refuses_when_free_is_short() {
        // The planner-moe shape: ~15.5 GiB required on the P100,
        // ~11 GiB free because another backend is resident. Under
        // the card total, so the cause is insufficient free memory.
        let failures = vram_gate_failures(
            &[1],
            &[(1, 15_862)],
            &[vram_reading(1, 16_384, 11_245)],
            &[],
        );
        assert_eq!(failures.len(), 1);
        let f = &failures[0];
        assert_eq!(f.index, 1);
        assert_eq!(f.required_mib, 15_862);
        assert_eq!(f.free_mib, 11_245);
        assert_eq!(f.total_mib, 16_384);
        assert_eq!(f.class, VramGateClass::InsufficientFree);
    }

    #[test]
    fn preflight_requirement_includes_recurrent_state_cache() {
        // The planner-moe incident end to end through the
        // production requirement: Whittle-MoE-27B Q8_0 file
        // (27,350 MiB of tensors, 13,536 MiB of them routed
        // experts, all offloaded), KV 65,536 B/token at n_ctx
        // 32768, recurrent-state geometry 156,893,184 B per
        // sequence at the auto slot count of 4. llama.cpp's own
        // figures for this configuration total 16,460 MiB
        // (13,813.54 weights + 598.50 state + 2,048 KV) — over a
        // 16,384 MiB card's total even when the card is empty, so
        // the gate must refuse with the exceeds-total class. The
        // weights+KV subtotal alone (15,862 MiB) is exactly the
        // figure the pre-rs-term requirement produced when the
        // start was wrongly allowed.
        let mib_bytes = |m: u64| m * manager_models::MIB;
        let req = manager_models::memory_requirement(
            mib_bytes(27_350),
            Some(mib_bytes(13_536)),
            None,
            manager_models::MemoryBasis::Exact,
            manager_models::ExpertOffload::AllExperts,
            Some(65_536),
            Some(64),
            32_768,
            Some(156_893_184),
            4,
        );
        assert_eq!(req.gpu_weights_mib, 13_814);
        assert_eq!(req.kv_mib, Some(2_048));
        assert_eq!(req.rs_mib, Some(599));
        let per_gpu = manager_optimize::residency_per_gpu(&req, &[0.0, 1.0], 2);
        assert_eq!(per_gpu, vec![0, 16_461]);
        let required: Vec<(u32, u64)> = per_gpu
            .iter()
            .enumerate()
            .filter(|(_, mib)| **mib > 0)
            .map(|(i, mib)| (i as u32, *mib))
            .collect();
        let failures = vram_gate_failures(&[1], &required, &[vram_reading(1, 16_384, 16_384)], &[]);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].class, VramGateClass::ExceedsTotal);
        assert_eq!(failures[0].required_mib, 16_461);
        assert!(failures[0].required_mib >= 16_460);
    }

    #[test]
    fn vram_gate_classifies_requirement_over_card_total() {
        // More required than the card holds at all: a different
        // cause class than a busy card, even with every byte free.
        let failures = vram_gate_failures(
            &[0],
            &[(0, 17 * 1024)],
            &[vram_reading(0, 16 * 1024, 16 * 1024)],
            &[],
        );
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].class, VramGateClass::ExceedsTotal);
        assert_eq!(failures[0].free_mib, 16 * 1024);
    }

    #[test]
    fn vram_gate_split_flags_only_the_short_gpu() {
        // Split across two GPUs: GPU 0 has room, GPU 1 does not.
        // Only the short GPU is named.
        let failures = vram_gate_failures(
            &[0, 1],
            &[(0, 8 * 1024), (1, 8 * 1024)],
            &[
                vram_reading(0, 16 * 1024, 16 * 1024),
                vram_reading(1, 16 * 1024, 4 * 1024),
            ],
            &[],
        );
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].index, 1);
    }

    #[test]
    fn vram_gate_credit_covers_a_restart() {
        // A restart of a backend already holding most of the card:
        // measured free is tiny, but its own residency is credited
        // back (the restart frees it before allocating), so the
        // same configuration passes its own gate.
        let failures = vram_gate_failures(
            &[1],
            &[(1, 15 * 1024)],
            &[vram_reading(1, 16 * 1024, 1024)],
            &[(1, 14_500)],
        );
        assert!(failures.is_empty());
        // A requirement grown past what the credit covers still
        // fails: 15 GiB required, 1 GiB free + 13 GiB credited.
        let failures = vram_gate_failures(
            &[1],
            &[(1, 15 * 1024)],
            &[vram_reading(1, 16 * 1024, 1024)],
            &[(1, 13 * 1024)],
        );
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].free_mib, 1024 + 13 * 1024);
    }

    #[test]
    fn vram_gate_down_gpu_is_unavailable_not_short() {
        // The thermal-offline shape: the placed GPU fell out of
        // the telemetry polls and is retained flagged down with
        // last-known figures that would otherwise "fit". The
        // class is unavailable, not a memory verdict — the
        // figures are not trustworthy.
        let failures = vram_gate_failures(
            &[1],
            &[(1, 12 * 1024)],
            &[vram_reading_down(1, 16 * 1024, 15 * 1024)],
            &[],
        );
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].class, VramGateClass::GpuUnavailable);
        assert_eq!(failures[0].index, 1);
    }

    #[test]
    fn vram_gate_missing_gpu_in_live_snapshot_is_unavailable() {
        // Telemetry reads GPU 0 fine but has never reported the
        // placed GPU 1 (the app started while the card was
        // offline): unavailable, not a skip.
        let failures = vram_gate_failures(
            &[1],
            &[(1, 12 * 1024)],
            &[vram_reading(0, 16 * 1024, 16 * 1024)],
            &[],
        );
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].class, VramGateClass::GpuUnavailable);
        assert_eq!(failures[0].index, 1);
    }

    #[test]
    fn vram_gate_zero_total_is_unavailable() {
        // A reading whose total memory is zero is a device not
        // responding with real figures, not a very full card.
        let failures = vram_gate_failures(&[0], &[(0, 1024)], &[vram_reading(0, 0, 0)], &[]);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].class, VramGateClass::GpuUnavailable);
    }

    #[test]
    fn vram_gate_wddm_over_total_is_confirmable_with_paged_amount() {
        // Pete's configuration (Oct 23, 2026): 17,224 MiB required
        // on a 16,376 MiB WDDM GPU (the expert cache is 4,096 of
        // it — without the cache the same backend needs 13,128
        // and passes outright). The failure carries the paged
        // excess, and the failure set is confirmable.
        let failures = vram_gate_failures(
            &[0],
            &[(0, 17_224)],
            &[vram_reading_driver(0, 16_376, 15_098, DriverModel::Wddm)],
            &[],
        );
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].class, VramGateClass::ExceedsTotal);
        assert_eq!(failures[0].paged_mib, 848);
        assert!(failures_are_wddm_overcommit(&failures));
        // Zero cache changes nothing about the gate's other
        // shapes: the uncached requirement fits free and passes.
        let failures = vram_gate_failures(
            &[0],
            &[(0, 13_128)],
            &[vram_reading_driver(0, 16_376, 15_098, DriverModel::Wddm)],
            &[],
        );
        assert!(failures.is_empty());
    }

    #[test]
    fn vram_gate_tcc_or_unknown_over_total_is_not_confirmable() {
        // The same excess on a TCC GPU (the P100 posture) is a
        // hard refusal: no paged amount, not confirmable. An
        // unreported driver model is judged the same way.
        for reading in [
            vram_reading_driver(0, 16_376, 15_098, DriverModel::Tcc),
            vram_reading(0, 16_376, 15_098),
        ] {
            let failures = vram_gate_failures(&[0], &[(0, 17_224)], &[reading], &[]);
            assert_eq!(failures.len(), 1);
            assert_eq!(failures[0].class, VramGateClass::ExceedsTotal);
            assert_eq!(failures[0].paged_mib, 0);
            assert!(!failures_are_wddm_overcommit(&failures));
        }
        // Insufficient free UNDER the total stays a hard refusal
        // even on WDDM: paging is judged against total VRAM, not
        // against transient free pressure.
        let failures = vram_gate_failures(
            &[0],
            &[(0, 15_000)],
            &[vram_reading_driver(0, 16_376, 10_000, DriverModel::Wddm)],
            &[],
        );
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].class, VramGateClass::InsufficientFree);
        assert!(!failures_are_wddm_overcommit(&failures));
        // A mixed set (one WDDM over-total, one unavailable) is
        // not confirmable as a whole.
        let failures = vram_gate_failures(
            &[0, 1],
            &[(0, 17_224), (1, 1024)],
            &[
                vram_reading_driver(0, 16_376, 15_098, DriverModel::Wddm),
                vram_reading_down(1, 16_376, 15_098),
            ],
            &[],
        );
        assert_eq!(failures.len(), 2);
        assert!(!failures_are_wddm_overcommit(&failures));
    }

    #[test]
    fn requirement_breakdown_itemizes_the_expert_cache() {
        // The refusal text's arithmetic: the itemized terms sum
        // to the gate's required figure, expert cache included.
        let req = manager_models::MemoryRequirement {
            gpu_weights_mib: 10_865,
            sys_weights_mib: 0,
            kv_mib: Some(1_638),
            rs_mib: Some(614),
            moe_cache_mib: 4_096,
            moe_cache_offload_layers: 48,
            moe_cache_moe_layers: 64,
            basis: manager_models::MemoryBasis::Exact,
        };
        assert_eq!(
            requirement_breakdown(&req),
            "weights 10865 MiB + KV cache 1638 MiB + state cache 614 MiB + expert cache 4096 MiB"
        );
        // Zero cache: no expert-cache term, nothing else changes.
        let req = manager_models::MemoryRequirement {
            moe_cache_mib: 0,
            ..req
        };
        assert_eq!(
            requirement_breakdown(&req),
            "weights 10865 MiB + KV cache 1638 MiB + state cache 614 MiB"
        );
    }

    #[test]
    fn vram_gate_empty_snapshot_judges_nothing() {
        // Telemetry has nothing at all (not started, no NVML):
        // the gate makes no claim in either direction.
        let failures = vram_gate_failures(&[1], &[(1, 12 * 1024)], &[], &[]);
        assert!(failures.is_empty());
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

    #[test]
    fn backend_spec_strips_raw_flag_quotes_and_keeps_backslashes() {
        // No shell runs llama-server: a quoted `-ot` value must
        // reach it unquoted (it refused buffer `CPU"` before), with
        // the regex's backslashes intact, exactly like the unquoted
        // spelling.
        let cfg = test_cfg();
        let mut b = backend(&cfg, "planner");
        let tail = |spec: &ProcessSpec| {
            let pos = spec.args.iter().position(|a| a == "-ot").unwrap();
            spec.args[pos..].to_vec()
        };
        b.raw_flags = Some(r"-ot blk\..=CPU --alias x".into());
        let plain = tail(&backend_spec(&cfg, &b, &[]));
        assert_eq!(plain, vec!["-ot", r"blk\..=CPU", "--alias", "x"]);
        b.raw_flags = Some(r#"-ot "blk\..=CPU" --alias 'x'"#.into());
        assert_eq!(tail(&backend_spec(&cfg, &b, &[])), plain);
        b.raw_flags = Some(r#"-ot "blk\..=CPU" --alias """#.into());
        assert_eq!(
            tail(&backend_spec(&cfg, &b, &[])),
            vec!["-ot", r"blk\..=CPU", "--alias", ""]
        );
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
            mtp: None,
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
    fn catalog_arm_requirement_reports_kv_for_planner_mtp() {
        // The Oct 20, 2026 defect: planner-mtp's card showed no KV
        // figure. Its backend resolves through the catalog arm
        // whenever the GGUF header does not parse (here: no file
        // on disk at all, the pre-download shape) — the catalog
        // arm must still supply the KV figure from the catalog's
        // pinned geometry: 65,536 B/token, 2,048 MiB at the
        // backend's n_ctx 32768.
        let mut cfg = test_cfg();
        let mut b = serve_model(
            &mut cfg,
            "planner",
            "qwen3.8-27b-mtp",
            "models/Qwen3.8-27B-GSQ-RCO-IQ3_S-mtp.gguf",
            None,
        );
        b.flags.n_ctx = 32_768;
        let layout = resolve_weight_layout(&cfg, &b).unwrap();
        assert_eq!(layout.basis, manager_models::MemoryBasis::Catalog);
        assert_eq!(layout.kv_per_token, Some(65_536));
        let req = backend_memory_requirement(&cfg, &b).unwrap();
        assert_eq!(req.kv_mib, Some(2_048));
        assert_eq!(req.basis, manager_models::MemoryBasis::Catalog);
    }

    #[test]
    fn catalog_arm_requirement_reports_kv_for_planner_and_coder() {
        // The same catalog-arm resolution for the base planner
        // (65,536 B/token — the figure its downloaded file's parse
        // reports, so the card's KV tile is identical before and
        // after download) and the coder (24,576 B/token, 768 MiB
        // at n_ctx 32768).
        let mut cfg = test_cfg();
        let mut b = serve_model(
            &mut cfg,
            "planner",
            "qwen3-27b-planner",
            "models/Qwen3.8-27B.gguf",
            None,
        );
        b.flags.n_ctx = 32_768;
        let req = backend_memory_requirement(&cfg, &b).unwrap();
        assert_eq!(req.kv_mib, Some(2_048));
        let mut cfg = test_cfg();
        let mut b = serve_model(
            &mut cfg,
            "coder",
            "qwen3-coder-30b",
            "models/Qwen3-Coder-Next-UD-Q4_K_M.gguf",
            Some("-ot exps=CPU"),
        );
        b.flags.n_ctx = 32_768;
        let req = backend_memory_requirement(&cfg, &b).unwrap();
        assert_eq!(req.kv_mib, Some(768));
    }

    /// A minimal GGUF header carrying complete KV geometry and
    /// no tensors: `blocks` blocks, `heads_kv` KV heads, key/value
    /// lengths `klen`/`vlen` under a test architecture.
    fn gguf_kv_header_bytes(blocks: u32, heads_kv: u32, klen: u32, vlen: u32) -> Vec<u8> {
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
        let mut b = Vec::new();
        b.extend_from_slice(b"GGUF");
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&0u64.to_le_bytes()); // tensor count
        b.extend_from_slice(&5u64.to_le_bytes()); // kv count
        w_kv_str(&mut b, "general.architecture", "testarch");
        w_kv_u32(&mut b, "testarch.block_count", blocks);
        w_kv_u32(&mut b, "testarch.attention.head_count_kv", heads_kv);
        w_kv_u32(&mut b, "testarch.attention.key_length", klen);
        w_kv_u32(&mut b, "testarch.attention.value_length", vlen);
        b
    }

    fn gguf_probe_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "llm-mgr-gguf-probe-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("models")).unwrap();
        dir
    }

    #[test]
    fn exact_arm_requirement_reports_the_gguf_kv_figure() {
        // A downloaded file that parses supplies the KV figure
        // from its own header (basis Exact) — for a model the
        // catalog does not carry at all, the header is the only
        // possible source: 4 blocks * 2 KV heads * (64 + 64) * 2 B
        // = 2,048 B/token, 64 MiB at n_ctx 32768.
        let dir = gguf_probe_dir("exact");
        std::fs::write(
            dir.join("models").join("user.gguf"),
            gguf_kv_header_bytes(4, 2, 64, 64),
        )
        .unwrap();
        let mut cfg = test_cfg();
        cfg.data_dir = dir.clone();
        register_test_model(&mut cfg, "user-kv-model", "models/user.gguf");
        let b = uncreated_backend("user-backend", "models/user.gguf", None);
        let layout = resolve_weight_layout(&cfg, &b).unwrap();
        assert_eq!(layout.basis, manager_models::MemoryBasis::Exact);
        assert_eq!(layout.kv_per_token, Some(2_048));
        let req = backend_memory_requirement(&cfg, &b).unwrap();
        assert_eq!(req.kv_mib, Some(64));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gguf_layout_recovers_when_a_missing_or_unparseable_file_is_replaced() {
        // Neither a missing file nor an unparseable one pins the
        // path: once the file is replaced by a parseable header,
        // the layout resolves. (The unparseable bytes are a
        // different file version — different length — so this
        // exercises the re-read path every poll takes when a
        // download lands or a file is rewritten.)
        let dir = gguf_probe_dir("recover");
        let path = dir.join("models").join("probe.gguf");
        assert!(gguf_layout_cached(&path).is_none());
        std::fs::write(&path, b"not a gguf file at all").unwrap();
        assert!(gguf_layout_cached(&path).is_none());
        std::fs::write(&path, gguf_kv_header_bytes(4, 2, 64, 64)).unwrap();
        let layout = gguf_layout_cached(&path).expect("replaced file parses");
        assert_eq!(layout.kv.unwrap().bytes_per_token(), 2_048);
        let _ = std::fs::remove_dir_all(&dir);
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

    /// A GGUF header whose first `dense_lead` blocks are dense and
    /// whose next `moe_blocks` blocks each carry one 1 MiB routed
    /// expert tensor (262,144 F32 elements), so offloaded bytes
    /// read directly as whole MiB.
    fn gguf_lead_dense_header(dense_lead: u32, moe_blocks: u32) -> Vec<u8> {
        fn w_str(out: &mut Vec<u8>, s: &str) {
            out.extend_from_slice(&(s.len() as u64).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        fn w_tensor(out: &mut Vec<u8>, name: &str, elems: u64) {
            w_str(out, name);
            out.extend_from_slice(&1u32.to_le_bytes());
            out.extend_from_slice(&elems.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&0u64.to_le_bytes());
        }
        let total = dense_lead + moe_blocks;
        let mut b = Vec::new();
        b.extend_from_slice(b"GGUF");
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&(total as u64).to_le_bytes());
        b.extend_from_slice(&2u64.to_le_bytes());
        w_str(&mut b, "general.architecture");
        b.extend_from_slice(&8u32.to_le_bytes());
        w_str(&mut b, "testarch");
        w_str(&mut b, "testarch.block_count");
        b.extend_from_slice(&4u32.to_le_bytes());
        b.extend_from_slice(&total.to_le_bytes());
        for i in 0..dense_lead {
            w_tensor(&mut b, &format!("blk.{i}.ffn_gate.weight"), 262_144);
        }
        for i in dense_lead..total {
            w_tensor(&mut b, &format!("blk.{i}.ffn_gate_exps.weight"), 262_144);
        }
        b
    }

    #[test]
    fn requirement_prices_ot_block_ranges_and_n_cpu_moe_block_indices() {
        // P-5 and the --n-cpu-moe check (review item 11), through
        // the app's own requirement and slider table: 3 leading
        // dense blocks then 37 MoE blocks, 1 MiB of experts each.
        let dir = gguf_probe_dir("p5");
        std::fs::write(
            dir.join("models").join("lead-dense.gguf"),
            gguf_lead_dense_header(3, 37),
        )
        .unwrap();
        let mut cfg = test_cfg();
        cfg.data_dir = dir.clone();
        register_test_model(&mut cfg, "lead-dense-moe", "models/lead-dense.gguf");
        cfg.models.last_mut().unwrap().moe_layers = Some(37);
        cfg.models.last_mut().unwrap().default_offload_layers = Some(10);
        let req = |raw: &str| {
            let b = uncreated_backend(
                "lead-dense",
                "models/lead-dense.gguf",
                Some(raw.to_string()),
            );
            backend_memory_requirement(&cfg, &b).unwrap()
        };
        // Block-range -ot recipe: blocks 0..=19 match, of which
        // 3..=19 carry experts — 17 MiB, not all 37.
        let r = req(r#"-ot "blk\.([0-9]|1[0-9])\.ffn_.*_exps\.=CPU""#);
        assert_eq!(r.sys_weights_mib, 17);
        assert_eq!(r.gpu_weights_mib, 3 + 20);
        assert_eq!(r.basis, manager_models::MemoryBasis::Exact);
        // An uncompilable pattern: all on the GPU, Estimate.
        let r = req(r"-ot blk\.(?!0).*_exps=CPU");
        assert_eq!(r.sys_weights_mib, 0);
        assert_eq!(r.basis, manager_models::MemoryBasis::Estimate);
        // --n-cpu-moe 10 is a block-index bound: blocks 3..=9.
        assert_eq!(req("--n-cpu-moe 10").sys_weights_mib, 7);
        // The slider counts MoE layers: the saved bound 10 reads
        // back as the 7 MoE layers it moves, and the table entry
        // at that position agrees with the requirement.
        let b = uncreated_backend(
            "lead-dense",
            "models/lead-dense.gguf",
            Some("--n-cpu-moe 10".to_string()),
        );
        let info = backend_offload_info(&cfg, &b).unwrap();
        assert_eq!(info.layers, 7);
        assert_eq!(info.sys_bytes_by_position[7], 7 * 1024 * 1024);
        assert_eq!(info.sys_bytes_by_position[10], 10 * 1024 * 1024);
        assert_eq!(info.sys_bytes_by_position[37], 37 * 1024 * 1024);
        // Position 10 (10 MoE layers) writes bound 13.
        assert_eq!(
            info.offload_for(10),
            manager_models::ExpertOffload::CpuMoeLayers(13)
        );
        assert_eq!(req("--n-cpu-moe 13").sys_weights_mib, 10);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn offload_slider_converts_moe_layers_to_block_bounds() {
        // Review item 11: 2 leading dense blocks + 4 MoE blocks.
        let dir = gguf_probe_dir("item11");
        std::fs::write(
            dir.join("models").join("lead2.gguf"),
            gguf_lead_dense_header(2, 4),
        )
        .unwrap();
        let mut cfg = test_cfg();
        cfg.data_dir = dir.clone();
        register_test_model(&mut cfg, "lead2-moe", "models/lead2.gguf");
        cfg.models.last_mut().unwrap().moe_layers = Some(4);
        cfg.models.last_mut().unwrap().default_offload_layers = Some(2);
        let info_for = |raw: Option<&str>| {
            let b = uncreated_backend("lead2", "models/lead2.gguf", raw.map(str::to_string));
            backend_offload_info(&cfg, &b).unwrap()
        };
        // Slider "2 MoE layers" writes --n-cpu-moe 4 ...
        let info = info_for(None);
        assert_eq!(info.moe_layers, 4);
        let target = info.offload_for(2);
        assert_eq!(
            manager_models::set_expert_offload(None, target).as_deref(),
            Some("--n-cpu-moe 4")
        );
        // ... and reads back as 2, canonical.
        let info = info_for(Some("--n-cpu-moe 4"));
        assert_eq!(info.layers, 2);
        assert!(!info.custom);
        // The row tells the UI which flag value each position writes.
        assert_eq!(
            offload_row_json(&info, None)["n_cpu_moe_by_position"],
            json!([0, 3, 4, 5, 6])
        );
        // The model's declared default (2 MoE layers) is written
        // at the same bound when the file is present ...
        let model = cfg.models.last().unwrap().clone();
        assert_eq!(
            model_default_raw_flags_at(&cfg.data_dir, &model).as_deref(),
            Some("--n-cpu-moe 4")
        );
        // ... and as-is without a parsed layout (no file).
        assert_eq!(
            model_default_raw_flags(&model).as_deref(),
            Some("--n-cpu-moe 2")
        );
        // An existing config written with the old semantics
        // (`--n-cpu-moe 2`, inside the dense prefix) displays what
        // llama.cpp really does — no MoE layer — and is marked
        // custom, since the slider would write it differently.
        let info = info_for(Some("--n-cpu-moe 2"));
        assert_eq!(info.layers, 0);
        assert!(info.custom);
        let info = info_for(Some("--n-cpu-moe 3"));
        assert_eq!(info.layers, 1);
        let _ = std::fs::remove_dir_all(&dir);
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
    fn moe_cache_info_and_requirement_pricing() {
        // The declared-MoE fixture (the offload slider's own): a
        // 48-block MoE GGUF on disk and a model record declaring
        // the count and a default offload.
        let dir = std::env::temp_dir().join(format!(
            "llm-mgr-moe-cache-{}",
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

        // Info: eligibility is the offload slider's — present
        // for the MoE backend, absent for a dense one.
        let b = uncreated_backend(
            "moe-backend",
            "models/user-moe.gguf",
            Some("--n-cpu-moe 16 --moe-cache-mib 4096".to_string()),
        );
        let info = backend_moe_cache_info(&cfg, &b).expect("MoE backend carries cache state");
        assert_eq!(info.mib, Some(4096));
        assert!(!info.custom);
        assert_eq!(info.offload_layers, 16);
        assert_eq!(info.moe_layers, 48);
        // An `=` spelling decodes to the same state, marked custom.
        let b_eq = uncreated_backend(
            "moe-backend",
            "models/user-moe.gguf",
            Some("--n-cpu-moe 16 --moe-cache-mib=4096".to_string()),
        );
        let info_eq = backend_moe_cache_info(&cfg, &b_eq).unwrap();
        assert_eq!(info_eq.mib, Some(4096));
        assert!(info_eq.custom);
        // No token: off, not custom.
        let b_off = uncreated_backend(
            "moe-backend",
            "models/user-moe.gguf",
            Some("--n-cpu-moe 16".to_string()),
        );
        let info_off = backend_moe_cache_info(&cfg, &b_off).unwrap();
        assert_eq!(info_off.mib, None);
        assert!(!info_off.custom);
        // A dense backend (the default config's planner) has none.
        let dense = cfg
            .backends
            .iter()
            .find(|x| x.id == "planner")
            .unwrap()
            .clone();
        assert!(backend_moe_cache_info(&cfg, &dense).is_none());

        // Pricing: the requirement carries the cache, residency
        // grows by exactly the cache on the single GPU, and the
        // start gate refuses the over-card cache with the numbers
        // while admitting the same requirement with room.
        let req_off = backend_memory_requirement(&cfg, &b_off).unwrap();
        assert_eq!(req_off.moe_cache_mib, 0);
        let req_on = backend_memory_requirement(&cfg, &b).unwrap();
        assert_eq!(req_on.moe_cache_mib, 4096);
        assert_eq!(req_on.moe_cache_offload_layers, 16);
        assert_eq!(req_on.moe_cache_moe_layers, 48);
        let res_off = manager_optimize::residency_per_gpu(&req_off, &[], 1)[0];
        let res_on = manager_optimize::residency_per_gpu(&req_on, &[], 1)[0];
        assert_eq!(res_on, res_off + 4096);
        let failures = vram_gate_failures(
            &[0],
            &[(0, res_on)],
            &[vram_reading(0, res_on - 1, res_on - 1)],
            &[],
        );
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].class, VramGateClass::ExceedsTotal);
        assert_eq!(failures[0].required_mib, res_on);
        let failures = vram_gate_failures(
            &[0],
            &[(0, res_on)],
            &[vram_reading(0, res_on + 1024, res_on + 1024)],
            &[],
        );
        assert!(failures.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn backend_set_moe_cache_round_trip_and_refusals() {
        let (state, dir) = advanced_test_state(test_cfg());
        let sink = test_sink();
        op_model_add(
            &state,
            &sink,
            "qwen3.8-27b-moe".to_string(),
            "https://example.com/Qwen3.8-27B-MoE.gguf".to_string(),
            None,
            None,
            Some(27.0),
            Some(48),
            Some(16),
            None,
            None,
        )
        .await
        .unwrap();
        std::fs::create_dir_all(dir.join("models")).unwrap();
        std::fs::write(
            dir.join("models").join("Qwen3.8-27B-MoE.gguf"),
            gguf_header_bytes(48, 0),
        )
        .unwrap();
        op_backend_add(
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
            None,
            None,
            None,
        )
        .await
        .unwrap();
        // The declared default offload (16) is in place; the cache
        // op rewrites only its own token.
        let r =
            op_backend_set_moe_cache(&state, &sink, "moe-backend".to_string(), Some(4096), None)
                .await
                .unwrap();
        assert_eq!(r["mib"], 4096);
        assert_eq!(r["changed"], true);
        let saved = manager_config::load(&state.config_path).unwrap();
        let b = saved
            .backends
            .iter()
            .find(|b| b.id == "moe-backend")
            .unwrap();
        assert_eq!(
            b.raw_flags.as_deref(),
            Some("--n-cpu-moe 16 --moe-cache-mib 4096")
        );
        // An explicit size and Auto at once is ambiguous input.
        let r = op_backend_set_moe_cache(
            &state,
            &sink,
            "moe-backend".to_string(),
            Some(4096),
            Some(true),
        )
        .await;
        assert!(matches!(r, Err(ApiError::InvalidInput(_))), "{r:?}");
        // Offload 0 refuses a nonzero cache with the reason;
        // turning the cache off stays allowed, so a stranded
        // token can always be cleared.
        op_backend_set_offload(&state, &sink, "moe-backend".to_string(), 0, None)
            .await
            .unwrap();
        let r =
            op_backend_set_moe_cache(&state, &sink, "moe-backend".to_string(), Some(8192), None)
                .await;
        match r {
            Err(ApiError::InvalidInput(msg)) => {
                assert!(msg.contains("offloads 0 MoE layers"), "{msg}")
            }
            other => panic!("expected the offload-0 refusal, got {other:?}"),
        }
        let r = op_backend_set_moe_cache(&state, &sink, "moe-backend".to_string(), None, None)
            .await
            .unwrap();
        assert_eq!(r["mib"], Value::Null);
        let saved = manager_config::load(&state.config_path).unwrap();
        let b = saved
            .backends
            .iter()
            .find(|b| b.id == "moe-backend")
            .unwrap();
        assert!(
            !b.raw_flags.as_deref().unwrap_or("").contains("moe-cache"),
            "{:?}",
            b.raw_flags
        );
        // Auto cannot price anything in this environment (no GPU
        // telemetry): it refuses rather than guessing a size.
        op_backend_set_offload(&state, &sink, "moe-backend".to_string(), 16, None)
            .await
            .unwrap();
        let r =
            op_backend_set_moe_cache(&state, &sink, "moe-backend".to_string(), None, Some(true))
                .await;
        assert!(matches!(r, Err(ApiError::InvalidInput(_))), "{r:?}");
        // A row (tensor) split refuses a nonzero cache: the
        // pinned llama.cpp rejects the cache under tensor
        // parallelism.
        {
            let mut cfg = state.config.write().await;
            let b = cfg
                .backends
                .iter_mut()
                .find(|b| b.id == "moe-backend")
                .unwrap();
            b.flags.split_mode = manager_config::SplitMode::Row;
        }
        let r =
            op_backend_set_moe_cache(&state, &sink, "moe-backend".to_string(), Some(4096), None)
                .await;
        match r {
            Err(ApiError::InvalidInput(msg)) => {
                assert!(msg.contains("row (tensor) split"), "{msg}")
            }
            other => panic!("expected the row-split refusal, got {other:?}"),
        }
        // A dense backend has no expert cache to set.
        let r =
            op_backend_set_moe_cache(&state, &sink, "planner".to_string(), Some(4096), None).await;
        match r {
            Err(ApiError::InvalidInput(msg)) => {
                assert!(msg.contains("does not support an expert cache"), "{msg}")
            }
            other => panic!("expected the dense refusal, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
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
            None,
            None,
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
            None,
            None,
        )
        .await
        .unwrap();
        let off = preview["offload"]
            .as_object()
            .expect("preview carries offload");
        assert_eq!(off["moe_layers"], 48);
        assert_eq!(off["green_limit_layers"], 16);
        assert_eq!(off["layers"], 16);
        let cfg_now = state.config.read().await.clone();
        let info = backend_offload_info(&cfg_now, &b).expect("offload info for the new backend");
        assert_eq!(info.moe_layers, 48);
        assert_eq!(info.green_limit_layers, 16);
        // The preview's requirement summary prices exactly what the
        // created backend's card prices (same configuration: the
        // catalog default offload at the 32K creation context).
        let req =
            backend_memory_requirement(&cfg_now, &b).expect("requirement for the new backend");
        let mem = preview["memory"]
            .as_object()
            .expect("preview carries memory");
        assert_eq!(mem["gpu_ram_mib"], json!(req.gpu_weights_mib));
        assert_eq!(mem["sys_ram_mib"], json!(req.sys_weights_mib));
        assert!(
            req.sys_weights_mib > 0,
            "partial offload holds experts in system RAM"
        );
        assert_eq!(mem["kv_ram_mib"], json!(req.kv_mib));
        assert!(mem["per_gpu"].is_array());
        // A larger context draft re-prices the KV term, never the
        // weights.
        let wide = op_backend_add_preview(
            &state,
            &sink,
            "qwen3.8-27b-moe".to_string(),
            None,
            Some(65536),
            None,
        )
        .await
        .unwrap();
        assert_eq!(wide["memory"]["gpu_ram_mib"], mem["gpu_ram_mib"]);
        assert_eq!(wide["memory"]["sys_ram_mib"], mem["sys_ram_mib"]);
        if let (Some(a), Some(b2)) = (
            mem["kv_ram_mib"].as_u64(),
            wide["memory"]["kv_ram_mib"].as_u64(),
        ) {
            assert!(b2 > a, "KV at 64K exceeds KV at 32K");
        }
        // per_gpu: one entry per GPU the split uses, carrying its
        // share and the start gate's residency for it; an unsplit
        // placement loads on the first GPU.
        let gpus = test_gpus();
        let mut split_b = b.clone();
        split_b.flags.tensor_split = vec![0.75, 0.0, 0.25];
        let m = add_preview_memory_json(&cfg_now, &split_b, None, None, &gpus);
        let per = m["per_gpu"].as_array().unwrap();
        assert_eq!(per.len(), 2);
        assert_eq!(per[0]["gpu"], 0);
        assert_eq!(per[1]["gpu"], 2);
        assert_eq!(per[1]["total_mib"], 16384);
        let res = manager_optimize::residency_per_gpu(&req, &split_b.flags.tensor_split, 3);
        assert_eq!(per[0]["required_mib"], json!(res[0]));
        assert_eq!(per[1]["required_mib"], json!(res[2]));
        let mut unsplit = b.clone();
        unsplit.flags.tensor_split = Vec::new();
        let m = add_preview_memory_json(&cfg_now, &unsplit, None, None, &gpus);
        let per = m["per_gpu"].as_array().unwrap();
        assert_eq!(per.len(), 1);
        assert_eq!(per[0]["gpu"], 0);
        assert_eq!(per[0]["share"], 1.0);
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
                None,
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
    async fn user_declared_mtp_model_round_trip() {
        // A user-added model whose GGUF carries an MTP head the
        // curated catalog does not know about, declared at add
        // time — the declaration path that gives it the MTP
        // draft control (backend_mtp_info / backend_set_mtp
        // consume the EFFECTIVE capability).
        let (state, dir) = advanced_test_state(test_cfg());
        let sink = test_sink();
        let r = op_model_add(
            &state,
            &sink,
            "qwen3.6-27b-mtp".to_string(),
            "https://example.com/Qwen3.6-27B-mtp.gguf".to_string(),
            None,
            None,
            Some(27.0),
            None,
            None,
            Some(true),
            None,
        )
        .await
        .unwrap();
        assert_eq!(r["model_id"], "qwen3.6-27b-mtp");
        let saved = manager_config::load(&state.config_path).unwrap();
        let m = saved
            .models
            .iter()
            .find(|m| m.id == "qwen3.6-27b-mtp")
            .unwrap();
        assert_eq!(m.mtp, Some(true));
        // The declaration is stored in the config file; the
        // catalog pin field of the payload is null (the catalog
        // knows neither the id nor the URL).
        let text = std::fs::read_to_string(&state.config_path).unwrap();
        assert!(text.contains("\"mtp\": true"), "{text}");
        let listed = op_list_models(&state, &sink).await.unwrap();
        let row = listed["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == "qwen3.6-27b-mtp")
            .unwrap()
            .clone();
        assert_eq!(row["mtp"], true);
        assert_eq!(row["catalog_mtp"], serde_json::Value::Null);
        // A backend serving it gets the MTP control, and
        // backend_set_mtp canonicalizes the spec tokens.
        std::fs::create_dir_all(dir.join("models")).unwrap();
        std::fs::write(
            dir.join("models").join("Qwen3.6-27B-mtp.gguf"),
            gguf_header_bytes(0, 4),
        )
        .unwrap();
        op_backend_add(
            &state,
            &sink,
            "mtp-backend".to_string(),
            "qwen3.6-27b-mtp".to_string(),
            8197,
            "mtp".to_string(),
            true,
            true,
            None,
            None,
            None,
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
            .find(|b| b.id == "mtp-backend")
            .unwrap()
            .clone();
        assert!(backend_mtp_info(&saved, &b).is_some());
        op_backend_set_mtp(&state, &sink, "mtp-backend".to_string(), Some(2))
            .await
            .unwrap();
        let saved = manager_config::load(&state.config_path).unwrap();
        let flags = saved
            .backends
            .iter()
            .find(|b| b.id == "mtp-backend")
            .unwrap()
            .raw_flags
            .clone()
            .unwrap();
        assert!(flags.contains("--spec-type draft-mtp"), "{flags}");
        assert!(flags.contains("--spec-draft-n-max 2"), "{flags}");
        // Declaring the model not capable removes the control
        // and the op — and does NOT rewrite the flags already on
        // the backend (the offload declaration's precedent).
        let r = op_model_set_mtp(&state, &sink, "qwen3.6-27b-mtp".to_string(), Some(false))
            .await
            .unwrap();
        assert_eq!(r["mtp"], false);
        let saved = manager_config::load(&state.config_path).unwrap();
        let b = saved
            .backends
            .iter()
            .find(|b| b.id == "mtp-backend")
            .unwrap()
            .clone();
        assert!(backend_mtp_info(&saved, &b).is_none());
        assert!(matches!(
            op_backend_set_mtp(&state, &sink, "mtp-backend".to_string(), Some(1)).await,
            Err(ApiError::InvalidInput(_))
        ));
        // Clearing the declaration outright behaves the same,
        // drops the field from the stored config entirely, and
        // still leaves the backend's flags untouched.
        op_model_set_mtp(&state, &sink, "qwen3.6-27b-mtp".to_string(), None)
            .await
            .unwrap();
        let saved = manager_config::load(&state.config_path).unwrap();
        assert_eq!(
            saved
                .models
                .iter()
                .find(|m| m.id == "qwen3.6-27b-mtp")
                .unwrap()
                .mtp,
            None
        );
        let text = std::fs::read_to_string(&state.config_path).unwrap();
        assert!(!text.contains("\"mtp\":"), "{text}");
        let flags_after = saved
            .backends
            .iter()
            .find(|b| b.id == "mtp-backend")
            .unwrap()
            .raw_flags
            .clone()
            .unwrap();
        assert_eq!(flags_after, flags);
        // An unknown model refuses with NotFound.
        assert!(matches!(
            op_model_set_mtp(&state, &sink, "nope".to_string(), Some(true)).await,
            Err(ApiError::NotFound(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn mtp_declaration_does_not_displace_a_catalog_pin() {
        let (state, dir) = advanced_test_state(test_cfg());
        let sink = test_sink();
        // Seed the catalog models into the config (the default
        // test config carries none until the catalog sync runs).
        op_list_models(&state, &sink).await.unwrap();
        // The catalog pins the coder NOT MTP-capable: a stored
        // declaration of true is shadowed by the pin.
        op_model_set_mtp(&state, &sink, "qwen3-coder-30b".to_string(), Some(true))
            .await
            .unwrap();
        // And the pinned-capable planner build stays capable
        // with a declaration of false stored on it.
        op_model_set_mtp(&state, &sink, "qwen3.8-27b-mtp".to_string(), Some(false))
            .await
            .unwrap();
        let saved = manager_config::load(&state.config_path).unwrap();
        let mut probe = saved.backends[0].clone();
        let coder_model = saved
            .models
            .iter()
            .find(|m| m.id == "qwen3-coder-30b")
            .unwrap();
        probe.model_file = coder_model.gguf_path.clone();
        assert!(backend_mtp_info(&saved, &probe).is_none());
        let mtp_model = saved
            .models
            .iter()
            .find(|m| m.id == "qwen3.8-27b-mtp")
            .unwrap();
        probe.model_file = mtp_model.gguf_path.clone();
        assert!(backend_mtp_info(&saved, &probe).is_some());
        // list_models reports pin and declaration side by side.
        let listed = op_list_models(&state, &sink).await.unwrap();
        let rows = listed["models"].as_array().unwrap();
        let coder_row = rows.iter().find(|v| v["id"] == "qwen3-coder-30b").unwrap();
        assert_eq!(coder_row["catalog_mtp"], false);
        assert_eq!(coder_row["mtp"], true);
        let mtp_row = rows.iter().find(|v| v["id"] == "qwen3.8-27b-mtp").unwrap();
        assert_eq!(mtp_row["catalog_mtp"], true);
        assert_eq!(mtp_row["mtp"], false);
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
        let preview =
            op_backend_add_preview(&state, &sink, "dense-7b".to_string(), None, None, None)
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
        fn next_status(script: &std::sync::Mutex<Vec<u16>>) -> u16 {
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
                            head_end = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
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
                    let path = req.split_whitespace().nth(1).unwrap_or("").to_string();
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
        (format!("http://127.0.0.1:{port}"), health_count, chat_count)
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
        let (base, health_count, chat_count) = offload_probe_stub(vec![503, 503, 200], vec![200]);
        let mut alive = || {
            Box::pin(async { true })
                as std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
        };
        let outcome =
            offload_probe_http_sequence(&base, offload_probe_test_timing(), &mut alive).await;
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
        let (base, _health_count, chat_count) = offload_probe_stub(vec![200], vec![503, 503, 200]);
        let mut alive = || {
            Box::pin(async { true })
                as std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
        };
        let outcome =
            offload_probe_http_sequence(&base, offload_probe_test_timing(), &mut alive).await;
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
        let outcome =
            offload_probe_http_sequence(&base, offload_probe_test_timing(), &mut alive).await;
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
            assert_eq!(spec.args.iter().filter(|a| *a == "--jinja").count(), 1);
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

    // --- KV cache / flash attention integration (feature/backend-kv-fa) ---

    fn planner_cache_layout() -> WeightLayout {
        WeightLayout {
            total_bytes: 12_120_016_960,
            expert_bytes: None,
            expert_bytes_by_block: None,
            expert_tensors: None,
            raw_flags: None,
            moe_layers: None,
            kv_geometry: Some(manager_models::KvGeometry {
                kv_layers: 16,
                head_count_kv: 4,
                key_length: 256,
                value_length: 256,
            }),
            kv_per_token: Some(65_536),
            rs_bytes_per_seq: Some(156_893_184),
            basis: manager_models::MemoryBasis::Exact,
            catalog: None,
            green_limit_layers: None,
        }
    }

    #[test]
    fn selected_cache_types_price_the_requirement() {
        // Pricing delta at the integration seam: the same layout
        // priced through kv_bytes_per_token_for (the function
        // backend_memory_requirement uses) at each selection.
        let layout = planner_cache_layout();
        assert_eq!(kv_bytes_per_token_for(&layout, None), Some(65_536));
        assert_eq!(
            kv_bytes_per_token_for(
                &layout,
                Some("--cache-type-k f16 --cache-type-v f16 --flash-attn on")
            ),
            Some(65_536),
            "an explicit f16 selection prices exactly as the pre-feature default"
        );
        assert_eq!(
            kv_bytes_per_token_for(
                &layout,
                Some("--cache-type-k q8_0 --cache-type-v q8_0 --flash-attn on")
            ),
            Some(34_816)
        );
        assert_eq!(
            kv_bytes_per_token_for(
                &layout,
                Some("--cache-type-k q8_0 --cache-type-v q4_0 --flash-attn on")
            ),
            Some(26_624)
        );
        assert_eq!(
            kv_bytes_per_token_for(
                &layout,
                Some("--cache-type-k q4_0 --cache-type-v q4_0 --flash-attn on")
            ),
            Some(18_432)
        );
        // The requirement totals differ by exactly the KV deltas
        // at 32K context, 1 slot: f16 2048 MiB, q8/q8 1088, q4/q4 576.
        let kv_mib = |raw: Option<&str>| {
            manager_models::memory_requirement(
                layout.total_bytes,
                layout.expert_bytes,
                None,
                layout.basis,
                manager_models::ExpertOffload::None,
                kv_bytes_per_token_for(&layout, raw),
                layout.moe_layers,
                32_768,
                layout.rs_bytes_per_seq,
                1,
            )
            .kv_mib
        };
        assert_eq!(kv_mib(None), Some(2_048));
        assert_eq!(
            kv_mib(Some("--cache-type-k q8_0 --cache-type-v q8_0")),
            Some(1_088)
        );
        assert_eq!(
            kv_mib(Some("--cache-type-k q4_0 --cache-type-v q4_0")),
            Some(576)
        );
        // No geometry: the f16 figure is used unchanged, never zero.
        let mut no_geo = planner_cache_layout();
        no_geo.kv_geometry = None;
        assert_eq!(
            kv_bytes_per_token_for(&no_geo, Some("--cache-type-k q4_0")),
            Some(65_536)
        );
    }

    #[test]
    fn cache_selection_resolver_validates_the_wire_values() {
        let s = |v: &str| Some(v.to_string());
        assert_eq!(resolve_cache_selection(None, None, None).unwrap(), None);
        let sel = resolve_cache_selection(s("q8_0"), s("q4_0"), s("on"))
            .unwrap()
            .unwrap();
        assert_eq!(sel.k, manager_models::CacheType::Q8_0);
        assert_eq!(sel.v, manager_models::CacheType::Q4_0);
        assert_eq!(sel.flash_attn, manager_models::FlashAttn::On);
        // Partial sets are refused.
        assert!(resolve_cache_selection(s("q8_0"), None, None).is_err());
        assert!(resolve_cache_selection(None, s("q8_0"), s("on")).is_err());
        // Unoffered types and bad FA spellings are refused.
        assert!(resolve_cache_selection(s("q5_0"), s("q8_0"), s("on")).is_err());
        assert!(resolve_cache_selection(s("q8_0"), s("q8_0"), s("banana")).is_err());
        // The pairing llama.cpp refuses is refused here too.
        assert!(resolve_cache_selection(s("q8_0"), s("q8_0"), s("off")).is_err());
        assert!(resolve_cache_selection(s("f16"), s("f16"), s("off")).is_ok());
    }

    #[test]
    fn cache_preview_reports_best_and_honest_no_fit() {
        let layout = planner_cache_layout();
        let targets = (Vec::new(), vec![16_384u64], vec![0u32], vec![None]);
        let sel_f16 = manager_models::CacheSelection::default();
        // 64K: preview prices f16, best is the q8_0 rung.
        let p = cache_preview_json(
            &layout,
            manager_models::ExpertOffload::None,
            sel_f16,
            65_536,
            1,
            0,
            Some(&targets),
        );
        assert_eq!(p["kv_mib"], 4_096);
        assert_eq!(p["fits_current"], false);
        assert_eq!(p["best"]["k"], "q8_0");
        assert_eq!(p["best"]["v"], "q8_0");
        assert_eq!(p["headroom_reserve_percent"], 5);
        // The single target carries the whole total as its
        // required share, with its own verdict (over the 5%
        // headroom limit here, so false like fits_current).
        assert_eq!(p["targets"][0]["gpu"], 0);
        assert_eq!(p["targets"][0]["total_mib"], 16_384);
        assert_eq!(p["targets"][0]["required_mib"], 11_559 + 4_096 + 150);
        assert_eq!(p["targets"][0]["fits"], false);
        // The three-state classification judges total VRAM, not
        // the headroom reserve: under total is "fits" here even
        // though the headroom verdict above is false, and the
        // driver model rides along (unreported in this fixture).
        assert_eq!(p["targets"][0]["fit_state"], "fits");
        assert_eq!(p["targets"][0]["paged_mib"], 0);
        assert_eq!(p["targets"][0]["driver_model"], Value::Null);
        // Split across two targets: shares follow the split and
        // each target's verdict is its own — the small card
        // refuses its half while the large one takes it.
        let split_targets = (
            vec![0.5, 0.5],
            vec![16_384u64, 4_096],
            vec![0u32, 3],
            vec![None, None],
        );
        let p = cache_preview_json(
            &layout,
            manager_models::ExpertOffload::None,
            sel_f16,
            32_768,
            1,
            0,
            Some(&split_targets),
        );
        let half = (11_559 + 2_048 + 150) / 2 + (11_559 + 2_048 + 150) % 2;
        assert_eq!(p["targets"][0]["gpu"], 0);
        assert_eq!(p["targets"][0]["required_mib"], half);
        assert_eq!(p["targets"][0]["fits"], true);
        assert_eq!(p["targets"][1]["gpu"], 3);
        assert_eq!(
            p["targets"][1]["required_mib"],
            (11_559 + 2_048 + 150) - half
        );
        assert_eq!(p["targets"][1]["fits"], false);
        // Over total with no WDDM driver model reported: the
        // three-state verdict is a hard does_not_fit, no paging.
        assert_eq!(p["targets"][1]["fit_state"], "does_not_fit");
        assert_eq!(p["targets"][1]["paged_mib"], 0);
        assert_eq!(p["fits_current"], false);
        // 256K: no rung fits; best is null and the ladder's lowest
        // rung carries the honest numbers (nothing is applied).
        let p = cache_preview_json(
            &layout,
            manager_models::ExpertOffload::None,
            sel_f16,
            262_144,
            1,
            0,
            Some(&targets),
        );
        assert_eq!(p["best"], Value::Null);
        assert_eq!(p["ladder"][3]["total_mib"], 11_559 + 4_608 + 150);
        assert_eq!(p["ladder"][3]["fits"], false);
    }

    #[test]
    fn backend_cache_info_distinguishes_pre_feature_and_custom() {
        let cfg = test_cfg();
        let mut b = backend(&cfg, "planner");
        b.raw_flags = None;
        let info = backend_cache_info(&b);
        assert!(!info.present);
        assert!(!info.custom);
        assert_eq!(info.selection, manager_models::CacheSelection::default());
        b.raw_flags = Some("--parallel 1".to_string());
        assert!(!backend_cache_info(&b).present);
        b.raw_flags = Some("--cache-type-k q8_0 --cache-type-v q8_0 --flash-attn on".to_string());
        let info = backend_cache_info(&b);
        assert!(info.present);
        assert!(!info.custom);
        assert_eq!(info.selection.k, manager_models::CacheType::Q8_0);
        // A short-spelling saved form is present but custom.
        b.raw_flags = Some("-ctk q8_0".to_string());
        let info = backend_cache_info(&b);
        assert!(info.present);
        assert!(info.custom);
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
    let result = test_tool_call(&base_url).await.map_err(ApiError::from)?;
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
    mutate_save_install(&mut cfg, &state.config_path, |cfg| {
        if let Some(b) = cfg.backends.iter_mut().find(|b| b.id == id) {
            b.tool_call_probe = Some(manager_config::ToolCallProbe {
                outcome,
                at_ms: now_ms(),
            });
        }
    })
    .map_err(ApiError::from)?;
    drop(cfg);
    Ok(serde_json::to_value(&result)?)
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
    let status =
        manager_models::offload_status(restrictions, layers, moe_layers, &placed_ccs, cpu_only);
    let restricting_cc = placed_ccs
        .iter()
        .copied()
        .find(|cc| restrictions.iter().any(|r| r.compute_capability == *cc));
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
    let cc = placed_ccs
        .iter()
        .copied()
        .find(|cc| restrictions.iter().any(|r| r.compute_capability == *cc))?;
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
    let started_at = backend_start_times(state, cfg).await;
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
async fn offload_probe_context(state: &AppState, id: &str) -> Option<(u32, u32, u16)> {
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
    /// is reported as terminal, measured from the first transient
    /// failure.
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
type OffloadProbeAlive<'a> = dyn FnMut() -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>
    + Send
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
    // The window opens at the first transient failure, not before
    // the first request: a slow first attempt must not consume the
    // whole window, so a transient failure is always retried at
    // least once (and the outcome no longer depends on how long
    // that first request happened to take).
    let mut retry_deadline: Option<std::time::Instant> = None;
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
        let now = std::time::Instant::now();
        let deadline = *retry_deadline.get_or_insert(now + timing.retry_window);
        if !transient || now >= deadline {
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
