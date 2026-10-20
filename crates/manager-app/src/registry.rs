//! The operation registry: the single declaration of every capability
//! the app exposes, used to generate every surface that offers them.
//!
//! Drift prevention (Pete's requirement, admin API plan phase 0/1):
//! each operation is declared exactly once in [`for_each_operation!`]
//! with its name, capability class (auth plan §1), handler, parameter
//! list, and REST route. From that one declaration this module
//! generates the [`Op`] enum, [`OP_SPECS`], the exhaustive
//! [`dispatch`] match, and the class table; src-tauri generates its
//! `#[tauri::command]` adapters and `generate_handler!` list from the
//! same macro; manager-api generates its axum router from
//! [`OP_SPECS`]. Adding an operation means adding one registry line:
//! the build then fails at every site that has not been wired (the
//! generated exhaustive matches have no wildcard arms) until the new
//! operation is handled everywhere.
//!
//! A route of `None` is an explicit, visible exclusion from the REST
//! surface (desktop-only actions, v1 carve-outs in PLAN.md §4), never
//! an oversight: the contract tests below enumerate them.

use std::sync::Arc;

use serde_json::Value;

use crate::{ApiError, AppState, EventSink};

/// Capability classes (auth plan §1; PLAN.md §2.6 — the auth taxonomy
/// governs authorization). `S` is split into ordinary config reads and
/// the three disclosure items, matching the §3.5 matrix rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapClass {
    /// R — reads (non-sensitive).
    R,
    /// S — config reads (config family, MXC policy read).
    SConfig,
    /// S — disclosure (master key, diagnostics bundle, launch command).
    SDisclosure,
    /// L — lifecycle control.
    L,
    /// C — configuration edits.
    C,
    /// D — destructive / install-level operations.
    D,
    /// X — security-policy and credential writes.
    X,
    /// User administration (API-layer surface, not an app operation).
    UserAdmin,
    /// API settings (API-layer surface, not an app operation).
    ApiSettings,
}

/// The four roles (auth plan §2). Exactly one per user in v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Viewer,
    Operator,
    Configurator,
    Admin,
}

/// The role → class permission table (PLAN.md §3.5). Exhaustive over
/// [`CapClass`] with no wildcard arm: adding a class breaks the build
/// here until its grants are decided.
pub fn role_allows(role: Role, class: CapClass) -> bool {
    match class {
        CapClass::R => true,
        CapClass::SConfig => true,
        CapClass::SDisclosure => matches!(role, Role::Admin),
        CapClass::L => matches!(role, Role::Operator | Role::Configurator | Role::Admin),
        CapClass::C => matches!(role, Role::Configurator | Role::Admin),
        CapClass::D => matches!(role, Role::Admin),
        CapClass::X => matches!(role, Role::Admin),
        CapClass::UserAdmin => matches!(role, Role::Admin),
        CapClass::ApiSettings => matches!(role, Role::Admin),
    }
}

/// HTTP methods used by the REST route table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

/// How a route's response is produced (rest-api.md §2/§6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteMode {
    /// Run the operation and return its result (200).
    Sync,
    /// Long-running: the operation runs as a task; the route answers
    /// 202 with the operation resource (phase 4; synchronous until
    /// then).
    AsyncOp,
    /// Creates a resource: 201 with the operation result.
    Created,
}

/// One operation's declared REST route. Path placeholders are named
/// after the operation's arguments — the API layer merges them into
/// the argument object by name. `body_wrap` names an argument the
/// whole request body is wrapped under (for operations whose single
/// argument is the body document itself: `patch`, `request`,
/// `report`, `policy`); `None` means body fields merge into the
/// argument object.
#[derive(Debug, Clone, Copy)]
pub struct RouteSpec {
    pub method: HttpMethod,
    pub path: &'static str,
    pub body_wrap: Option<&'static str>,
    pub mode: RouteMode,
}

/// A plain synchronous route.
pub const fn route(method: HttpMethod, path: &'static str) -> RouteSpec {
    RouteSpec {
        method,
        path,
        body_wrap: None,
        mode: RouteMode::Sync,
    }
}

/// A synchronous route whose body is one named argument's value.
pub const fn route_wrap(method: HttpMethod, path: &'static str, wrap: &'static str) -> RouteSpec {
    RouteSpec {
        method,
        path,
        body_wrap: Some(wrap),
        mode: RouteMode::Sync,
    }
}

/// A long-running route (202 + operation resource).
pub const fn route_async(method: HttpMethod, path: &'static str) -> RouteSpec {
    RouteSpec {
        method,
        path,
        body_wrap: None,
        mode: RouteMode::AsyncOp,
    }
}

/// A resource-creating route (201).
pub const fn route_created(method: HttpMethod, path: &'static str) -> RouteSpec {
    RouteSpec {
        method,
        path,
        body_wrap: None,
        mode: RouteMode::Created,
    }
}

/// The static description of one registered operation.
#[derive(Debug, Clone, Copy)]
pub struct OpSpec {
    pub op: Op,
    /// The Tauri command name (also the operation's canonical name).
    pub name: &'static str,
    pub class: CapClass,
    /// The operation's argument names. The REST layer treats request
    /// bodies as closed objects: fields outside this list are
    /// rejected (rest-api.md §2).
    pub params: &'static [&'static str],
    /// REST route, or `None` for an explicit exclusion (see module docs).
    pub route: Option<RouteSpec>,
}

/// Deserialize one named argument out of the argument object.
/// Missing keys deserialize as `null` (so `Option<T>` arguments read
/// as `None`, and a missing required argument is an `InvalidInput`).
fn arg<T: serde::de::DeserializeOwned>(args: &Value, name: &str) -> Result<T, ApiError> {
    let v = args.get(name).cloned().unwrap_or(Value::Null);
    serde_json::from_value(v).map_err(|e| ApiError::InvalidInput(format!("argument `{name}`: {e}")))
}

/// The canonical operation list. Entry shape:
/// `(Variant, name, Class, op_fn, [param: Type, ...], route)`.
/// Classes follow auth plan §1 with the PLAN.md §2.6 reconciliations
/// (notably: `mxc_run_self_test` is read-class behavior).
#[macro_export]
macro_rules! for_each_operation {
    ($m:ident) => {
        $m! {
            // Status / config
            (GetStatus, get_status, R, op_get_status, [], Some(route(HttpMethod::Get, "/api/v1/status"))),
            (GetConfig, get_config, SConfig, op_get_config, [], Some(route(HttpMethod::Get, "/api/v1/config"))),
            // Excluded from REST v1 (PLAN.md decision 11: the broad raw
            // config patch; domain endpoints cover the same ground).
            (UpdateConfig, update_config, C, op_update_config, [patch: Value], None),
            // Backends
            (ListBackends, list_backends, R, op_list_backends, [], Some(route(HttpMethod::Get, "/api/v1/backends"))),
            (StartBackend, start_backend, L, op_start_backend, [id: String], Some(route_async(HttpMethod::Post, "/api/v1/backends/{id}/start"))),
            (StopBackend, stop_backend, L, op_stop_backend, [id: String, graceful: bool], Some(route_async(HttpMethod::Post, "/api/v1/backends/{id}/stop"))),
            (RestartBackend, restart_backend, L, op_restart_backend, [id: String, graceful: bool], Some(route_async(HttpMethod::Post, "/api/v1/backends/{id}/restart"))),
            (TestBackend, test_backend, L, op_test_backend, [id: String, prompt: String], Some(route(HttpMethod::Post, "/api/v1/backends/{id}/test"))),
            (BackendGetConfig, backend_get_config, SConfig, op_backend_get_config, [id: String], Some(route(HttpMethod::Get, "/api/v1/backends/{id}/config"))),
            (BackendPreviewDiff, backend_preview_diff, R, op_backend_preview_diff, [id: String, patch: Value], Some(route(HttpMethod::Post, "/api/v1/backends/{id}/config/preview-diff"))),
            (BackendApplyConfig, backend_apply_config, C, op_backend_apply_config, [id: String, patch: Value, apply_mode: String], Some(route(HttpMethod::Patch, "/api/v1/backends/{id}/config"))),
            (BackendAdd, backend_add, C, op_backend_add, [id: String, model_id: String, port: u16, group: String, create_group: bool, enabled: bool, n_ctx: Option<u32>, n_batch: Option<u32>, offload_layers: Option<u32>, offload_override: Option<bool>, gpu_indices: Option<Vec<u32>>, parallel: Option<u32>], Some(route_created(HttpMethod::Post, "/api/v1/backends"))),
            (BackendAddPreview, backend_add_preview, R, op_backend_add_preview, [model_id: String, gpu_indices: Option<Vec<u32>>], None),
            (BackendUpdateFlags, backend_update_flags, C, op_backend_update_flags, [id: String, n_ctx: u32, n_batch: u32], Some(route(HttpMethod::Put, "/api/v1/backends/{id}/flags"))),
            (BackendSetOffload, backend_set_offload, C, op_backend_set_offload, [id: String, layers: u32, allow_override: Option<bool>], Some(route(HttpMethod::Put, "/api/v1/backends/{id}/offload"))),
            (BackendSetMtp, backend_set_mtp, C, op_backend_set_mtp, [id: String, n_max: Option<u32>], Some(route(HttpMethod::Put, "/api/v1/backends/{id}/mtp"))),
            (BackendSetParallel, backend_set_parallel, C, op_backend_set_parallel, [id: String, slots: Option<u32>], Some(route(HttpMethod::Put, "/api/v1/backends/{id}/parallel"))),
            // Advanced flags mode (tracker #60): the raw-flags save
            // (advanced mode only; validation warns, never blocks)
            // and the mode flip itself (writes no flags).
            (BackendSetRawFlags, backend_set_raw_flags, C, op_backend_set_raw_flags, [id: String, raw_flags: Option<String>], Some(route(HttpMethod::Put, "/api/v1/backends/{id}/raw-flags"))),
            (BackendSetFlagsMode, backend_set_flags_mode, C, op_backend_set_flags_mode, [id: String, advanced: bool], Some(route(HttpMethod::Put, "/api/v1/backends/{id}/flags-mode"))),
            (BackendDelete, backend_delete, D, op_backend_delete, [id: String], Some(route(HttpMethod::Delete, "/api/v1/backends/{id}"))),
            (BackendGetLaunchCommand, backend_get_launch_command, SDisclosure, op_backend_get_launch_command, [id: String], Some(route(HttpMethod::Get, "/api/v1/backends/{id}/launch-command"))),
            // Wizard / setup
            (RunWizardStep, run_wizard_step, D, op_run_wizard_step, [step_id: String], Some(route_async(HttpMethod::Post, "/api/v1/setup/steps/{step_id}/run"))),
            (RunAudit, run_audit, L, op_run_audit, [], Some(route_async(HttpMethod::Post, "/api/v1/setup/audit"))),
            (SetupGetPrerequisites, setup_get_prerequisites, R, op_setup_get_prerequisites, [], Some(route(HttpMethod::Get, "/api/v1/setup/prerequisites"))),
            (SetupRetryStep, setup_retry_step, D, op_setup_retry_step, [step_id: String], Some(route_async(HttpMethod::Post, "/api/v1/setup/steps/{step_id}/retry"))),
            (SetupSkipStep, setup_skip_step, C, op_setup_skip_step, [step_id: String, note: String], Some(route(HttpMethod::Post, "/api/v1/setup/steps/{step_id}/skip"))),
            (SetupGetCommands, setup_get_commands, R, op_setup_get_commands, [step_id: String], Some(route(HttpMethod::Get, "/api/v1/setup/steps/{step_id}/commands"))),
            (SetupMarkManual, setup_mark_manual, C, op_setup_mark_manual, [step_id: String, path: String, version: String], Some(route(HttpMethod::Post, "/api/v1/setup/steps/{step_id}/mark-manual"))),
            (SetupGetSteps, setup_get_steps, R, op_setup_get_steps, [], Some(route(HttpMethod::Get, "/api/v1/setup/steps"))),
            (SetupStatus, setup_status, R, op_setup_status, [], Some(route(HttpMethod::Get, "/api/v1/setup/status"))),
            (SetupMarkComplete, setup_mark_complete, C, op_setup_mark_complete, [], Some(route(HttpMethod::Post, "/api/v1/setup/complete"))),
            (SetupStepHelpUrl, setup_step_help_url, R, op_setup_step_help_url, [step_id: String], Some(route(HttpMethod::Get, "/api/v1/setup/steps/{step_id}/help-url"))),
            // Desktop-only actions: no REST equivalent (rest-api.md §7).
            (OpenUrl, open_url, L, op_open_url, [url: String], None),
            (OpenFolder, open_folder, L, op_open_folder, [path: String], None),
            // The master-key value is never mirrored over REST in v1
            // (PLAN.md decision 4); the Tauri command remains.
            (GatewayMasterKey, gateway_master_key, SDisclosure, op_gateway_master_key, [], None),
            // Exposed over REST only inside the PATCH /gateway/config
            // composition (rest-api.md §6.5), not as its own route.
            (GatewaySetBindHost, gateway_set_bind_host, C, op_gateway_set_bind_host, [host: String], None),
            // Models
            (ListModels, list_models, R, op_list_models, [], Some(route(HttpMethod::Get, "/api/v1/models"))),
            (DownloadModel, download_model, L, op_download_model, [id: String], Some(route_async(HttpMethod::Post, "/api/v1/models/{id}/download"))),
            (CancelDownload, cancel_download, L, op_cancel_download, [id: String], Some(route(HttpMethod::Post, "/api/v1/models/{id}/download/cancel"))),
            (ModelPauseDownload, model_pause_download, L, op_model_pause_download, [id: String], Some(route(HttpMethod::Post, "/api/v1/models/{id}/download/pause"))),
            (ModelResumeDownload, model_resume_download, L, op_model_resume_download, [id: String], Some(route_async(HttpMethod::Post, "/api/v1/models/{id}/download/resume"))),
            (ModelVerifyChecksum, model_verify_checksum, L, op_model_verify_checksum, [id: String], Some(route_async(HttpMethod::Post, "/api/v1/models/{id}/verify"))),
            // PATCH /models/{id} is a composite over this and
            // model_set_enabled (rest-api.md §6.3); no 1:1 route.
            (ModelSetShortName, model_set_short_name, C, op_model_set_short_name, [id: String, name: String], None),
            (ModelDelete, model_delete, D, op_model_delete, [id: String], Some(route(HttpMethod::Delete, "/api/v1/models/{id}"))),
            (ModelAssign, model_assign, C, op_model_assign, [id: String, backend_id: String, split: Vec<f32>], Some(route(HttpMethod::Post, "/api/v1/models/{id}/assign"))),
            (ModelAdd, model_add, C, op_model_add, [id: String, source_url: String, file_name: Option<String>, quant: Option<String>, params_b: Option<f32>], Some(route_created(HttpMethod::Post, "/api/v1/models"))),
            (ModelSetEnabled, model_set_enabled, C, op_model_set_enabled, [id: String, enabled: bool], None),
            // Logs
            (GetLogs, get_logs, R, op_get_logs, [scope: String, level: Option<String>, search: Option<String>, query: Option<String>, since_ms: Option<i64>, until_ms: Option<i64>, order: Option<String>, offset: Option<u64>, limit: Option<u64>], Some(route(HttpMethod::Get, "/api/v1/logs"))),
            // Gateway
            (GetGatewayConfig, get_gateway_config, SConfig, op_get_gateway_config, [], Some(route(HttpMethod::Get, "/api/v1/gateway/config"))),
            (UpdateGatewayConfig, update_gateway_config, C, op_update_gateway_config, [patch: Value], Some(route_wrap(HttpMethod::Patch, "/api/v1/gateway/config", "patch"))),
            (GatewayStatus, gateway_status, R, op_gateway_status, [], Some(route(HttpMethod::Get, "/api/v1/gateway/status"))),
            (GatewayStop, gateway_stop, L, op_gateway_stop, [], Some(route(HttpMethod::Post, "/api/v1/gateway/stop"))),
            (GatewayActivationPlan, gateway_activation_plan, R, op_gateway_activation_plan, [], Some(route(HttpMethod::Get, "/api/v1/activation-plan"))),
            (GatewayActivationCancel, gateway_activation_cancel, D, op_gateway_activation_cancel, [], Some(route(HttpMethod::Post, "/api/v1/activation-plan/cancel"))),
            // Internal bookkeeping: performed by the server after a
            // successful apply or gateway (re)start, not a client route.
            (GatewayActivationComplete, gateway_activation_complete, L, op_gateway_activation_complete, [], None),
            // Execution history: the record write is reported by the
            // desktop apply funnel when an apply ends (bookkeeping,
            // not a client route, like gateway_activation_complete);
            // the read is a plain history query.
            (GatewayActivationRecordExecution, gateway_activation_record_execution, C, op_gateway_activation_record_execution, [record: Value], None),
            (GatewayActivationHistory, gateway_activation_history, R, op_gateway_activation_history, [execution_id: Option<String>], Some(route(HttpMethod::Get, "/api/v1/activation-plan/history"))),
            (GatewayQuiesceWait, gateway_quiesce_wait, L, op_gateway_quiesce_wait, [timeout_secs: u64], Some(route(HttpMethod::Post, "/api/v1/activation-plan/quiesce-wait"))),
            (GatewayPreviewYaml, gateway_preview_yaml, R, op_gateway_preview_yaml, [], Some(route(HttpMethod::Get, "/api/v1/gateway/generated-config"))),
            (GatewayTestRouting, gateway_test_routing, R, op_gateway_test_routing, [request: Value], Some(route_wrap(HttpMethod::Post, "/api/v1/gateway/routing/test", "request"))),
            (GatewayRecentRequests, gateway_recent_requests, R, op_gateway_recent_requests, [], Some(route(HttpMethod::Get, "/api/v1/gateway/routing/recent"))),
            // Group membership (predicted-throughput design §8.5):
            // the move op carries a route — no API-layer composition
            // performs move semantics (remove from every group, join
            // exactly one). Create/delete are the desktop surface
            // for the group lifecycle with the app's own rules
            // (slug names, delete-empty-only); REST group CRUD is
            // served by the API-layer derived routes over
            // update_gateway_config (rest-api.md §6.17), so they
            // are explicit exclusions here, like model_set_short_name.
            (GatewaySetBackendGroup, gateway_set_backend_group, C, op_gateway_set_backend_group, [id: String, group: Option<String>], Some(route(HttpMethod::Put, "/api/v1/backends/{id}/group"))),
            (GatewayCreateGroup, gateway_create_group, C, op_gateway_create_group, [name: String], None),
            (GatewayDeleteGroup, gateway_delete_group, D, op_gateway_delete_group, [name: String], None),
            // Telemetry / placement
            (GetGpuTelemetry, get_gpu_telemetry, R, op_get_gpu_telemetry, [], Some(route(HttpMethod::Get, "/api/v1/telemetry/gpus"))),
            (TelemetrySetInterval, telemetry_set_interval, C, op_telemetry_set_interval, [ms: u64], Some(route(HttpMethod::Put, "/api/v1/telemetry/interval"))),
            (TelemetryResetPeaks, telemetry_reset_peaks, L, op_telemetry_reset_peaks, [], Some(route(HttpMethod::Post, "/api/v1/telemetry/reset-peaks"))),
            (TelemetrySaveSnapshot, telemetry_save_snapshot, L, op_telemetry_save_snapshot, [], Some(route(HttpMethod::Post, "/api/v1/telemetry/snapshots"))),
            (PlacementPropose, placement_propose, R, op_placement_propose, [model_id: String, split: Vec<f32>, backend_id: Option<String>], Some(route(HttpMethod::Post, "/api/v1/placement/propose"))),
            (PlacementApply, placement_apply, C, op_placement_apply, [model_id: String, split: Vec<f32>, backend_id: Option<String>], Some(route(HttpMethod::Post, "/api/v1/placement/apply"))),
            // Tasks / diagnostics
            (TaskAbort, task_abort, L, op_task_abort, [task_id: String], Some(route(HttpMethod::Post, "/api/v1/operations/{task_id}/abort"))),
            (TasksList, tasks_list, R, op_tasks_list, [], Some(route(HttpMethod::Get, "/api/v1/operations"))),
            (DiagnosticsExportBundle, diagnostics_export_bundle, SDisclosure, op_diagnostics_export_bundle, [], Some(route_async(HttpMethod::Post, "/api/v1/diagnostics/bundles"))),
            // MCP / VS Code — peripheral, excluded from REST v1
            // (PLAN.md decision 10); Tauri surface unchanged.
            (McpList, mcp_list, R, op_mcp_list, [], None),
            (McpStartPreflight, mcp_start_preflight, R, op_mcp_start_preflight, [], None),
            (McpTestConnection, mcp_test_connection, X, op_mcp_test_connection, [id: String], None),
            (McpSetExposure, mcp_set_exposure, X, op_mcp_set_exposure, [id: String, exposure: String], None),
            (McpSaveServer, mcp_save_server, X, op_mcp_save_server, [server: manager_config::McpServerConfig], None),
            (McpDeleteServer, mcp_delete_server, X, op_mcp_delete_server, [id: String], None),
            (McpStoreCredential, mcp_store_credential, X, op_mcp_store_credential, [id: String, value: String], None),
            (BackendTestToolCall, backend_test_tool_call, L, op_backend_test_tool_call, [id: String], None),
            // Notifications — desktop-only (toast delivery is a shell
            // concern); the config itself rides update_config.
            (NotificationsSetConfig, notifications_set_config, C, op_notifications_set_config, [config: manager_config::NotificationConfig], None),
            (NotificationsSendTest, notifications_send_test, L, op_notifications_send_test, [], None),
            // Windows services — desktop-only (SCM elevation is a
            // shell concern in v1).
            (ServicesList, services_list, R, op_services_list, [], None),
            (ServicesRegister, services_register, D, op_services_register, [target: String, password: Option<String>], None),
            (ServicesUnregister, services_unregister, D, op_services_unregister, [target: String], None),
            // Optimize — desktop-only in v1 (consultation spends local
            // backend capacity; plan state lives beside the config).
            (OptimizeGetState, optimize_get_state, R, op_optimize_get_state, [], None),
            (OptimizeValidatePlan, optimize_validate_plan, R, op_optimize_validate_plan, [], None),
            (OptimizeSavePurpose, optimize_save_purpose, C, op_optimize_save_purpose, [scope: String, target: Option<String>, text: String], None),
            (OptimizeSetItemState, optimize_set_item_state, C, op_optimize_set_item_state, [item_id: usize, decision: String], None),
            (OptimizeSetItemOutcome, optimize_set_item_outcome, C, op_optimize_set_item_outcome, [item_id: usize, outcome: String, error: Option<String>], None),
            (OptimizeDiscardPlan, optimize_discard_plan, C, op_optimize_discard_plan, [], None),
            (OptimizeSetExperimentState, optimize_set_experiment_state, C, op_optimize_set_experiment_state, [experiment_id: usize, decision: String], None),
            (OptimizeConsult, optimize_consult, L, op_optimize_consult, [backend_id: Option<String>, sample: Option<bool>, directive: Option<String>, candidate_repos: Option<Vec<String>>], None),
            (OptimizeDiscover, optimize_discover, L, op_optimize_discover, [directive: String], None),
            (OptimizeCancelConsult, optimize_cancel_consult, L, op_optimize_cancel_consult, [], None),
            (OptimizeMeasureBackend, optimize_measure_backend, L, op_optimize_measure_backend, [backend_id: String], None),
            // Predicted throughput (design §6.2, §10): the lookup
            // is a pure read over the measurement store plus the
            // live configuration; the delete is the history
            // view's per-entry control. Both carry REST routes
            // by design §11, unlike the rest of this section.
            (MeasurementLookup, measurement_lookup, R, op_measurement_lookup, [request: Value], Some(route_wrap(HttpMethod::Post, "/api/v1/measurements/lookup", "request"))),
            (MeasurementDelete, measurement_delete, D, op_measurement_delete, [fingerprint: String, measured_at: String], Some(route(HttpMethod::Delete, "/api/v1/measurements"))),
            (OptimizeEstimateBackend, optimize_estimate_backend, L, op_optimize_estimate_backend, [backend_id: String], None),
            (OptimizeCancelJob, optimize_cancel_job, L, op_optimize_cancel_job, [], None),
            (OptimizeRunBaseline, optimize_run_baseline, L, op_optimize_run_baseline, [backend_id: String], None),
            (OptimizeRunExperiments, optimize_run_experiments, L, op_optimize_run_experiments, [], None),
            (OptimizeEstimateExperiments, optimize_estimate_experiments, L, op_optimize_estimate_experiments, [], None),
            (VscodeInstallExtensions, vscode_install_extensions, X, op_vscode_install_extensions, [], None),
            (VscodeWriteConfig, vscode_write_config, X, op_vscode_write_config, [backup: bool], None),
            // MXC
            (GetMxcPolicy, get_mxc_policy, SConfig, op_get_mxc_policy, [], Some(route(HttpMethod::Get, "/api/v1/mxc/policy"))),
            (UpdateMxcPolicy, update_mxc_policy, X, op_update_mxc_policy, [policy: Value], Some(route_wrap(HttpMethod::Put, "/api/v1/mxc/policy", "policy"))),
            (MxcSetMode, mxc_set_mode, X, op_mxc_set_mode, [mode: manager_config::MxcMode], Some(route(HttpMethod::Put, "/api/v1/mxc/mode"))),
            (MxcRunSelfTest, mxc_run_self_test, R, op_mxc_run_self_test, [], Some(route(HttpMethod::Post, "/api/v1/mxc/self-test"))),
            (MxcGetActivityReport, mxc_get_activity_report, R, op_mxc_get_activity_report, [since_ms: Option<u64>], Some(route(HttpMethod::Get, "/api/v1/mxc/activity"))),
            (MxcProposeTightening, mxc_propose_tightening, R, op_mxc_propose_tightening, [report: Value], Some(route_wrap(HttpMethod::Post, "/api/v1/mxc/tightening-proposals", "report"))),
            // Windows ML — peripheral, excluded from REST v1 (decision 10).
            (WinmlProbe, winml_probe, R, op_winml_probe, [], None),
            (WinmlRegister, winml_register, D, op_winml_register, [model_id: String], None),
            (WinmlUnregister, winml_unregister, D, op_winml_unregister, [], None),
            (WinmlStatus, winml_status, R, op_winml_status, [], None),
            // Updates
            (UpdatesCheckAll, updates_check_all, R, op_updates_check_all, [], Some(route(HttpMethod::Post, "/api/v1/updates/check"))),
            (UpdatesApply, updates_apply, D, op_updates_apply, [component_id: String], Some(route(HttpMethod::Post, "/api/v1/updates/components/{component_id}/apply"))),
            (UpdatesPin, updates_pin, C, op_updates_pin, [component_id: String, version: Option<String>], Some(route(HttpMethod::Put, "/api/v1/updates/components/{component_id}/pin"))),
            (UpdatesSetAuto, updates_set_auto, C, op_updates_set_auto, [component_id: String, enabled: bool], Some(route(HttpMethod::Put, "/api/v1/updates/components/{component_id}/auto")))
        }
    };
}

// --- Generated from the single declaration above ---------------------------

macro_rules! gen_op_enum {
    ($(($variant:ident, $name:ident, $class:ident, $op:ident, [$($pname:ident : $pty:ty),*], $route:expr)),*) => {
        /// One variant per registered operation. Generated; do not edit.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Op {
            $($variant),*
        }
    };
}
for_each_operation!(gen_op_enum);

macro_rules! gen_op_specs {
    ($(($variant:ident, $name:ident, $class:ident, $op:ident, [$($pname:ident : $pty:ty),*], $route:expr)),*) => {
        /// Every registered operation with its class and REST route.
        /// Generated from the same declaration as [`Op`] and [`dispatch`].
        pub static OP_SPECS: &[OpSpec] = &[
            $(OpSpec {
                op: Op::$variant,
                name: stringify!($name),
                class: CapClass::$class,
                params: &[$(stringify!($pname)),*],
                route: $route,
            }),*
        ];
    };
}
for_each_operation!(gen_op_specs);

macro_rules! gen_dispatch {
    ($(($variant:ident, $name:ident, $class:ident, $op:ident, [$($pname:ident : $pty:ty),*], $route:expr)),*) => {
        /// Run one operation by its registry handle. The match is
        /// exhaustive with no wildcard arm — a registered operation
        /// without a dispatch arm does not compile.
        pub async fn dispatch(
            state: &Arc<AppState>,
            sink: &Arc<dyn EventSink>,
            op: Op,
            args: Value,
        ) -> Result<Value, ApiError> {
            match op {
                $(
                    Op::$variant => {
                        crate::$op(state, sink, $(arg::<$pty>(&args, stringify!($pname))?),*).await
                    }
                )*
            }
        }
    };
}
for_each_operation!(gen_dispatch);

/// The spec for one operation.
pub fn spec(op: Op) -> &'static OpSpec {
    OP_SPECS
        .iter()
        .find(|s| s.op == op)
        .expect("every Op has a spec")
}

/// Look an operation up by its canonical (Tauri command) name.
pub fn op_by_name(name: &str) -> Option<Op> {
    OP_SPECS.iter().find(|s| s.name == name).map(|s| s.op)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_covers_all_82_commands() {
        assert_eq!(OP_SPECS.len(), 120);
    }

    #[test]
    fn names_and_routes_are_unique() {
        let mut names: Vec<&str> = OP_SPECS.iter().map(|s| s.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), OP_SPECS.len(), "duplicate operation names");
        let mut routes: Vec<String> = OP_SPECS
            .iter()
            .filter_map(|s| s.route)
            .map(|r| format!("{:?} {}", r.method, r.path))
            .collect();
        routes.sort();
        routes.dedup();
        let route_count = OP_SPECS.iter().filter(|s| s.route.is_some()).count();
        assert_eq!(routes.len(), route_count, "duplicate REST routes");
    }

    #[test]
    fn every_spec_round_trips_through_lookup() {
        for s in OP_SPECS {
            assert_eq!(spec(s.op).name, s.name);
            assert_eq!(op_by_name(s.name), Some(s.op));
        }
    }

    /// The §3.5 matrix, row by row, for every class an operation can
    /// carry. If a class is added, `role_allows` stops compiling
    /// until its row exists, and this test pins the decided grants.
    #[test]
    fn role_matrix_matches_plan() {
        use CapClass::*;
        use Role::*;
        // (class, viewer, operator, configurator, admin)
        let rows = [
            (R, true, true, true, true),
            (SConfig, true, true, true, true),
            (SDisclosure, false, false, false, true),
            (L, false, true, true, true),
            (C, false, false, true, true),
            (D, false, false, false, true),
            (X, false, false, false, true),
            (UserAdmin, false, false, false, true),
            (ApiSettings, false, false, false, true),
        ];
        for (class, v, o, c, a) in rows {
            assert_eq!(role_allows(Viewer, class), v, "{class:?} viewer");
            assert_eq!(role_allows(Operator, class), o, "{class:?} operator");
            assert_eq!(
                role_allows(Configurator, class),
                c,
                "{class:?} configurator"
            );
            assert_eq!(role_allows(Admin, class), a, "{class:?} admin");
        }
    }

    /// Every command name the frontend invokes must be registered.
    /// (The capability audit found `backend_test_request` invoked
    /// where the command is `test_backend`; at v0.1.33 the call site
    /// is already correct — this test keeps the whole class of
    /// defect from coming back.)
    #[test]
    fn frontend_invocations_are_all_registered() {
        let api_ts = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/src/api.ts");
        let src = std::fs::read_to_string(&api_ts)
            .unwrap_or_else(|e| panic!("read {}: {e}", api_ts.display()));
        // Scan for invoke<T>("name", ...) / invoke("name", ...) call
        // sites: after `invoke`, allow a generic argument list, then
        // `(`, then the quoted command name as the first argument.
        let mut invoked: Vec<String> = Vec::new();
        for (i, _) in src.match_indices("invoke") {
            let bytes = src.as_bytes();
            let mut j = i + "invoke".len();
            let skip_ws = |j: &mut usize| {
                while *j < bytes.len() && (bytes[*j] as char).is_whitespace() {
                    *j += 1;
                }
            };
            skip_ws(&mut j);
            if j < bytes.len() && bytes[j] == b'<' {
                let mut depth = 0i32;
                while j < bytes.len() {
                    match bytes[j] {
                        b'<' => depth += 1,
                        b'>' => {
                            depth -= 1;
                            if depth == 0 {
                                j += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                skip_ws(&mut j);
            }
            if j >= bytes.len() || bytes[j] != b'(' {
                continue;
            }
            j += 1;
            skip_ws(&mut j);
            if j >= bytes.len() || bytes[j] != b'"' {
                continue;
            }
            let start = j + 1;
            let Some(end) = src[start..].find('"') else {
                continue;
            };
            let name = &src[start..start + end];
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
                invoked.push(name.to_string());
            }
        }
        assert!(
            invoked.len() >= 50,
            "scan found only {} invokes",
            invoked.len()
        );
        // Desktop-local shell commands (admin API lifecycle and
        // user administration) are not registry operations — they
        // manage the server itself, not app capabilities. They are
        // allowlisted here AND must exist in the shell source, so a
        // frontend invocation still cannot drift onto a surface
        // that does not provide it.
        const SHELL_LOCAL: &[&str] = &[
            "admin_api_get_status",
            "admin_api_set_enabled",
            "admin_api_list_users",
            "admin_api_create_user",
            "admin_api_update_user",
            "admin_api_delete_user",
            "admin_api_reset_password",
            "admin_api_get_pairing",
            "admin_api_create_enrollment",
            "admin_api_regenerate_cert",
            "admin_api_get_audit",
        ];
        let shell_src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../src-tauri/src/main.rs"
        ))
        .expect("shell source readable");
        for name in &invoked {
            if op_by_name(name).is_some() {
                continue;
            }
            assert!(
                SHELL_LOCAL.contains(&name.as_str()),
                "ui/src/api.ts invokes unregistered command `{name}`"
            );
            assert!(
                shell_src.contains(name),
                "shell-local command `{name}` is allowlisted but absent from src-tauri/src/main.rs"
            );
        }
    }

    #[tokio::test]
    async fn dispatch_validates_arguments_before_running() {
        // test_backend requires id + prompt; an empty argument object
        // must fail as InvalidInput without touching any state.
        let state = Arc::new(AppState::new(
            manager_config::AppConfig::default_config(),
            std::path::PathBuf::from("/nonexistent/config.json"),
            std::path::PathBuf::from("/nonexistent"),
        ));
        let sink: Arc<dyn EventSink> = Arc::new(crate::NoopSink);
        let err = dispatch(&state, &sink, Op::TestBackend, serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::InvalidInput(_)), "{err}");
    }

    #[tokio::test]
    async fn dispatch_reaches_the_operation() {
        // updates_check_all is a stub that always errors Unsupported;
        // reaching it through dispatch proves the arm is wired.
        let state = Arc::new(AppState::new(
            manager_config::AppConfig::default_config(),
            std::path::PathBuf::from("/nonexistent/config.json"),
            std::path::PathBuf::from("/nonexistent"),
        ));
        let sink: Arc<dyn EventSink> = Arc::new(crate::NoopSink);
        let err = dispatch(&state, &sink, Op::UpdatesCheckAll, serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::Unsupported(_)), "{err}");
    }
}
