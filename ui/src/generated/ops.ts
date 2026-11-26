// GENERATED from the manager-app operation registry
// (crates/manager-app/src/registry.rs, for_each_operation!) — do not edit.
// Regenerate: UPDATE_GENERATED=1 cargo test -p manager-app --lib generated_
//
// Argument keys are camelCase: the desktop shell binds each snake_case
// Rust parameter from its camelCase key. The snake_case argument object
// the admin API accepts is described in ops.schema.json.

/** Every registered operation name. */
export type OpName =
  | "get_status"
  | "get_config"
  | "update_config"
  | "settings_set_supervision"
  | "list_backends"
  | "start_backend"
  | "stop_backend"
  | "restart_backend"
  | "test_backend"
  | "backend_get_config"
  | "backend_preview_diff"
  | "backend_apply_config"
  | "backend_add"
  | "backend_add_preview"
  | "backend_update_flags"
  | "backend_set_offload"
  | "backend_set_offload_override"
  | "backend_clear_offload_override"
  | "backend_set_mtp"
  | "backend_set_parallel"
  | "backend_set_cache"
  | "backend_set_moe_cache"
  | "backend_cache_preview"
  | "backend_set_raw_flags"
  | "backend_set_flags_mode"
  | "backend_delete"
  | "backend_get_launch_command"
  | "run_wizard_step"
  | "run_audit"
  | "setup_get_prerequisites"
  | "setup_retry_step"
  | "setup_skip_step"
  | "setup_get_commands"
  | "setup_mark_manual"
  | "setup_get_steps"
  | "setup_status"
  | "setup_mark_complete"
  | "setup_step_help_url"
  | "open_url"
  | "open_folder"
  | "gateway_master_key"
  | "gateway_set_bind_host"
  | "list_models"
  | "download_model"
  | "cancel_download"
  | "model_pause_download"
  | "model_resume_download"
  | "model_verify_checksum"
  | "model_set_short_name"
  | "model_delete"
  | "model_assign"
  | "model_add"
  | "model_hf_lookup"
  | "model_hf_header"
  | "model_set_enabled"
  | "model_set_offload_spec"
  | "model_set_mtp"
  | "model_storage_status"
  | "model_storage_set"
  | "model_storage_promote"
  | "model_storage_demote"
  | "model_storage_cancel"
  | "model_set_keep_hot"
  | "get_logs"
  | "get_gateway_config"
  | "update_gateway_config"
  | "gateway_status"
  | "gateway_restart"
  | "gateway_stop"
  | "postgres_stop"
  | "gateway_activation_plan"
  | "gateway_activation_cancel"
  | "activation_apply"
  | "gateway_activation_complete"
  | "gateway_activation_record_execution"
  | "gateway_activation_history"
  | "gateway_quiesce_wait"
  | "gateway_preview_yaml"
  | "gateway_test_routing"
  | "gateway_recent_requests"
  | "gateway_set_backend_group"
  | "gateway_create_group"
  | "gateway_delete_group"
  | "get_gpu_telemetry"
  | "telemetry_set_interval"
  | "telemetry_reset_peaks"
  | "telemetry_save_snapshot"
  | "get_gpu_history"
  | "placement_propose"
  | "placement_apply"
  | "task_abort"
  | "tasks_list"
  | "diagnostics_export_bundle"
  | "mcp_list"
  | "mcp_start_preflight"
  | "mcp_test_connection"
  | "mcp_set_exposure"
  | "mcp_save_server"
  | "mcp_delete_server"
  | "mcp_store_credential"
  | "backend_test_tool_call"
  | "notifications_set_config"
  | "notifications_send_test"
  | "services_list"
  | "services_register"
  | "services_unregister"
  | "log_receiver_status"
  | "log_receiver_deploy"
  | "log_receiver_remove"
  | "log_receiver_flip_endpoint"
  | "log_receiver_start"
  | "log_receiver_stop"
  | "log_receiver_restart"
  | "optimize_get_state"
  | "optimize_validate_plan"
  | "optimize_save_purpose"
  | "optimize_set_item_state"
  | "optimize_set_item_outcome"
  | "optimize_discard_plan"
  | "optimize_apply_plan"
  | "optimize_set_experiment_state"
  | "optimize_consult"
  | "optimize_discover"
  | "optimize_cancel_consult"
  | "optimize_measure_backend"
  | "measurement_lookup"
  | "measurement_delete"
  | "optimize_estimate_backend"
  | "optimize_cancel_job"
  | "optimize_run_baseline"
  | "optimize_run_experiments"
  | "optimize_estimate_experiments"
  | "bench_prompts_list"
  | "bench_prompt_save"
  | "bench_prompt_delete"
  | "bench_prompt_generate"
  | "bench_prompt_generate_cancel"
  | "bench_run_start"
  | "bench_run_cancel"
  | "bench_results_list"
  | "bench_result_delete"
  | "optimize_tuning_preview"
  | "optimize_tuning_run"
  | "optimize_tuning_adopt"
  | "vscode_install_extensions"
  | "vscode_write_config"
  | "get_mxc_policy"
  | "update_mxc_policy"
  | "mxc_set_mode"
  | "mxc_run_self_test"
  | "mxc_get_activity_report"
  | "mxc_propose_tightening"
  | "winml_probe"
  | "winml_register"
  | "winml_unregister"
  | "winml_status"
  | "updates_check_all"
  | "updates_list"
  | "updates_apply"
  | "updates_pin"
  | "updates_set_auto"
  | "app_update_status"
  | "app_update_check"
  | "app_update_set_checking"
  | "app_update_set_auto"
  | "app_update_download"
  | "app_update_restart"
;

/** Arguments of `get_status`. */
export interface GetStatusArgs {}

/** Arguments of `get_config`. */
export interface GetConfigArgs {}

/** Arguments of `update_config`. */
export interface UpdateConfigArgs {
  readonly patch: unknown;
}

/** Arguments of `settings_set_supervision`. */
export interface SettingsSetSupervisionArgs {
  readonly mode: "process" | "service";
}

/** Arguments of `list_backends`. */
export interface ListBackendsArgs {}

/** Arguments of `start_backend`. */
export interface StartBackendArgs {
  readonly id: string;
  readonly allowWddmOvercommit?: boolean | null;
  readonly origin?: string | null;
}

/** Arguments of `stop_backend`. */
export interface StopBackendArgs {
  readonly id: string;
  readonly graceful: boolean;
  readonly origin?: string | null;
}

/** Arguments of `restart_backend`. */
export interface RestartBackendArgs {
  readonly id: string;
  readonly graceful: boolean;
  readonly allowWddmOvercommit?: boolean | null;
  readonly origin?: string | null;
  readonly waitReadySecs?: number | null;
}

/** Arguments of `test_backend`. */
export interface TestBackendArgs {
  readonly id: string;
  readonly prompt: string;
}

/** Arguments of `backend_get_config`. */
export interface BackendGetConfigArgs {
  readonly id: string;
}

/** Arguments of `backend_preview_diff`. */
export interface BackendPreviewDiffArgs {
  readonly id: string;
  readonly patch: unknown;
}

/** Arguments of `backend_apply_config`. */
export interface BackendApplyConfigArgs {
  readonly id: string;
  readonly patch: unknown;
  readonly applyMode: string;
}

/** Arguments of `backend_add`. */
export interface BackendAddArgs {
  readonly id: string;
  readonly modelId: string;
  readonly port: number;
  readonly group: string;
  readonly createGroup: boolean;
  readonly enabled: boolean;
  readonly nCtx?: number | null;
  readonly nBatch?: number | null;
  readonly offloadLayers?: number | null;
  readonly offloadOverride?: boolean | null;
  readonly gpuIndices?: number[] | null;
  readonly parallel?: number | null;
  readonly cacheK?: string | null;
  readonly cacheV?: string | null;
  readonly flashAttn?: string | null;
}

/** Arguments of `backend_add_preview`. */
export interface BackendAddPreviewArgs {
  readonly modelId: string;
  readonly gpuIndices?: number[] | null;
  readonly nCtx?: number | null;
  readonly parallel?: number | null;
  readonly offloadLayers?: number | null;
}

/** Arguments of `backend_update_flags`. */
export interface BackendUpdateFlagsArgs {
  readonly id: string;
  readonly nCtx: number;
  readonly nBatch: number;
}

/** Arguments of `backend_set_offload`. */
export interface BackendSetOffloadArgs {
  readonly id: string;
  readonly layers: number;
  readonly allowOverride?: boolean | null;
}

/** Arguments of `backend_set_offload_override`. */
export interface BackendSetOffloadOverrideArgs {
  readonly id: string;
  readonly layers: number;
  readonly placedCcs: string[];
}

/** Arguments of `backend_clear_offload_override`. */
export interface BackendClearOffloadOverrideArgs {
  readonly id: string;
}

/** Arguments of `backend_set_mtp`. */
export interface BackendSetMtpArgs {
  readonly id: string;
  readonly nMax?: number | null;
}

/** Arguments of `backend_set_parallel`. */
export interface BackendSetParallelArgs {
  readonly id: string;
  readonly slots?: number | null;
}

/** Arguments of `backend_set_cache`. */
export interface BackendSetCacheArgs {
  readonly id: string;
  readonly cacheK?: string | null;
  readonly cacheV?: string | null;
  readonly flashAttn?: string | null;
}

/** Arguments of `backend_set_moe_cache`. */
export interface BackendSetMoeCacheArgs {
  readonly id: string;
  readonly mib?: number | null;
  readonly auto?: boolean | null;
}

/** Arguments of `backend_cache_preview`. */
export interface BackendCachePreviewArgs {
  readonly id: string;
  readonly nCtx?: number | null;
  readonly slots?: number | null;
  readonly cacheK?: string | null;
  readonly cacheV?: string | null;
  readonly flashAttn?: string | null;
  readonly moeCacheMib?: number | null;
  readonly offloadLayers?: number | null;
  readonly gpuIndices?: number[] | null;
}

/** Arguments of `backend_set_raw_flags`. */
export interface BackendSetRawFlagsArgs {
  readonly id: string;
  readonly rawFlags?: string | null;
}

/** Arguments of `backend_set_flags_mode`. */
export interface BackendSetFlagsModeArgs {
  readonly id: string;
  readonly advanced: boolean;
}

/** Arguments of `backend_delete`. */
export interface BackendDeleteArgs {
  readonly id: string;
}

/** Arguments of `backend_get_launch_command`. */
export interface BackendGetLaunchCommandArgs {
  readonly id: string;
}

/** Arguments of `run_wizard_step`. */
export interface RunWizardStepArgs {
  readonly stepId: string;
  readonly origin?: string | null;
}

/** Arguments of `run_audit`. */
export interface RunAuditArgs {}

/** Arguments of `setup_get_prerequisites`. */
export interface SetupGetPrerequisitesArgs {}

/** Arguments of `setup_retry_step`. */
export interface SetupRetryStepArgs {
  readonly stepId: string;
}

/** Arguments of `setup_skip_step`. */
export interface SetupSkipStepArgs {
  readonly stepId: string;
  readonly note: string;
}

/** Arguments of `setup_get_commands`. */
export interface SetupGetCommandsArgs {
  readonly stepId: string;
}

/** Arguments of `setup_mark_manual`. */
export interface SetupMarkManualArgs {
  readonly stepId: string;
  readonly path: string;
  readonly version: string;
}

/** Arguments of `setup_get_steps`. */
export interface SetupGetStepsArgs {}

/** Arguments of `setup_status`. */
export interface SetupStatusArgs {}

/** Arguments of `setup_mark_complete`. */
export interface SetupMarkCompleteArgs {}

/** Arguments of `setup_step_help_url`. */
export interface SetupStepHelpUrlArgs {
  readonly stepId: string;
}

/** Arguments of `open_url`. */
export interface OpenUrlArgs {
  readonly url: string;
}

/** Arguments of `open_folder`. */
export interface OpenFolderArgs {
  readonly path: string;
}

/** Arguments of `gateway_master_key`. */
export interface GatewayMasterKeyArgs {}

/** Arguments of `gateway_set_bind_host`. */
export interface GatewaySetBindHostArgs {
  readonly host: string;
}

/** Arguments of `list_models`. */
export interface ListModelsArgs {}

/** Arguments of `download_model`. */
export interface DownloadModelArgs {
  readonly id: string;
}

/** Arguments of `cancel_download`. */
export interface CancelDownloadArgs {
  readonly id: string;
}

/** Arguments of `model_pause_download`. */
export interface ModelPauseDownloadArgs {
  readonly id: string;
}

/** Arguments of `model_resume_download`. */
export interface ModelResumeDownloadArgs {
  readonly id: string;
}

/** Arguments of `model_verify_checksum`. */
export interface ModelVerifyChecksumArgs {
  readonly id: string;
}

/** Arguments of `model_set_short_name`. */
export interface ModelSetShortNameArgs {
  readonly id: string;
  readonly name: string;
}

/** Arguments of `model_delete`. */
export interface ModelDeleteArgs {
  readonly id: string;
}

/** Arguments of `model_assign`. */
export interface ModelAssignArgs {
  readonly id: string;
  readonly backendId: string;
  readonly split: number[];
}

/** Arguments of `model_add`. */
export interface ModelAddArgs {
  readonly id: string;
  readonly sourceUrl: string;
  readonly fileName?: string | null;
  readonly quant?: string | null;
  readonly paramsB?: number | null;
  readonly moeLayers?: number | null;
  readonly defaultOffloadLayers?: number | null;
  readonly mtp?: boolean | null;
  readonly sha256?: string | null;
}

/** Arguments of `model_hf_lookup`. */
export interface ModelHfLookupArgs {
  readonly query: string;
  readonly suggest?: boolean | null;
}

/** Arguments of `model_hf_header`. */
export interface ModelHfHeaderArgs {
  readonly repoId: string;
  readonly file: string;
}

/** Arguments of `model_set_enabled`. */
export interface ModelSetEnabledArgs {
  readonly id: string;
  readonly enabled: boolean;
}

/** Arguments of `model_set_offload_spec`. */
export interface ModelSetOffloadSpecArgs {
  readonly id: string;
  readonly moeLayers?: number | null;
  readonly defaultOffloadLayers?: number | null;
}

/** Arguments of `model_set_mtp`. */
export interface ModelSetMtpArgs {
  readonly id: string;
  readonly mtp?: boolean | null;
}

/** Arguments of `model_storage_status`. */
export interface ModelStorageStatusArgs {}

/** Arguments of `model_storage_set`. */
export interface ModelStorageSetArgs {
  readonly storage: unknown;
}

/** Arguments of `model_storage_promote`. */
export interface ModelStoragePromoteArgs {
  readonly target: string;
}

/** Arguments of `model_storage_demote`. */
export interface ModelStorageDemoteArgs {
  readonly target: string;
}

/** Arguments of `model_storage_cancel`. */
export interface ModelStorageCancelArgs {
  readonly target: string;
}

/** Arguments of `model_set_keep_hot`. */
export interface ModelSetKeepHotArgs {
  readonly target: string;
  readonly keep: boolean;
}

/** Arguments of `get_logs`. */
export interface GetLogsArgs {
  readonly scope: string;
  readonly level?: string | null;
  readonly search?: string | null;
  readonly query?: string | null;
  readonly sinceMs?: number | null;
  readonly untilMs?: number | null;
  readonly order?: string | null;
  readonly offset?: number | null;
  readonly limit?: number | null;
}

/** Arguments of `get_gateway_config`. */
export interface GetGatewayConfigArgs {}

/** Arguments of `update_gateway_config`. */
export interface UpdateGatewayConfigArgs {
  readonly patch: unknown;
}

/** Arguments of `gateway_status`. */
export interface GatewayStatusArgs {}

/** Arguments of `gateway_restart`. */
export interface GatewayRestartArgs {
  readonly origin?: string | null;
  readonly drain?: boolean | null;
}

/** Arguments of `gateway_stop`. */
export interface GatewayStopArgs {
  readonly stopPostgres?: boolean | null;
  readonly origin?: string | null;
}

/** Arguments of `postgres_stop`. */
export interface PostgresStopArgs {
  readonly origin?: string | null;
}

/** Arguments of `gateway_activation_plan`. */
export interface GatewayActivationPlanArgs {}

/** Arguments of `gateway_activation_cancel`. */
export interface GatewayActivationCancelArgs {}

/** Arguments of `activation_apply`. */
export interface ActivationApplyArgs {
  readonly mode?: string | null;
  readonly source?: string | null;
}

/** Arguments of `gateway_activation_complete`. */
export interface GatewayActivationCompleteArgs {}

/** Arguments of `gateway_activation_record_execution`. */
export interface GatewayActivationRecordExecutionArgs {
  readonly record: unknown;
}

/** Arguments of `gateway_activation_history`. */
export interface GatewayActivationHistoryArgs {
  readonly executionId?: string | null;
}

/** Arguments of `gateway_quiesce_wait`. */
export interface GatewayQuiesceWaitArgs {
  readonly timeoutSecs: number;
}

/** Arguments of `gateway_preview_yaml`. */
export interface GatewayPreviewYamlArgs {}

/** Arguments of `gateway_test_routing`. */
export interface GatewayTestRoutingArgs {
  readonly request: unknown;
}

/** Arguments of `gateway_recent_requests`. */
export interface GatewayRecentRequestsArgs {}

/** Arguments of `gateway_set_backend_group`. */
export interface GatewaySetBackendGroupArgs {
  readonly id: string;
  readonly group?: string | null;
}

/** Arguments of `gateway_create_group`. */
export interface GatewayCreateGroupArgs {
  readonly name: string;
}

/** Arguments of `gateway_delete_group`. */
export interface GatewayDeleteGroupArgs {
  readonly name: string;
}

/** Arguments of `get_gpu_telemetry`. */
export interface GetGpuTelemetryArgs {}

/** Arguments of `telemetry_set_interval`. */
export interface TelemetrySetIntervalArgs {
  readonly ms: number;
}

/** Arguments of `telemetry_reset_peaks`. */
export interface TelemetryResetPeaksArgs {}

/** Arguments of `telemetry_save_snapshot`. */
export interface TelemetrySaveSnapshotArgs {}

/** Arguments of `get_gpu_history`. */
export interface GetGpuHistoryArgs {
  readonly range: string;
}

/** Arguments of `placement_propose`. */
export interface PlacementProposeArgs {
  readonly modelId: string;
  readonly split: number[];
  readonly backendId?: string | null;
}

/** Arguments of `placement_apply`. */
export interface PlacementApplyArgs {
  readonly modelId: string;
  readonly split: number[];
  readonly backendId?: string | null;
}

/** Arguments of `task_abort`. */
export interface TaskAbortArgs {
  readonly taskId: string;
}

/** Arguments of `tasks_list`. */
export interface TasksListArgs {}

/** Arguments of `diagnostics_export_bundle`. */
export interface DiagnosticsExportBundleArgs {}

/** Arguments of `mcp_list`. */
export interface McpListArgs {}

/** Arguments of `mcp_start_preflight`. */
export interface McpStartPreflightArgs {}

/** Arguments of `mcp_test_connection`. */
export interface McpTestConnectionArgs {
  readonly id: string;
}

/** Arguments of `mcp_set_exposure`. */
export interface McpSetExposureArgs {
  readonly id: string;
  readonly exposure: string;
}

/** Arguments of `mcp_save_server`. */
export interface McpSaveServerArgs {
  readonly server: Record<string, unknown>;
}

/** Arguments of `mcp_delete_server`. */
export interface McpDeleteServerArgs {
  readonly id: string;
}

/** Arguments of `mcp_store_credential`. */
export interface McpStoreCredentialArgs {
  readonly id: string;
  readonly value: string;
}

/** Arguments of `backend_test_tool_call`. */
export interface BackendTestToolCallArgs {
  readonly id: string;
}

/** Arguments of `notifications_set_config`. */
export interface NotificationsSetConfigArgs {
  readonly config: Record<string, unknown>;
}

/** Arguments of `notifications_send_test`. */
export interface NotificationsSendTestArgs {}

/** Arguments of `services_list`. */
export interface ServicesListArgs {}

/** Arguments of `services_register`. */
export interface ServicesRegisterArgs {
  readonly target: string;
}

/** Arguments of `services_unregister`. */
export interface ServicesUnregisterArgs {
  readonly target: string;
}

/** Arguments of `log_receiver_status`. */
export interface LogReceiverStatusArgs {}

/** Arguments of `log_receiver_deploy`. */
export interface LogReceiverDeployArgs {}

/** Arguments of `log_receiver_remove`. */
export interface LogReceiverRemoveArgs {}

/** Arguments of `log_receiver_flip_endpoint`. */
export interface LogReceiverFlipEndpointArgs {}

/** Arguments of `log_receiver_start`. */
export interface LogReceiverStartArgs {
  readonly origin?: string | null;
}

/** Arguments of `log_receiver_stop`. */
export interface LogReceiverStopArgs {
  readonly origin?: string | null;
}

/** Arguments of `log_receiver_restart`. */
export interface LogReceiverRestartArgs {
  readonly origin?: string | null;
}

/** Arguments of `optimize_get_state`. */
export interface OptimizeGetStateArgs {}

/** Arguments of `optimize_validate_plan`. */
export interface OptimizeValidatePlanArgs {}

/** Arguments of `optimize_save_purpose`. */
export interface OptimizeSavePurposeArgs {
  readonly scope: string;
  readonly target?: string | null;
  readonly text: string;
}

/** Arguments of `optimize_set_item_state`. */
export interface OptimizeSetItemStateArgs {
  readonly itemId: number;
  readonly decision: string;
}

/** Arguments of `optimize_set_item_outcome`. */
export interface OptimizeSetItemOutcomeArgs {
  readonly itemId: number;
  readonly outcome: string;
  readonly error?: string | null;
}

/** Arguments of `optimize_discard_plan`. */
export interface OptimizeDiscardPlanArgs {}

/** Arguments of `optimize_apply_plan`. */
export interface OptimizeApplyPlanArgs {
  readonly destructive?: boolean | null;
  readonly dryRun?: boolean | null;
}

/** Arguments of `optimize_set_experiment_state`. */
export interface OptimizeSetExperimentStateArgs {
  readonly experimentId: number;
  readonly decision: string;
}

/** Arguments of `optimize_consult`. */
export interface OptimizeConsultArgs {
  readonly backendId?: string | null;
  readonly sample?: boolean | null;
  readonly directive?: string | null;
  readonly candidateRepos?: string[] | null;
  readonly replace?: boolean | null;
}

/** Arguments of `optimize_discover`. */
export interface OptimizeDiscoverArgs {
  readonly directive: string;
}

/** Arguments of `optimize_cancel_consult`. */
export interface OptimizeCancelConsultArgs {}

/** Arguments of `optimize_measure_backend`. */
export interface OptimizeMeasureBackendArgs {
  readonly backendId: string;
}

/** Arguments of `measurement_lookup`. */
export interface MeasurementLookupArgs {
  readonly request: unknown;
}

/** Arguments of `measurement_delete`. */
export interface MeasurementDeleteArgs {
  readonly fingerprint: string;
  readonly measuredAt: string;
}

/** Arguments of `optimize_estimate_backend`. */
export interface OptimizeEstimateBackendArgs {
  readonly backendId: string;
}

/** Arguments of `optimize_cancel_job`. */
export interface OptimizeCancelJobArgs {}

/** Arguments of `optimize_run_baseline`. */
export interface OptimizeRunBaselineArgs {
  readonly backendId: string;
}

/** Arguments of `optimize_run_experiments`. */
export interface OptimizeRunExperimentsArgs {}

/** Arguments of `optimize_estimate_experiments`. */
export interface OptimizeEstimateExperimentsArgs {}

/** Arguments of `bench_prompts_list`. */
export interface BenchPromptsListArgs {}

/** Arguments of `bench_prompt_save`. */
export interface BenchPromptSaveArgs {
  readonly entry: unknown;
}

/** Arguments of `bench_prompt_delete`. */
export interface BenchPromptDeleteArgs {
  readonly id: string;
}

/** Arguments of `bench_prompt_generate`. */
export interface BenchPromptGenerateArgs {
  readonly purpose: string;
  readonly description: string;
  readonly size: string;
  readonly backendId?: string | null;
}

/** Arguments of `bench_prompt_generate_cancel`. */
export interface BenchPromptGenerateCancelArgs {}

/** Arguments of `bench_run_start`. */
export interface BenchRunStartArgs {
  readonly request: unknown;
}

/** Arguments of `bench_run_cancel`. */
export interface BenchRunCancelArgs {
  readonly sweepId: string;
}

/** Arguments of `bench_results_list`. */
export interface BenchResultsListArgs {}

/** Arguments of `bench_result_delete`. */
export interface BenchResultDeleteArgs {
  readonly sweepId: string;
}

/** Arguments of `optimize_tuning_preview`. */
export interface OptimizeTuningPreviewArgs {
  readonly backendId: string;
}

/** Arguments of `optimize_tuning_run`. */
export interface OptimizeTuningRunArgs {
  readonly backendId: string;
}

/** Arguments of `optimize_tuning_adopt`. */
export interface OptimizeTuningAdoptArgs {
  readonly sweepId: string;
  readonly adoptAnyway?: boolean | null;
}

/** Arguments of `vscode_install_extensions`. */
export interface VscodeInstallExtensionsArgs {}

/** Arguments of `vscode_write_config`. */
export interface VscodeWriteConfigArgs {
  readonly backup: boolean;
}

/** Arguments of `get_mxc_policy`. */
export interface GetMxcPolicyArgs {}

/** Arguments of `update_mxc_policy`. */
export interface UpdateMxcPolicyArgs {
  readonly policy: unknown;
}

/** Arguments of `mxc_set_mode`. */
export interface MxcSetModeArgs {
  readonly mode: "learning" | "enforced" | "disabled";
}

/** Arguments of `mxc_run_self_test`. */
export interface MxcRunSelfTestArgs {}

/** Arguments of `mxc_get_activity_report`. */
export interface MxcGetActivityReportArgs {
  readonly sinceMs?: number | null;
}

/** Arguments of `mxc_propose_tightening`. */
export interface MxcProposeTighteningArgs {
  readonly report: unknown;
}

/** Arguments of `winml_probe`. */
export interface WinmlProbeArgs {}

/** Arguments of `winml_register`. */
export interface WinmlRegisterArgs {
  readonly modelId: string;
}

/** Arguments of `winml_unregister`. */
export interface WinmlUnregisterArgs {}

/** Arguments of `winml_status`. */
export interface WinmlStatusArgs {}

/** Arguments of `updates_check_all`. */
export interface UpdatesCheckAllArgs {}

/** Arguments of `updates_list`. */
export interface UpdatesListArgs {}

/** Arguments of `updates_apply`. */
export interface UpdatesApplyArgs {
  readonly componentId: string;
  readonly version?: string | null;
}

/** Arguments of `updates_pin`. */
export interface UpdatesPinArgs {
  readonly componentId: string;
  readonly version?: string | null;
}

/** Arguments of `updates_set_auto`. */
export interface UpdatesSetAutoArgs {
  readonly componentId: string;
  readonly enabled: boolean;
}

/** Arguments of `app_update_status`. */
export interface AppUpdateStatusArgs {}

/** Arguments of `app_update_check`. */
export interface AppUpdateCheckArgs {}

/** Arguments of `app_update_set_checking`. */
export interface AppUpdateSetCheckingArgs {
  readonly enabled: boolean;
}

/** Arguments of `app_update_set_auto`. */
export interface AppUpdateSetAutoArgs {
  readonly enabled: boolean;
}

/** Arguments of `app_update_download`. */
export interface AppUpdateDownloadArgs {}

/** Arguments of `app_update_restart`. */
export interface AppUpdateRestartArgs {}

/** Operation name → argument interface. */
export interface OpArgs {
  get_status: GetStatusArgs;
  get_config: GetConfigArgs;
  update_config: UpdateConfigArgs;
  settings_set_supervision: SettingsSetSupervisionArgs;
  list_backends: ListBackendsArgs;
  start_backend: StartBackendArgs;
  stop_backend: StopBackendArgs;
  restart_backend: RestartBackendArgs;
  test_backend: TestBackendArgs;
  backend_get_config: BackendGetConfigArgs;
  backend_preview_diff: BackendPreviewDiffArgs;
  backend_apply_config: BackendApplyConfigArgs;
  backend_add: BackendAddArgs;
  backend_add_preview: BackendAddPreviewArgs;
  backend_update_flags: BackendUpdateFlagsArgs;
  backend_set_offload: BackendSetOffloadArgs;
  backend_set_offload_override: BackendSetOffloadOverrideArgs;
  backend_clear_offload_override: BackendClearOffloadOverrideArgs;
  backend_set_mtp: BackendSetMtpArgs;
  backend_set_parallel: BackendSetParallelArgs;
  backend_set_cache: BackendSetCacheArgs;
  backend_set_moe_cache: BackendSetMoeCacheArgs;
  backend_cache_preview: BackendCachePreviewArgs;
  backend_set_raw_flags: BackendSetRawFlagsArgs;
  backend_set_flags_mode: BackendSetFlagsModeArgs;
  backend_delete: BackendDeleteArgs;
  backend_get_launch_command: BackendGetLaunchCommandArgs;
  run_wizard_step: RunWizardStepArgs;
  run_audit: RunAuditArgs;
  setup_get_prerequisites: SetupGetPrerequisitesArgs;
  setup_retry_step: SetupRetryStepArgs;
  setup_skip_step: SetupSkipStepArgs;
  setup_get_commands: SetupGetCommandsArgs;
  setup_mark_manual: SetupMarkManualArgs;
  setup_get_steps: SetupGetStepsArgs;
  setup_status: SetupStatusArgs;
  setup_mark_complete: SetupMarkCompleteArgs;
  setup_step_help_url: SetupStepHelpUrlArgs;
  open_url: OpenUrlArgs;
  open_folder: OpenFolderArgs;
  gateway_master_key: GatewayMasterKeyArgs;
  gateway_set_bind_host: GatewaySetBindHostArgs;
  list_models: ListModelsArgs;
  download_model: DownloadModelArgs;
  cancel_download: CancelDownloadArgs;
  model_pause_download: ModelPauseDownloadArgs;
  model_resume_download: ModelResumeDownloadArgs;
  model_verify_checksum: ModelVerifyChecksumArgs;
  model_set_short_name: ModelSetShortNameArgs;
  model_delete: ModelDeleteArgs;
  model_assign: ModelAssignArgs;
  model_add: ModelAddArgs;
  model_hf_lookup: ModelHfLookupArgs;
  model_hf_header: ModelHfHeaderArgs;
  model_set_enabled: ModelSetEnabledArgs;
  model_set_offload_spec: ModelSetOffloadSpecArgs;
  model_set_mtp: ModelSetMtpArgs;
  model_storage_status: ModelStorageStatusArgs;
  model_storage_set: ModelStorageSetArgs;
  model_storage_promote: ModelStoragePromoteArgs;
  model_storage_demote: ModelStorageDemoteArgs;
  model_storage_cancel: ModelStorageCancelArgs;
  model_set_keep_hot: ModelSetKeepHotArgs;
  get_logs: GetLogsArgs;
  get_gateway_config: GetGatewayConfigArgs;
  update_gateway_config: UpdateGatewayConfigArgs;
  gateway_status: GatewayStatusArgs;
  gateway_restart: GatewayRestartArgs;
  gateway_stop: GatewayStopArgs;
  postgres_stop: PostgresStopArgs;
  gateway_activation_plan: GatewayActivationPlanArgs;
  gateway_activation_cancel: GatewayActivationCancelArgs;
  activation_apply: ActivationApplyArgs;
  gateway_activation_complete: GatewayActivationCompleteArgs;
  gateway_activation_record_execution: GatewayActivationRecordExecutionArgs;
  gateway_activation_history: GatewayActivationHistoryArgs;
  gateway_quiesce_wait: GatewayQuiesceWaitArgs;
  gateway_preview_yaml: GatewayPreviewYamlArgs;
  gateway_test_routing: GatewayTestRoutingArgs;
  gateway_recent_requests: GatewayRecentRequestsArgs;
  gateway_set_backend_group: GatewaySetBackendGroupArgs;
  gateway_create_group: GatewayCreateGroupArgs;
  gateway_delete_group: GatewayDeleteGroupArgs;
  get_gpu_telemetry: GetGpuTelemetryArgs;
  telemetry_set_interval: TelemetrySetIntervalArgs;
  telemetry_reset_peaks: TelemetryResetPeaksArgs;
  telemetry_save_snapshot: TelemetrySaveSnapshotArgs;
  get_gpu_history: GetGpuHistoryArgs;
  placement_propose: PlacementProposeArgs;
  placement_apply: PlacementApplyArgs;
  task_abort: TaskAbortArgs;
  tasks_list: TasksListArgs;
  diagnostics_export_bundle: DiagnosticsExportBundleArgs;
  mcp_list: McpListArgs;
  mcp_start_preflight: McpStartPreflightArgs;
  mcp_test_connection: McpTestConnectionArgs;
  mcp_set_exposure: McpSetExposureArgs;
  mcp_save_server: McpSaveServerArgs;
  mcp_delete_server: McpDeleteServerArgs;
  mcp_store_credential: McpStoreCredentialArgs;
  backend_test_tool_call: BackendTestToolCallArgs;
  notifications_set_config: NotificationsSetConfigArgs;
  notifications_send_test: NotificationsSendTestArgs;
  services_list: ServicesListArgs;
  services_register: ServicesRegisterArgs;
  services_unregister: ServicesUnregisterArgs;
  log_receiver_status: LogReceiverStatusArgs;
  log_receiver_deploy: LogReceiverDeployArgs;
  log_receiver_remove: LogReceiverRemoveArgs;
  log_receiver_flip_endpoint: LogReceiverFlipEndpointArgs;
  log_receiver_start: LogReceiverStartArgs;
  log_receiver_stop: LogReceiverStopArgs;
  log_receiver_restart: LogReceiverRestartArgs;
  optimize_get_state: OptimizeGetStateArgs;
  optimize_validate_plan: OptimizeValidatePlanArgs;
  optimize_save_purpose: OptimizeSavePurposeArgs;
  optimize_set_item_state: OptimizeSetItemStateArgs;
  optimize_set_item_outcome: OptimizeSetItemOutcomeArgs;
  optimize_discard_plan: OptimizeDiscardPlanArgs;
  optimize_apply_plan: OptimizeApplyPlanArgs;
  optimize_set_experiment_state: OptimizeSetExperimentStateArgs;
  optimize_consult: OptimizeConsultArgs;
  optimize_discover: OptimizeDiscoverArgs;
  optimize_cancel_consult: OptimizeCancelConsultArgs;
  optimize_measure_backend: OptimizeMeasureBackendArgs;
  measurement_lookup: MeasurementLookupArgs;
  measurement_delete: MeasurementDeleteArgs;
  optimize_estimate_backend: OptimizeEstimateBackendArgs;
  optimize_cancel_job: OptimizeCancelJobArgs;
  optimize_run_baseline: OptimizeRunBaselineArgs;
  optimize_run_experiments: OptimizeRunExperimentsArgs;
  optimize_estimate_experiments: OptimizeEstimateExperimentsArgs;
  bench_prompts_list: BenchPromptsListArgs;
  bench_prompt_save: BenchPromptSaveArgs;
  bench_prompt_delete: BenchPromptDeleteArgs;
  bench_prompt_generate: BenchPromptGenerateArgs;
  bench_prompt_generate_cancel: BenchPromptGenerateCancelArgs;
  bench_run_start: BenchRunStartArgs;
  bench_run_cancel: BenchRunCancelArgs;
  bench_results_list: BenchResultsListArgs;
  bench_result_delete: BenchResultDeleteArgs;
  optimize_tuning_preview: OptimizeTuningPreviewArgs;
  optimize_tuning_run: OptimizeTuningRunArgs;
  optimize_tuning_adopt: OptimizeTuningAdoptArgs;
  vscode_install_extensions: VscodeInstallExtensionsArgs;
  vscode_write_config: VscodeWriteConfigArgs;
  get_mxc_policy: GetMxcPolicyArgs;
  update_mxc_policy: UpdateMxcPolicyArgs;
  mxc_set_mode: MxcSetModeArgs;
  mxc_run_self_test: MxcRunSelfTestArgs;
  mxc_get_activity_report: MxcGetActivityReportArgs;
  mxc_propose_tightening: MxcProposeTighteningArgs;
  winml_probe: WinmlProbeArgs;
  winml_register: WinmlRegisterArgs;
  winml_unregister: WinmlUnregisterArgs;
  winml_status: WinmlStatusArgs;
  updates_check_all: UpdatesCheckAllArgs;
  updates_list: UpdatesListArgs;
  updates_apply: UpdatesApplyArgs;
  updates_pin: UpdatesPinArgs;
  updates_set_auto: UpdatesSetAutoArgs;
  app_update_status: AppUpdateStatusArgs;
  app_update_check: AppUpdateCheckArgs;
  app_update_set_checking: AppUpdateSetCheckingArgs;
  app_update_set_auto: AppUpdateSetAutoArgs;
  app_update_download: AppUpdateDownloadArgs;
  app_update_restart: AppUpdateRestartArgs;
}
