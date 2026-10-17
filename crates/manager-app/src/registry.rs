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

/// One operation's declared REST route: method, path template (path
/// placeholders are named after the operation's arguments — the API
/// layer merges them into the argument object by name), and an
/// optional argument name the whole request body is wrapped under
/// (for operations whose single argument is the body document
/// itself: `patch`, `request`, `report`, `policy`). `None` in the
/// wrap slot means body fields merge into the argument object.
pub type RouteSpec = (HttpMethod, &'static str, Option<&'static str>);

/// The static description of one registered operation.
#[derive(Debug, Clone, Copy)]
pub struct OpSpec {
    pub op: Op,
    /// The Tauri command name (also the operation's canonical name).
    pub name: &'static str,
    pub class: CapClass,
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
            (GetStatus, get_status, R, op_get_status, [], Some((HttpMethod::Get, "/api/v1/status", None))),
            (GetConfig, get_config, SConfig, op_get_config, [], Some((HttpMethod::Get, "/api/v1/config", None))),
            // Excluded from REST v1 (PLAN.md decision 11: the broad raw
            // config patch; domain endpoints cover the same ground).
            (UpdateConfig, update_config, C, op_update_config, [patch: Value], None),
            // Backends
            (ListBackends, list_backends, R, op_list_backends, [], Some((HttpMethod::Get, "/api/v1/backends", None))),
            (StartBackend, start_backend, L, op_start_backend, [id: String], Some((HttpMethod::Post, "/api/v1/backends/{id}/start", None))),
            (StopBackend, stop_backend, L, op_stop_backend, [id: String, graceful: bool], Some((HttpMethod::Post, "/api/v1/backends/{id}/stop", None))),
            (RestartBackend, restart_backend, L, op_restart_backend, [id: String, graceful: bool], Some((HttpMethod::Post, "/api/v1/backends/{id}/restart", None))),
            (TestBackend, test_backend, L, op_test_backend, [id: String, prompt: String], Some((HttpMethod::Post, "/api/v1/backends/{id}/test", None))),
            (BackendGetConfig, backend_get_config, SConfig, op_backend_get_config, [id: String], Some((HttpMethod::Get, "/api/v1/backends/{id}/config", None))),
            (BackendPreviewDiff, backend_preview_diff, R, op_backend_preview_diff, [id: String, patch: Value], Some((HttpMethod::Post, "/api/v1/backends/{id}/config/preview-diff", None))),
            (BackendApplyConfig, backend_apply_config, C, op_backend_apply_config, [id: String, patch: Value, apply_mode: String], Some((HttpMethod::Patch, "/api/v1/backends/{id}/config", None))),
            (BackendAdd, backend_add, C, op_backend_add, [id: String, model_id: String, port: u16, group: String, create_group: bool, enabled: bool], Some((HttpMethod::Post, "/api/v1/backends", None))),
            (BackendUpdateFlags, backend_update_flags, C, op_backend_update_flags, [id: String, n_ctx: u32, n_batch: u32], Some((HttpMethod::Put, "/api/v1/backends/{id}/flags", None))),
            (BackendSetOffload, backend_set_offload, C, op_backend_set_offload, [id: String, layers: u32], Some((HttpMethod::Put, "/api/v1/backends/{id}/offload", None))),
            (BackendDelete, backend_delete, D, op_backend_delete, [id: String], Some((HttpMethod::Delete, "/api/v1/backends/{id}", None))),
            (BackendGetLaunchCommand, backend_get_launch_command, SDisclosure, op_backend_get_launch_command, [id: String], Some((HttpMethod::Get, "/api/v1/backends/{id}/launch-command", None))),
            // Wizard / setup
            (RunWizardStep, run_wizard_step, D, op_run_wizard_step, [step_id: String], Some((HttpMethod::Post, "/api/v1/setup/steps/{step_id}/run", None))),
            (RunAudit, run_audit, L, op_run_audit, [], Some((HttpMethod::Post, "/api/v1/setup/audit", None))),
            (SetupGetPrerequisites, setup_get_prerequisites, R, op_setup_get_prerequisites, [], Some((HttpMethod::Get, "/api/v1/setup/prerequisites", None))),
            (SetupRetryStep, setup_retry_step, D, op_setup_retry_step, [step_id: String], Some((HttpMethod::Post, "/api/v1/setup/steps/{step_id}/retry", None))),
            (SetupSkipStep, setup_skip_step, C, op_setup_skip_step, [step_id: String, note: String], Some((HttpMethod::Post, "/api/v1/setup/steps/{step_id}/skip", None))),
            (SetupGetCommands, setup_get_commands, R, op_setup_get_commands, [step_id: String], Some((HttpMethod::Get, "/api/v1/setup/steps/{step_id}/commands", None))),
            (SetupMarkManual, setup_mark_manual, C, op_setup_mark_manual, [step_id: String, path: String, version: String], Some((HttpMethod::Post, "/api/v1/setup/steps/{step_id}/mark-manual", None))),
            (SetupGetSteps, setup_get_steps, R, op_setup_get_steps, [], Some((HttpMethod::Get, "/api/v1/setup/steps", None))),
            (SetupStatus, setup_status, R, op_setup_status, [], Some((HttpMethod::Get, "/api/v1/setup/status", None))),
            (SetupMarkComplete, setup_mark_complete, C, op_setup_mark_complete, [], Some((HttpMethod::Post, "/api/v1/setup/complete", None))),
            (SetupStepHelpUrl, setup_step_help_url, R, op_setup_step_help_url, [step_id: String], Some((HttpMethod::Get, "/api/v1/setup/steps/{step_id}/help-url", None))),
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
            (ListModels, list_models, R, op_list_models, [], Some((HttpMethod::Get, "/api/v1/models", None))),
            (DownloadModel, download_model, L, op_download_model, [id: String], Some((HttpMethod::Post, "/api/v1/models/{id}/download", None))),
            (CancelDownload, cancel_download, L, op_cancel_download, [id: String], Some((HttpMethod::Post, "/api/v1/models/{id}/download/cancel", None))),
            (ModelPauseDownload, model_pause_download, L, op_model_pause_download, [id: String], Some((HttpMethod::Post, "/api/v1/models/{id}/download/pause", None))),
            (ModelResumeDownload, model_resume_download, L, op_model_resume_download, [id: String], Some((HttpMethod::Post, "/api/v1/models/{id}/download/resume", None))),
            (ModelVerifyChecksum, model_verify_checksum, L, op_model_verify_checksum, [id: String], Some((HttpMethod::Post, "/api/v1/models/{id}/verify", None))),
            // PATCH /models/{id} is a composite over this and
            // model_set_enabled (rest-api.md §6.3); no 1:1 route.
            (ModelSetShortName, model_set_short_name, C, op_model_set_short_name, [id: String, name: String], None),
            (ModelDelete, model_delete, D, op_model_delete, [id: String], Some((HttpMethod::Delete, "/api/v1/models/{id}", None))),
            (ModelAssign, model_assign, C, op_model_assign, [id: String, backend_id: String, split: Vec<f32>], Some((HttpMethod::Post, "/api/v1/models/{id}/assign", None))),
            (ModelAdd, model_add, C, op_model_add, [id: String, source_url: String, file_name: Option<String>, quant: Option<String>, params_b: Option<f32>], Some((HttpMethod::Post, "/api/v1/models", None))),
            (ModelSetEnabled, model_set_enabled, C, op_model_set_enabled, [id: String, enabled: bool], None),
            // Logs
            (GetLogs, get_logs, R, op_get_logs, [scope: String, level: Option<String>, search: Option<String>, query: Option<String>, since_ms: Option<i64>, until_ms: Option<i64>, order: Option<String>, offset: Option<u64>, limit: Option<u64>], Some((HttpMethod::Get, "/api/v1/logs", None))),
            // Gateway
            (GetGatewayConfig, get_gateway_config, SConfig, op_get_gateway_config, [], Some((HttpMethod::Get, "/api/v1/gateway/config", None))),
            (UpdateGatewayConfig, update_gateway_config, C, op_update_gateway_config, [patch: Value], Some((HttpMethod::Patch, "/api/v1/gateway/config", Some("patch")))),
            (GatewayStatus, gateway_status, R, op_gateway_status, [], Some((HttpMethod::Get, "/api/v1/gateway/status", None))),
            (GatewayStop, gateway_stop, L, op_gateway_stop, [], Some((HttpMethod::Post, "/api/v1/gateway/stop", None))),
            (GatewayActivationPlan, gateway_activation_plan, R, op_gateway_activation_plan, [], Some((HttpMethod::Get, "/api/v1/activation-plan", None))),
            (GatewayActivationCancel, gateway_activation_cancel, D, op_gateway_activation_cancel, [], Some((HttpMethod::Post, "/api/v1/activation-plan/cancel", None))),
            // Internal bookkeeping: performed by the server after a
            // successful apply or gateway (re)start, not a client route.
            (GatewayActivationComplete, gateway_activation_complete, L, op_gateway_activation_complete, [], None),
            (GatewayQuiesceWait, gateway_quiesce_wait, L, op_gateway_quiesce_wait, [timeout_secs: u64], Some((HttpMethod::Post, "/api/v1/activation-plan/quiesce-wait", None))),
            (GatewayPreviewYaml, gateway_preview_yaml, R, op_gateway_preview_yaml, [], Some((HttpMethod::Get, "/api/v1/gateway/generated-config", None))),
            (GatewayTestRouting, gateway_test_routing, R, op_gateway_test_routing, [request: Value], Some((HttpMethod::Post, "/api/v1/gateway/routing/test", Some("request")))),
            (GatewayRecentRequests, gateway_recent_requests, R, op_gateway_recent_requests, [], Some((HttpMethod::Get, "/api/v1/gateway/routing/recent", None))),
            // Telemetry / placement
            (GetGpuTelemetry, get_gpu_telemetry, R, op_get_gpu_telemetry, [], Some((HttpMethod::Get, "/api/v1/telemetry/gpus", None))),
            (TelemetrySetInterval, telemetry_set_interval, C, op_telemetry_set_interval, [ms: u64], Some((HttpMethod::Put, "/api/v1/telemetry/interval", None))),
            (TelemetryResetPeaks, telemetry_reset_peaks, L, op_telemetry_reset_peaks, [], Some((HttpMethod::Post, "/api/v1/telemetry/reset-peaks", None))),
            (TelemetrySaveSnapshot, telemetry_save_snapshot, L, op_telemetry_save_snapshot, [], Some((HttpMethod::Post, "/api/v1/telemetry/snapshots", None))),
            (PlacementPropose, placement_propose, R, op_placement_propose, [model_id: String, split: Vec<f32>, backend_id: Option<String>], Some((HttpMethod::Post, "/api/v1/placement/propose", None))),
            (PlacementApply, placement_apply, C, op_placement_apply, [model_id: String, split: Vec<f32>, backend_id: Option<String>], Some((HttpMethod::Post, "/api/v1/placement/apply", None))),
            // Tasks / diagnostics
            (TaskAbort, task_abort, L, op_task_abort, [task_id: String], Some((HttpMethod::Post, "/api/v1/operations/{task_id}/abort", None))),
            (TasksList, tasks_list, R, op_tasks_list, [], Some((HttpMethod::Get, "/api/v1/operations", None))),
            (DiagnosticsExportBundle, diagnostics_export_bundle, SDisclosure, op_diagnostics_export_bundle, [], Some((HttpMethod::Post, "/api/v1/diagnostics/bundles", None))),
            // MCP / VS Code — peripheral, excluded from REST v1
            // (PLAN.md decision 10); Tauri surface unchanged.
            (McpList, mcp_list, R, op_mcp_list, [], None),
            (McpTestConnection, mcp_test_connection, X, op_mcp_test_connection, [id: String], None),
            (McpSetMode, mcp_set_mode, X, op_mcp_set_mode, [id: String, mode: String], None),
            (VscodeInstallExtensions, vscode_install_extensions, X, op_vscode_install_extensions, [], None),
            (VscodeWriteConfig, vscode_write_config, X, op_vscode_write_config, [backup: bool], None),
            // MXC
            (GetMxcPolicy, get_mxc_policy, SConfig, op_get_mxc_policy, [], Some((HttpMethod::Get, "/api/v1/mxc/policy", None))),
            (UpdateMxcPolicy, update_mxc_policy, X, op_update_mxc_policy, [policy: Value], Some((HttpMethod::Put, "/api/v1/mxc/policy", Some("policy")))),
            (MxcSetMode, mxc_set_mode, X, op_mxc_set_mode, [mode: manager_config::MxcMode], Some((HttpMethod::Put, "/api/v1/mxc/mode", None))),
            (MxcRunSelfTest, mxc_run_self_test, R, op_mxc_run_self_test, [], Some((HttpMethod::Post, "/api/v1/mxc/self-test", None))),
            (MxcGetActivityReport, mxc_get_activity_report, R, op_mxc_get_activity_report, [since_ms: Option<u64>], Some((HttpMethod::Get, "/api/v1/mxc/activity", None))),
            (MxcProposeTightening, mxc_propose_tightening, R, op_mxc_propose_tightening, [report: Value], Some((HttpMethod::Post, "/api/v1/mxc/tightening-proposals", Some("report")))),
            // Windows ML — peripheral, excluded from REST v1 (decision 10).
            (WinmlProbe, winml_probe, R, op_winml_probe, [], None),
            (WinmlRegister, winml_register, D, op_winml_register, [model_id: String], None),
            (WinmlUnregister, winml_unregister, D, op_winml_unregister, [], None),
            (WinmlStatus, winml_status, R, op_winml_status, [], None),
            // Updates
            (UpdatesCheckAll, updates_check_all, R, op_updates_check_all, [], Some((HttpMethod::Post, "/api/v1/updates/check", None))),
            (UpdatesApply, updates_apply, D, op_updates_apply, [component_id: String], Some((HttpMethod::Post, "/api/v1/updates/components/{component_id}/apply", None))),
            (UpdatesPin, updates_pin, C, op_updates_pin, [component_id: String, version: Option<String>], Some((HttpMethod::Put, "/api/v1/updates/components/{component_id}/pin", None))),
            (UpdatesSetAuto, updates_set_auto, C, op_updates_set_auto, [component_id: String, enabled: bool], Some((HttpMethod::Put, "/api/v1/updates/components/{component_id}/auto", None)))
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
        assert_eq!(OP_SPECS.len(), 82);
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
            .map(|(m, p, _)| format!("{m:?} {p}"))
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
        for name in &invoked {
            assert!(
                op_by_name(name).is_some(),
                "ui/src/api.ts invokes unregistered command `{name}`"
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
