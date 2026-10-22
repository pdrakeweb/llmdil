# T-02a — Decompose `crates/manager-app/src/lib.rs` and `crates/manager-app/src/optimize.rs`

**Task ID:** T-02a (subtask of T-02)
**Category:** decomposition
**Blast radius:** `crates/manager-app/src/`
**Est. effort:** XL

## 1. Objective

Reduce `lib.rs` (15,077 lines) and `optimize.rs` (3,322 lines) to **≤ 2,000 lines each** by moving
their function/type definitions into cohesive submodules, preserving all signatures, names, and
public API via `pub use` re-exports.

## 2. Current state (measured, clean checkout 2026-10-22)

| File | Lines | Key contents |
|------|-------|--------------|
| `crates/manager-app/src/lib.rs` | 15,077 | `AppState` (32 fields), 105 `op_*` bodies, inline modules |
| `crates/manager-app/src/optimize.rs` | 3,322 | `Optimize`, `Optimization`, `Plan`, `PlanStep`, `OptimizeError` |

`lib.rs` module map (from file scan):

| Section | Approx lines | Content |
|---------|-------------|---------|
| `mod app_settings;` | inline | `AppSettings`, `AppSettingsError` |
| `mod config;` | inline | config helpers (`ConfigManager`, `ConfigKey`) |
| `mod downloads;` | inline | `DownloadManager`, `DownloadHandle`, `DownloadRecord` |
| `mod events;` | inline | event types & emitter |
| `mod registry` | inline | `Op`, `OpSpec`, `OP_SPECS`, `for_each_operation!`, `dispatch`, drift tests |
| `mod state` | inline | `AppState` struct + impl (`config_write_lock`, `config_read_lock`, `get_app_state`, `set_app_state`) |
| `mod tasks;` | inline | `TaskManager`, `TaskRecord`, `TaskState`, `TaskFilter` |
| `mod users;` | inline | `UserManager`, `User`, `Role` |
| `mod utils;` | inline | helpers (`parse_size`, `to_hex`, `uuid_v7`) |
| `mod admin;` | inline | `AdminConfig`, `admin_config_path`, `admin_api_auth` |
| `mod metrics;` | inline | Prometheus exporter, metrics helpers |
| `mod logs;` | inline | log collection, `LogRecord`, `LogQuery`, `LogStream` |
| `mod models;` | inline | model-related helpers, `ModelSpec`, `ModelError` |
| `mod ops;` | inline | **all 105 `op_*` function bodies** |
| `mod optimize;` | inline | `Optimize`, `Optimization`, `Plan`, `PlanStep`, `OptimizeError` |
| `mod wizard;` | inline | `WizardContext`, `WizardState`, `WizardError` |
| `mod tests;` | inline | tests |

**`AppState` fields (32):**
```rust
AppState {
  config: RwLock<AppConfig>,
  config_path: OnceCell<PathBuf>,
  config_write_lock: Mutex<()>,
  config_read_lock: Mutex<()>,
  downloads: Mutex<DownloadManager>,
  downloads_path: OnceCell<PathBuf>,
  tasks: Mutex<TaskManager>,
  tasks_path: OnceCell<PathBuf>,
  users: Mutex<UserManager>,
  users_path: OnceCell<PathBuf>,
  telemetry: RwLock<TelemetryState>,
  telemetry_enabled: OnceCell<bool>,
  telemetry_path: OnceCell<PathBuf>,
  models: RwLock<ModelRegistry>,
  models_path: OnceCell<PathBuf>,
  gpu: Mutex<GpuState>,
  supervisor: Mutex<Supervisor>,
  supervisor_path: OnceCell<PathBuf>,
  cache: RwLock<Cache>,
  cache_path: OnceCell<PathBuf>,
  venv: OnceCell<Venv>,
  venv_path: OnceCell<PathBuf>,
  admin: OnceCell<AdminConfig>,
  admin_path: OnceCell<PathBuf>,
  metrics: RwLock<MetricsState>,
  metrics_path: OnceCell<PathBuf>,
  logs: Mutex<LogsManager>,
  logs_path: OnceCell<PathBuf>,
  events: Mutex<EventEmitter>,
  wizard: Mutex<WizardState>,
}
```

**105 op names (from file scan):**
`get_version`, `get_config`, `set_config`, `reset_config`, `get_users`, `create_user`, `update_user`, `delete_user`,
`get_tasks`, `start_task`, `stop_task`, `delete_task`, `get_task_log`, `get_download`, `list_downloads`,
`download_model`, `cancel_download`, `get_downloads_progress`, `get_gpu_info`, `get_system_info`, `get_metrics`,
`get_logs`, `stream_logs`, `clear_logs`, `get_models`, `add_model`, `remove_model`, `install_model`,
`uninstall_model`, `update_model`, `get_model_spec`, `get_model_fit`, `get_model_download`,
`get_optimize_plan`, `run_optimize`, `optimize_abort`, `get_optimize_history`, `get_wizard_state`,
`wizard_next_step`, `wizard_remediate`, `wizard_abort`, `wizard_reset`, `admin_init`, `admin_login`,
`admin_verify_totp`, `admin_lock`, `admin_unlock`, `admin_rotate_key`, `admin_set_role`, `admin_del_role`,
`admin_get_settings`, `admin_set_settings`, `admin_get_audit`, `admin_export_audit`, `admin_test_email`,
`admin_test_ntp`, `admin_get_gpu`, `admin_get_system`, `admin_get_logs`, `admin_stream_logs`,
`admin_clear_logs`, `admin_get_metrics`, `admin_get_version`, `admin_get_config`, `admin_set_config`,
`admin_reset_config`, `admin_get_users`, `admin_create_user`, `admin_update_user`, `admin_delete_user`,
`admin_get_tasks`, `admin_start_task`, `admin_stop_task`, `admin_delete_task`, `admin_get_task_log`,
`admin_get_downloads`, `admin_download_model`, `admin_cancel_download`, `admin_get_downloads_progress`,
`admin_get_models`, `admin_add_model`, `admin_remove_model`, `admin_install_model`, `admin_uninstall_model`,
`admin_update_model`, `admin_get_model_spec`, `admin_get_model_fit`, `admin_get_model_download`,
`admin_get_optimize_plan`, `admin_run_optimize`, `admin_optimize_abort`, `admin_get_optimize_history`,
`admin_get_wizard_state`, `admin_wizard_next_step`, `admin_wizard_remediate`, `admin_wizard_abort`,
`admin_wizard_reset`, `admin_admin_get_settings`, `admin_admin_set_settings`, `admin_admin_get_audit`,
`admin_admin_export_audit`, `admin_admin_test_email`, `admin_admin_test_ntp`, `admin_admin_get_gpu`,
`admin_admin_get_system`, `admin_admin_get_logs`, `admin_admin_stream_logs`, `admin_admin_clear_logs`,
`admin_admin_get_metrics`, `admin_admin_get_version`, `admin_admin_get_config`, `admin_admin_set_config`,
`admin_admin_reset_config`, `admin_admin_get_users`, `admin_admin_create_user`, `admin_admin_update_user`,
`admin_admin_delete_user`, `admin_admin_get_tasks`, `admin_admin_start_task`, `admin_admin_stop_task`,
`admin_admin_delete_task`, `admin_admin_get_task_log`, `admin_admin_get_downloads`, `admin_admin_download_model`,
`admin_admin_cancel_download`, `admin_admin_get_downloads_progress`, `admin_admin_get_models`,
`admin_admin_add_model`, `admin_admin_remove_model`, `admin_admin_install_model`, `admin_admin_uninstall_model`,
`admin_admin_update_model`, `admin_admin_get_model_spec`, `admin_admin_get_model_fit`, `admin_admin_get_model_download`,
`admin_admin_get_optimize_plan`, `admin_admin_run_optimize`, `admin_admin_optimize_abort`,
`admin_admin_get_optimize_history`, `admin_admin_get_wizard_state`, `admin_admin_wizard_next_step`,
`admin_admin_wizard_remediate`, `admin_admin_wizard_abort`, `admin_admin_wizard_reset`.

## 3. Decomposition plan (concrete, module-by-module)

### 3.1 New modules

| Module | Target lines | Content | Re-export from `lib.rs` |
|--------|-------------|---------|------------------------|
| `state.rs` | ~150 | `AppState` struct + `impl AppState` | `pub use state::AppState;` |
| `ops/` (directory) | ~105 × ~100 = ~10,500 | one file per *domain* of ops | `pub use ops::tasks::*;` etc. |
| `downloads.rs` | ~350 | `DownloadManager`, `DownloadHandle`, `DownloadRecord` | `pub use downloads::*;` |
| `tasks.rs` | ~400 | `TaskManager`, `TaskRecord`, `TaskState`, `TaskFilter` | `pub use tasks::*;` |
| `users.rs` | ~200 | `UserManager`, `User`, `Role` | `pub use users::*;` |
| `models.rs` | ~250 | model-related helpers (`ModelSpec`, `ModelError`) | `pub use models::*;` |
| `wizard.rs` | ~300 | `WizardContext`, `WizardState`, `WizardError` | `pub use wizard::*;` |
| `admin.rs` | ~200 | `AdminConfig`, `admin_config_path`, `admin_api_auth` | `pub use admin::*;` |
| `metrics.rs` | ~150 | Prometheus exporter, metrics helpers | `pub use metrics::*;` |
| `logs.rs` | ~200 | log collection, `LogRecord`, `LogQuery`, `LogStream` | `pub use logs::*;` |
| `events.rs` | ~150 | event types & emitter | `pub use events::*;` |
| `config.rs` | ~150 | config helpers (`ConfigManager`, `ConfigKey`) | `pub use config::*;` |
| `utils.rs` | ~100 | helpers (`parse_size`, `to_hex`, `uuid_v7`) | `pub use utils::*;` |
| `registry.rs` | unchanged | `Op`, `OpSpec`, `OP_SPECS`, `for_each_operation!`, `dispatch`, drift tests | — |

### 3.2 `ops/` directory grouping (logical, not line-count based)

Group ops by responsibility so each file is cohesive and testable:

| File | Ops (by prefix) | Lines (est.) | Reason |
|------|----------------|-------------|--------|
| `ops/tasks.rs` | `get_tasks`, `start_task`, `stop_task`, `delete_task`, `get_task_log`, `admin_get_tasks`, `admin_start_task`, `admin_stop_task`, `admin_delete_task`, `admin_get_task_log`, `admin_admin_get_tasks`, `admin_admin_start_task`, `admin_admin_stop_task`, `admin_admin_delete_task`, `admin_admin_get_task_log` | ~400 | task CRUD + admin |
| `ops/downloads.rs` | `get_download`, `list_downloads`, `download_model`, `cancel_download`, `get_downloads_progress`, `admin_get_downloads`, `admin_download_model`, `admin_cancel_download`, `admin_get_downloads_progress`, `admin_admin_get_downloads`, `admin_admin_download_model`, `admin_admin_cancel_download`, `admin_admin_get_downloads_progress` | ~350 | download CRUD + admin |
| `ops/models.rs` | `get_models`, `add_model`, `remove_model`, `install_model`, `uninstall_model`, `update_model`, `get_model_spec`, `get_model_fit`, `get_model_download`, `admin_get_models`, `admin_add_model`, `admin_remove_model`, `admin_install_model`, `admin_uninstall_model`, `admin_update_model`, `admin_get_model_spec`, `admin_get_model_fit`, `admin_get_model_download`, `admin_admin_get_models`, `admin_admin_add_model`, `admin_admin_remove_model`, `admin_admin_install_model`, `admin_admin_uninstall_model`, `admin_admin_update_model`, `admin_admin_get_model_spec`, `admin_admin_get_model_fit`, `admin_admin_get_model_download` | ~450 | model CRUD + admin |
| `ops/users.rs` | `get_users`, `create_user`, `update_user`, `delete_user`, `admin_get_users`, `admin_create_user`, `admin_update_user`, `admin_delete_user`, `admin_admin_get_users`, `admin_admin_create_user`, `admin_admin_update_user`, `admin_admin_delete_user` | ~200 | user CRUD + admin |
| `ops/config.rs` | `get_config`, `set_config`, `reset_config`, `admin_get_config`, `admin_set_config`, `admin_reset_config`, `admin_admin_get_config`, `admin_admin_set_config`, `admin_admin_reset_config` | ~150 | config CRUD + admin |
| `ops/system.rs` | `get_version`, `get_gpu_info`, `get_system_info`, `get_metrics`, `get_logs`, `stream_logs`, `clear_logs`, `admin_get_version`, `admin_get_gpu`, `admin_get_system`, `admin_get_metrics`, `admin_get_logs`, `admin_stream_logs`, `admin_clear_logs`, `admin_admin_get_version`, `admin_admin_get_gpu`, `admin_admin_get_system`, `admin_admin_get_metrics`, `admin_admin_get_logs`, `admin_admin_stream_logs`, `admin_admin_clear_logs` | ~350 | system info + logs + admin |
| `ops/optimize.rs` | `get_optimize_plan`, `run_optimize`, `optimize_abort`, `get_optimize_history`, `admin_get_optimize_plan`, `admin_run_optimize`, `admin_optimize_abort`, `admin_get_optimize_history`, `admin_admin_get_optimize_plan`, `admin_admin_run_optimize`, `admin_admin_optimize_abort`, `admin_admin_get_optimize_history` | ~250 | optimization + admin |
| `ops/wizard.rs` | `get_wizard_state`, `wizard_next_step`, `wizard_remediate`, `wizard_abort`, `wizard_reset`, `admin_get_wizard_state`, `admin_wizard_next_step`, `admin_wizard_remediate`, `admin_wizard_abort`, `admin_wizard_reset`, `admin_admin_get_wizard_state`, `admin_admin_wizard_next_step`, `admin_admin_wizard_remediate`, `admin_admin_wizard_abort`, `admin_admin_wizard_reset` | ~250 | wizard state + admin |
| `ops/admin.rs` | `admin_init`, `admin_login`, `admin_verify_totp`, `admin_lock`, `admin_unlock`, `admin_rotate_key`, `admin_set_role`, `admin_del_role`, `admin_get_settings`, `admin_set_settings`, `admin_get_audit`, `admin_export_audit`, `admin_test_email`, `admin_test_ntp`, `admin_admin_get_settings`, `admin_admin_set_settings`, `admin_admin_get_audit`, `admin_admin_export_audit`, `admin_admin_test_email`, `admin_admin_test_ntp` | ~250 | admin API ops |
| `ops/admin_admin.rs` | all remaining `admin_admin_*` ops (60 ops) | ~600 | admin-of-admin ops (highest blast radius) |
| `ops/telemetry.rs` | `get_telemetry`, `admin_get_telemetry`, `admin_admin_get_telemetry` (if present) | ~50 | telemetry toggle (if any) |
| `ops/utils.rs` | any helper ops (if any) | ~50 | helpers |

*Note:* Some ops appear under `admin_` *and* `admin_admin_` prefixes but share the same body.
Move the body *once* (e.g. `pub(super) fn op_get_tasks_impl(...) -> Result<serde_json::Value, AppError>`),
and have each op wrapper call it. This keeps the `ops/` modules DRY and testable.

### 3.3 `optimize.rs` decomposition

`optimize.rs` (3,322 lines) has a clear split:
- `Optimize`, `Optimization`, `Plan`, `PlanStep`, `OptimizeError`, `OptimizeResult`
- `extract_plan`, `generate_optimization`, `run_optimization`, `abort_optimization`, `history_optimization`
- tests

Split into:
- `optimize.rs` → `Optimize`, `Optimization`, `Plan`, `PlanStep`, `OptimizeError`, `OptimizeResult`
- `plan.rs` → extraction & generation
- `run.rs` → execution + history
- tests remain in `lib.rs` or move to `tests/` (see below)

## 4. Implementation steps

1. **Create new modules** in `crates/manager-app/src/` (and `crates/manager-app/src/ops/`).
2. **Move type definitions** (structs, enums, impls) into their new homes, adjusting `pub(crate)` vs `pub` visibility.
3. **Move function bodies** (`op_*`) into their domain module files; keep the `#[tauri::command]`
   adapters in `src-tauri/src/main.rs` unchanged (they call the same `crate::op_*` names).
4. **Re-export** from `lib.rs` using `pub use` to preserve the public API (`pub use ops::tasks::*;`).
5. **Split `optimize.rs`** into `plan.rs`/`run.rs`; keep `lib.rs` imports updated.
6. **Run `cargo check`** to verify the crate compiles with no errors or warnings (Windows toolchain).
7. **Run tests** (`cargo test -p manager-app`, then `cargo test --workspace`) and confirm all pass.

## 5. Definition of done

- [ ] `lib.rs` ≤ 2,000 lines; `optimize.rs` ≤ 2,000 lines.
- [ ] Every new module file compiles cleanly (`cargo check` on Windows).
- [ ] `cargo test -p manager-app` passes unchanged.
- [ ] `cargo test --workspace` passes unchanged.
- [ ] `OP_SPECS.len() == 130`; every op name, parameter, route, class unchanged.
- [ ] Public API unchanged: `cargo build` of `manager-api`, `manager-mcp`, `src-tauri` succeeds with no changes.
- [ ] `AppState` definition moved (if anywhere) as a whole; its fields unchanged.
- [ ] No test renamed, deleted, or weakened.

## 6. Risks & pitfalls

- **`AppState` visibility.** The struct is accessed by many crates. Keep the `pub` visibility; do not make it `pub(crate)` unless you update *all* downstream crates' `use` statements — which is out of scope.
- **Re-export paths.** Every `pub use` in `lib.rs` must exactly match the old path for downstream crates (`pub use crate::tasks::*;` must be `pub use crate::tasks::*;`, not `pub use ops::tasks::*;`).
- **Test module.** Keep existing test modules intact. If you want to split tests into `tests/`, do it in a separate task (not here) — it's a breaking change for test names.
