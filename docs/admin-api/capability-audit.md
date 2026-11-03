# Capability audit — LLMdil

Status: pre-implementation audit snapshot, 2026-10-16 — the design
input for the Admin API, retained as a record. The codebase it
describes has since been restructured: operations were extracted
from `src-tauri/src/main.rs` into `manager-app` and are declared
once in the operation registry (`crates/manager-app/src/registry.rs`,
108 operations at v0.1.40), which is the current inventory; the
as-built REST surface is `rest-api.md` and the as-built record is
`PLAN.md` §9. Locations and counts below are as of the audit date.

Purpose: the complete inventory of capabilities the app exposes today, as the
input to designing an admin REST API (served from the managed machine on an
admin port) that a future Android client will use for read and control.

Method: the Tauri command registrations in `src-tauri/src/main.rs`
(`tauri::generate_handler!`, line 5882 — this crate has no `lib.rs`; `main.rs`
is the shell), their implementations in the same file plus
`src-tauri/src/activation.rs` and `src-tauri/src/gateway_stats.rs`, the UI
command surface in `ui/src/api.ts`, and the workspace crates under `crates/`
(`manager-config`, `manager-models`, `manager-telemetry`, `manager-gateway`,
`manager-wizard`, `manager-supervisor`, `manager-mxc`, `manager-mcp`,
`manager-winml`, `manager-present`).

Conventions used below:

- **Entry point** names the Tauri command and its Rust function location.
  "UI: yes/no" says whether the current UI invokes the command at all
  (checked against every `invoke(...)` call in `ui/src`).
- **Inputs / outputs** are the wire shapes. Every command returns
  `ApiResult = Result<serde_json::Value, String>` — untyped JSON out, and
  errors are plain strings (an `ApiError` kind rendered as text). There is
  no schema or status-code taxonomy today; the REST design will need both.
- **Class**: `read` (no state change), `mutate` (config/state/file change,
  no process lifecycle), `lifecycle` (starts/stops/restarts a process or a
  background operation), `long-running` (blocks for seconds-to-minutes, or
  launches background work whose completion is observed indirectly).
- Side effects call out processes, files, network, and elevation (UAC).

Total registered commands: **82**. The UI invokes **57** of them;
**25 are registered but never invoked by the current UI** (flagged per entry
and listed again in §13).

---

## 1. Status and configuration

### `get_status` — read
- What: Overall service status snapshot.
- Entry: `get_status`, main.rs:922. UI: yes (`getStatus`).
- In: none. Out: `{status, version, backends_configured, gateway_port, winml_enabled, mxc_mode}`.
- Side effects: none (in-memory config read).

### `get_config` — read
- What: The full application configuration as JSON.
- Entry: `get_config`, main.rs:937. UI: yes (`getAppConfig`).
- In: none. Out: the serialized `AppConfig` (version, data_dir, view_modes, backends, gateway, models, telemetry, mxc, winml, updates, notifications) plus `config_path`.
- Side effects: none. Note: this is the whole config document in one payload — the REST equivalent needs field-level scoping thought, not a blind mirror.

### `update_config` — mutate
- What: Merge an arbitrary JSON patch into the config, validate, persist.
- Entry: `update_config`, main.rs:955. UI: **no** — the Settings view reads config but writes go through narrower commands.
- In: `patch: JSON object` (deep-merged). Out: `{ok: true, persisted: true}`.
- Side effects: writes `config.json` (atomic save with `.bak` rotation via `manager_config::save`); runs catalog seeding/repair first. This is the broadest write surface in the app — a single endpoint that can change any config field, including gateway bind and ports.

## 2. Backends

### `list_backends` — read
- What: Configured backends merged with live supervisor state.
- Entry: `list_backends`, main.rs:1477. UI: yes (`getBackends`, `listBackends`).
- In: none. Out: `{backends: [{id, enabled, configured, port, model_file, state (stopped|starting|running|stopping|failed), pid, adopted, restarts, uptime_secs, requests_1h?, errors_1h?, throughput_toks?, n_ctx?, n_batch?, tensor_split?, group?, gpu_ram_mib?, sys_ram_mib?, kv_ram_mib?, rs_ram_mib?, failure_reason?, memory_basis?, offload?}]}`. Rows with `configured: false` are supervisor-only orphan processes. `rs_ram_mib` is the recurrent-state cache (hybrid SSM models) at the backend's effective slot count; `failure_reason` is the supervisor's recorded reason for the last spawn/load failure, null when none was recorded. Trailing-hour stats come from the gateway metrics store (see §13.2); memory figures derive from the model's GGUF layout (`manager-models` gguf/memory modules).
- Side effects: none directly; reads the supervisor and telemetry stores.

### `start_backend` — lifecycle
- What: Start one backend's llama-server under the supervisor.
- Entry: `start_backend`, main.rs:1634. UI: yes.
- In: `id: string`. Out: `{task_id, backend_id}`.
- Side effects: spawns a process (wizard-installed llama-server binary; argv built by `backend_spec`, main.rs:1104, including `CUDA_VISIBLE_DEVICES` restricted to the placed GPUs' UUIDs and remapped `--tensor-split`/`--main-gpu`/`--device`); runs a start preflight (refuses while the model file's checksum verification is in flight); emits the `backend-status` event; records a task.

### `stop_backend` — lifecycle
- What: Stop one backend (graceful drain, then kill).
- Entry: `stop_backend`, main.rs:1671. UI: yes.
- In: `id: string, graceful: bool` (UI always sends `graceful: true`). Out: `{task_id, backend_id}`.
- Side effects: terminates the supervised process (supervisor drain, 30 s default, then kill); adopted processes are stopped by pid tracking; emits `backend-status`; records a task.

### `restart_backend` — lifecycle
- What: Stop + start one backend under the supervisor.
- Entry: `restart_backend`, main.rs:1717. UI: yes.
- In: `id: string, graceful: bool`. Out: `{task_id, backend_id}`.
- Side effects: as start + stop; runs the start preflight first.

### `test_backend` — read (active probe)
- What: One inference request against a running backend.
- Entry: `test_backend`, main.rs:1766. UI: yes, **but under the wrong name** — `ui/src/api.ts` invokes `"backend_test_request"` while the registered command is `test_backend`; the invoke cannot resolve and the wrapper degrades to an error result (latent defect found by this audit).
- In: `id: string, prompt: string`. Out: `TestResult {status, content, prompt_tokens, completion_tokens, latency_ms}` (a non-2xx HTTP status is reported in the payload, not thrown).
- Side effects: network — POSTs to the backend's `http://127.0.0.1:<port>/v1/chat/completions`; consumes inference on that backend. No state change.

### `backend_get_config` — read
- What: One backend's stored configuration.
- Entry: `backend_get_config`, main.rs:3125. UI: yes.
- In: `id: string`. Out: the serialized `BackendConfig {id, enabled, model_file, port, flags, raw_flags, restart_policy, overrides_global}`.
- Side effects: none.

### `backend_preview_diff` — read
- What: Preview the field-level diff a config patch would make to a backend.
- Entry: `backend_preview_diff`, main.rs:3137. UI: **no**.
- In: `id: string, patch: BackendPatch` (all-optional fields; `raw_flags` is double-optional: untouched vs clear). Out: `ConfigDiff {backend_id, changes: [{field, old, new}], valid}`.
- Side effects: none (pure computation, `manager_config::diff_config`).

### `backend_apply_config` — mutate
- What: Apply a config patch to a backend, with restart semantics.
- Entry: `backend_apply_config`, main.rs:3158. UI: yes (`applyBackendConfig`).
- In: `id: string, patch: BackendPatch, apply_mode: "RestartNow" | "OnNextRestart"`. Out: `{backend_id, diff, apply_mode, restart_task_id: string|null, persisted: true}`.
- Side effects: validates and persists the whole config (`config.json` write); writes activation-journal entries (a gateway-scope entry and, when fields changed, a backend-scope entry — see §7); `RestartNow` only *records* a restart task — the actual restart is a separate `restart_backend` call the client must make.

### `backend_add` — mutate
- What: Create a backend for a registered model and route it through a gateway group.
- Entry: `backend_add`, main.rs:3234. UI: yes.
- In: `id: string` (slug-validated), `model_id: string, port: u16, group: string, create_group: bool, enabled: bool`. Out: `{backend_id, group}`.
- Side effects: persists config (backend created with seeded defaults: 32K context, tensor split assigned from live telemetry when available, standard restart policy); creates the gateway group when `create_group` is set; the gateway routes the backend only after its next restart (generated config).

### `backend_update_flags` — mutate
- What: Change a backend's context length and batch size.
- Entry: `backend_update_flags`, main.rs:3433. UI: yes.
- In: `id: string, n_ctx: u32, n_batch: u32`. Out: `{backend_id, persisted}`.
- Side effects: persists config; journals an activation-plan entry; applies on the backend's next start (no restart is performed).

### `backend_set_offload` — mutate
- What: Set a MoE backend's expert-offload layer count (the Edit Flags slider).
- Entry: `backend_set_offload`, main.rs:3508. UI: yes.
- In: `id: string, layers: u32` (0 = no offload; the model's full MoE layer count = the `-ot exps=CPU` form). Out: `{backend_id, layers, raw_flags, changed}`.
- Side effects: rewrites only the offload tokens in the backend's `raw_flags`; persists config; journals a backend-scoped activation entry; applies on next start.

### `backend_delete` — mutate
- What: Remove a backend and its gateway group memberships.
- Entry: `backend_delete`, main.rs:3593. UI: yes.
- In: `id: string`. Out: `{backend_id, deleted}`.
- Side effects: persists config; refused while the backend is running.

### `backend_get_launch_command` — read
- What: The exact argv the supervisor will run for a backend.
- Entry: `backend_get_launch_command`, main.rs:3644. UI: yes.
- In: `id: string`. Out: `{id, argv: string[]}`.
- Side effects: none (same `backend_spec` construction the supervisor uses).

## 3. Setup wizard and audit

The wizard is a step state machine (`manager-wizard`): 12 steps —
`detect-hardware`, `install-llamacpp`, `configure-tensor-split`,
`download-models`, `install-postgres`, `start-gateway`, `vscode-extensions`,
`mcp-servers`, `winml-register`, `install-mxc`, `mxc-policies`, `smoke-test`.
Steps run in Install or Audit mode (audit re-runs check-type steps without
reinstalling). One wizard lock serializes step execution; concurrent runs
are rejected with a busy report rather than queued. Step state persists to
disk and is rehydrated at app launch (a background audit adopts the
machine's actual state into the checklist).

### `run_wizard_step` — long-running
- What: Run one wizard step to completion.
- Entry: `run_wizard_step`, main.rs:1875. UI: yes.
- In: `step_id: string`. Out: `{task_id, busy: false, outcome: {state (queued|running|done|failed|skipped|manual), message, log_tail: string[], duration_ms, manual_command?: string}}`, or `{task_id, busy: true, running_step?, elapsed_secs?, outcome: null}` when another step holds the lock.
- Side effects: step-dependent and the heaviest in the app — installs software (llama.cpp, PostgreSQL), downloads models, writes files, reconciles the Windows firewall rule (**one UAC elevation prompt** on the server console when the rule state changes; a decline is a warning, not a failure), starts/stops the gateway and PostgreSQL, generates/stores credentials. `start-gateway` alone: enforces the `prisma==0.11.0` pin (deletes and reinstalls the prisma package in the gateway venv), runs `prisma generate`, spawns the LiteLLM gateway with `PRISMA_HEALTH_WATCHDOG_ENABLED=false`, and confirms sustained health (~10 s). Per-step timeout budgets run to 600 s. Emits `wizard-progress`; appends to `<data_dir>/logs/wizard.log`; reloads config from disk afterward.

### `run_audit` — long-running
- What: Re-run all check-type wizard steps in audit mode.
- Entry: `run_audit`, main.rs:1982. UI: yes.
- In: none. Out: `{task_id, done, total, steps: [step states], already_running: bool}` (`already_running` when an audit — e.g. the launch-time hydration audit — is in flight; a report, not an error).
- Side effects: probes the machine (binaries, GPU inventory, HTTP probes); does not install; emits `wizard-progress`.

### `setup_get_prerequisites` — read
- What: Per-step prerequisite lists (which step ids must be Done or Manual first).
- Entry: `setup_get_prerequisites`, main.rs:2876. UI: **no**.
- In: none. Out: `{steps: [{id, name, prerequisites: string[]}]}`.
- Side effects: none.

### `setup_retry_step` — mutate
- What: Reset a failed step to Queued so it can run again.
- Entry: `setup_retry_step`, main.rs:2893. UI: **no** (the wizard UI re-runs steps via `run_wizard_step`).
- In: `step_id: string`. Out: `{ok: true, step_id}`.
- Side effects: mutates wizard state; persists it to disk.

### `setup_skip_step` — mutate
- What: Skip a step, recording a note.
- Entry: `setup_skip_step`, main.rs:2902. UI: **no**.
- In: `step_id: string, note: string`. Out: `{ok: true, step_id}`.
- Side effects: mutates and persists wizard state.

### `setup_get_commands` — read
- What: The exact commands a step will run (the failed-step "run it manually" data).
- Entry: `setup_get_commands`, main.rs:2912. UI: yes.
- In: `step_id: string`. Out: `[{argv: string[], cwd, env, stdout_log, stderr_log}]` (`PlannedCommand`s; contractually equal to the argv the step actually spawns).
- Side effects: none.

### `setup_mark_manual` — mutate
- What: Mark a step as completed manually by the operator, with the path and version they installed.
- Entry: `setup_mark_manual`, main.rs:3107. UI: **no**.
- In: `step_id: string, path: string, version: string`. Out: `{ok: true, step_id}`.
- Side effects: mutates and persists wizard state.

### `setup_get_steps` — read
- What: Live wizard step states for the checklist.
- Entry: `setup_get_steps`, main.rs:2956. UI: yes.
- In: none. Out: `{steps: [{id, name, state, message, duration_ms, log_tail, note, manual_command, remediation, help_url}], done, total}`; while a step executes, the last-known snapshot is served with `busy: true` instead of blocking.
- Side effects: none.

### `setup_status` — read
- What: Whether the setup wizard has been completed.
- Entry: `setup_status`, main.rs:2983. UI: yes.
- In: none. Out: `{setup_completed: bool}`.
- Side effects: none.

### `setup_mark_complete` — mutate
- What: Mark the setup wizard complete (persisted).
- Entry: `setup_mark_complete`, main.rs:2995. UI: yes.
- In: none. Out: `{ok, persisted}`.
- Side effects: sets `setup_completed` in config and saves `config.json`.

### `setup_step_help_url` — read
- What: The help URL a step offers (e.g. a driver download page).
- Entry: `setup_step_help_url`, main.rs:3010. UI: yes.
- In: `step_id: string`. Out: `{url: string|null, step_id}`.
- Side effects: none.

## 4. Desktop shell actions

### `open_url` — mutate (desktop side effect)
- What: Open an http/https URL in the server machine's default browser.
- Entry: `open_url`, main.rs:3019. UI: yes.
- In: `url: string` (http/https only). Out: `{ok: true}`.
- Side effects: spawns `cmd /C start` on the server's interactive desktop (Windows only; Unsupported elsewhere). No app state change. **No meaningful remote equivalent** — it acts on the server's desktop session, not the client's.

### `open_folder` — mutate (desktop side effect)
- What: Open a folder (or a file's containing folder) in Windows Explorer on the server.
- Entry: `open_folder`, main.rs:3066. UI: yes (after diagnostics export).
- In: `path: string` (must exist). Out: `{ok: true}`.
- Side effects: spawns `explorer.exe` on the server's desktop. Same remote-equivalence problem as `open_url`.

## 5. Models

### `list_models` — read
- What: Configured models with on-disk and download state.
- Entry: `list_models`, main.rs:2085. UI: yes.
- In: none. Out: `{models: [{id, name (short display name), gguf_path, quant, params_b, sha256, verified_at, enabled, on_disk, size_bytes, downloading, verifying, paused, partial_bytes, download_error, catalog_size_bytes, ctx_max, fit_check_available}]}`. `fit_check_available` is hardcoded `false` in v0.1.x (see §13). `download_error` carries the last terminal failure this session, including hash mismatches (which also remove the downloaded file).
- Side effects: filesystem stats of model files and `.part` partials; no writes.

### `download_model` — long-running
- What: Start a model download in a background worker (HTTP range resume from any `.part` file).
- Entry: `download_model`, main.rs:2200. UI: yes.
- In: `id: string`. Out: `{model_id, dest}` — returns immediately; the transfer continues in the background.
- Side effects: network download of multi-GiB files into the data dir; emits `download-progress` events; on completion a backend watcher verifies the file against the pinned SHA-256 (when a real pin exists), stamps `verified_at` in config (a config write), and deletes the file on mismatch.

### `cancel_download` — lifecycle
- What: Abort an in-flight download; the partial file is discarded.
- Entry: `cancel_download`, main.rs:2395. UI: yes.
- In: `id: string`. Out: `{model_id, cancelled}`. Errors with "no download in flight" when already finished.
- Side effects: terminates the download worker; deletes the `.part` file.

### `model_pause_download` — lifecycle
- What: Pause an in-flight download; the worker parks and the partial file is kept for resume.
- Entry: `model_pause_download`, main.rs:4177. UI: yes.
- In: `id: string`. Out: `{model_id, paused}`.
- Side effects: parks the worker; no file change.

### `model_resume_download` — lifecycle
- What: Resume a paused download from its partial file.
- Entry: `model_resume_download`, main.rs:4190. UI: yes.
- In: `id: string`. Out: `{model_id, resumed}`.
- Side effects: network transfer resumes.

### `model_verify_checksum` — long-running
- What: SHA-256 the on-disk file against the model's pinned hash.
- Entry: `model_verify_checksum`, main.rs:4212. UI: yes.
- In: `id: string`. Out: `{id, ok}`. Errors when no real hash is pinned, and when a verification is already in progress for the model.
- Side effects: reads the full file (minutes for multi-GiB models; the command blocks); on completion stamps or clears `verified_at` and saves config. While it runs, `list_models` reports the model as `verifying` and backend start preflight refuses to launch on that file.

### `model_set_short_name` — mutate
- What: Set a model's operator-chosen short display name (persisted; the model id never changes).
- Entry: `model_set_short_name`, main.rs:4291. UI: yes.
- In: `id: string, name: string`. Out: `{id, name}`.
- Side effects: persists config (`ModelConfig.display_name`).

### `model_delete` — mutate
- What: Delete a model's file (and any partial) and its config entry.
- Entry: `model_delete`, main.rs:4312. UI: yes.
- In: `id: string`. Out: `{model_id, deleted, bytes_freed}`.
- Side effects: deletes files from disk; persists config. Refused while a download is in flight for the model or while any backend references it (`manager_models::delete_model_file` refuses with `InUse` when a backend's `model_file` or `assigned_backends` point at the model; a missing file is not an error).

### `model_add` — mutate
- What: Register a new model by id and GGUF source URL.
- Entry: `model_add`, main.rs:4371. UI: yes.
- In: `id: string, source_url: string, file_name?: string` (defaults to the URL's last segment)`, quant?: string, params_b?: number`. Out: `{model_id}`.
- Side effects: persists config; the model starts enabled with no pinned hash.

### `model_set_enabled` — mutate
- What: Include/exclude a model from the wizard's download step.
- Entry: `model_set_enabled`, main.rs:4352. UI: yes.
- In: `id: string, enabled: bool`. Out: `{model_id, enabled}`.
- Side effects: persists config; file and configuration are kept either way.

### `model_assign` — mutate
- What: Assign a model to a backend: sets the backend's `model_file` to the model's gguf path and applies a tensor split.
- Entry: `model_assign`, main.rs:4438. UI: yes.
- In: `id: string` (model)`, backend_id: string, split: number[]` (validated against the discovered GPU count). Out: `{task_id, persisted}`.
- Side effects: persists config; journals a backend-scoped activation entry (takes effect at the backend's next (re)start).

## 6. Logs

### `get_logs` — read
- What: Query captured logs across scopes with filters and paging.
- Entry: `get_logs`, main.rs:2578. UI: yes.
- In: `scope: string` (a process id, `"gateway"`, `"wizard"`, `"all"`, or an archived scope id from the payload's scope list)`, level?: "error"|"warn"|"info"|"debug"` (heuristic line match)`, search?: string` (substring over line text)`, query?: string` (substring over line text + source)`, since_ms?: i64, until_ms?: i64` (inclusive)`, order?: "asc"|"desc"` (default desc)`, offset?: u64, limit?: u64` (default 200).
- Out: `{lines: [{ts, stream, line, source, archived}], total_matching, total, truncated, scopes: [{id, previous}]}`.
- Side effects: reads supervisor ring buffers (live and retained previous-run rings) plus the file-backed gateway and wizard logs under the data dir. Read-only, but the only log access is this paged pull — there is no live tail channel (see §14).


## 7. Gateway and the activation plan

The gateway is the LiteLLM proxy. It is **not** supervisor-managed: the
wizard's `start-gateway` step spawns it and records its pid in
`<data_dir>/gateway.pid`; the app-managed PostgreSQL server (port 54329)
exists to serve its database. Saved configuration changes that are not
live yet are tracked by the **activation journal**
(`src-tauri/src/activation.rs`): mutations record entries with a scope
(`gateway` or `backend`) and targets; entries resolve when the relevant
process (re)starts after the change, by comparing process start times;
a pre-change config snapshot supports cancel.

### `get_gateway_config` — read
- What: The gateway section of the config, with derived per-group enablement.
- Entry: `get_gateway_config`, main.rs:2676. UI: yes.
- In: none. Out: `GatewayConfig {port, bind_host, groups: [{name, members, strategy, fallbacks, enabled, enabled_members}], routing_rules: [{id, description, source, litellm_fragment}], request_log_len, openrouter: {enabled, cred_ref, daily_cap_usd, model_allowlist} | null}`. `enabled`/`enabled_members` are derived by `manager-gateway` (a group is enabled iff ≥1 member backend is enabled); never stored.
- Side effects: none.

### `update_gateway_config` — mutate
- What: Merge a JSON patch into the gateway configuration and persist.
- Entry: `update_gateway_config`, main.rs:2697. UI: yes.
- In: `patch: JSON object`. Out: `{ok, persisted}` (`persisted` is true only after the file save succeeds; a failed save leaves memory unchanged).
- Side effects: writes `config.json`; journals a gateway-scope activation entry (the change is not live until the gateway restarts).

### `gateway_status` — read (active probe)
- What: Gateway liveness and summary state.
- Entry: `gateway_status`, main.rs:3672. UI: yes.
- In: none. Out: `{port, bind_host, lan_ip, groups (configured count), routing_rules (count), up, routing_log_len, openrouter_cred_present: bool|null, activation_pending, activation_changes}`.
- Side effects: network probe of the gateway (`GET /v1/models` on the probe host, ~1 s); a `cmdkey` subprocess presence check for the OpenRouter credential (presence only, value never read); reads the activation journal. Note: `groups` counts *configured* groups, while the generated config includes only *enabled* groups — the two numbers can differ.

### `gateway_stop` — lifecycle
- What: Stop the running gateway (and the app-managed PostgreSQL after it).
- Entry: `gateway_stop`, main.rs:3926. UI: yes.
- In: none. Out: `{ok: true, stopped: bool, postgres_stopped: bool}` (`stopped: false` with `ok: true` means it was not running).
- Side effects: terminates the process recorded in `gateway.pid`; stops the app-managed PostgreSQL server (best-effort; its failure is reported in the payload, not as a command error). There is no `gateway_start` command — starting is the wizard's `start-gateway` step (§3).

### `gateway_master_key` — read (with a first-call write)
- What: The LiteLLM master key — the API key local clients present and the admin UI password.
- Entry: `gateway_master_key`, main.rs:3054. UI: yes.
- In: none. Out: `{master_key: string}` — **the secret itself, in plaintext**.
- Side effects: reads Windows Credential Manager; on first use **generates the key and stores it** (`local-llm-service-manager/litellm-master-key`). A REST equivalent is a credential-disclosure endpoint and needs the highest privilege tier plus audit logging.

### `gateway_set_bind_host` — mutate
- What: Set the gateway bind address (e.g. `0.0.0.0` vs `127.0.0.1`).
- Entry: `gateway_set_bind_host`, main.rs:4011. UI: yes.
- In: `host: string` (must parse as an IP address). Out: `{bind_host, restart_required: true}` (always true).
- Side effects: validates and persists config; journals a gateway-scope activation entry; the firewall rule is reconciled at the next gateway start (by the start-gateway step, with elevation — §3).

### `gateway_preview_yaml` — read
- What: The generated LiteLLM `config.yaml` for the current config.
- Entry: `gateway_preview_yaml`, main.rs:4121. UI: yes.
- In: none. Out: `{yaml: string}` (`manager_gateway::generate_config_yaml`; only enabled groups are emitted; errors when no group is enabled).
- Side effects: none.

### `gateway_test_routing` — mutate (records a trace)
- What: Dry-run routing explainer for a model group.
- Entry: `gateway_test_routing`, main.rs:4137. UI: **no**.
- In: `request: {group: string}`. Out: `RoutingDecision {at, group, backend, strategy, reason}`.
- Side effects: appends the decision to the in-memory routing log (session-only ring, cap 50). Computation only — every member is assumed healthy; live traffic is routed by LiteLLM, not by this code.

### `gateway_recent_requests` — read
- What: The recorded routing-test traces, newest first.
- Entry: `gateway_recent_requests`, main.rs:4166. UI: **no**.
- In: none. Out: `{requests: [RoutingDecision]}`.
- Side effects: none. Despite the name, these are dry-run traces from `gateway_test_routing`, not live requests.

### `gateway_activation_plan` — read
- What: Every saved change that is not live yet, across scopes, with the state the apply decision depends on.
- Entry: `gateway_activation_plan`, main.rs:3751. UI: yes.
- In: none. Out: `{pending, gateway_up, gateway_pending, drift, applies_at_next_start, actionable, backend_restart_ids: string[], changes: [{ts, kind, summary, scope, targets, activation ("gateway_restart"|"restart"|"next_start")}], snapshot_available}`. `drift` reports saved-config-vs-running divergence detected outside the journal.
- Side effects: reads the journal, supervisor start times, and config. Known defect (code review, 2026-10-16): journal prune/cancel paths do not all use the scoped resolution this command uses, so entries can outlive their resolution — the REST design should treat the plan computation as the single source of truth.

### `gateway_activation_cancel` — mutate
- What: Cancel all pending changes: restore the pre-change config snapshot and clear the journal.
- Entry: `gateway_activation_cancel`, main.rs:3835. UI: yes.
- In: none. Out: `{cancelled: true}`, or `{cancelled: false, reason}` when there is nothing pending; an error when the config has since changed through another path (the snapshot no longer matches).
- Side effects: overwrites `config.json` with the snapshot; clears the journal file; re-derives backend specs.

### `gateway_activation_complete` — mutate
- What: Mark the gateway side of the plan complete after a successful gateway (re)start.
- Entry: `gateway_activation_complete`, main.rs:3864. UI: yes (as the last step of the apply composition, §14).
- In: none. Out: `{cleared: true}`.
- Side effects: clears gateway-scope journal entries. Backend-scope entries are *not* cleared here — they resolve through their backends' restarts.

### `gateway_quiesce_wait` — long-running
- What: Wait until the gateway's in-flight request count drains to zero (the "Drain Requests and Apply" pre-step).
- Entry: `gateway_quiesce_wait`, main.rs:3876. UI: yes.
- In: `timeout_secs: u64` (clamped to 5–600). Out: `{drained, in_flight, timed_out}`.
- Side effects: polls the gateway's Prometheus metrics (master key as Bearer) every 2 s until drain or timeout — the call blocks for up to the full timeout. A gateway that does not answer metrics counts as drained.

## 8. Telemetry and placement

Telemetry is a polled store (`manager-telemetry`): NVML primary source,
`nvidia-smi` fallback, plus a Windows PDH fixup for per-process VRAM on
WDDM devices (`crates/manager-telemetry/src/pdh.rs`). Samples go stale
after 10 s without a successful poll; last-known values are kept and
flagged, never synthesized. Code review note (2026-10-16): the PDH
module's locally declared `DxgiAdapterDesc1` struct is 8 bytes too small
(missing trailing `Flags: u32`) and DXGI `GetDesc1` overflows it on
every WDDM poll — a known Blocker in the current build, listed here
because any API consumer of per-process VRAM depends on this path.

### `get_gpu_telemetry` — read
- What: Current GPU telemetry snapshot with per-process VRAM attribution.
- Entry: `get_gpu_telemetry`, main.rs:1800. UI: yes.
- In: none. Out: `{samples: [{index, name, ts, utilization_pct, vram_used_mib, vram_total_mib, temp_c, power_w, power_absence, stale, down?, source, mem_util_pct, fan_pct, power_limit_w, temp_peak_c}], processes: [{gpu_index, pid, name, vram_mib: number|null, backend_id}], at_ms, has_polled, source_error}`. `vram_mib` is `null` when not measured (WDDM without PDH data) — unknown, never 0. `power_absence` classifies a null `power_w`: `driver_not_available` (driver answered N/A) vs `field_missing` (no power element in the source output). `down` marks a GPU retained after falling off the bus. Peaks ride on the samples.
- Side effects: lazily starts the telemetry poller on first use; emits the `telemetry-update` event.

### `telemetry_set_interval` — mutate
- What: Set the GPU poll interval.
- Entry: `telemetry_set_interval`, main.rs:4552. UI: yes.
- In: `ms: u64`. Out: `{ok: true, poll_interval_ms}`.
- Side effects: persists config immediately, but the running poller's cadence is fixed at startup — the new interval applies on the next app start.

### `telemetry_reset_peaks` — mutate
- What: Reset the per-GPU peak trackers (utilization, VRAM, temperature, power).
- Entry: `telemetry_reset_peaks`, main.rs:4565. UI: yes.
- In: none. Out: `{ok: true}`.
- Side effects: in-memory store only; nothing persisted.

### `telemetry_save_snapshot` — mutate
- What: Write the current telemetry snapshot to a file.
- Entry: `telemetry_save_snapshot`, main.rs:4572. UI: yes.
- In: none. Out: `{path}`.
- Side effects: writes `<data_dir>/telemetry-snapshot-<epoch_ms>.json`.

### `placement_propose` — read
- What: Project a tensor split for a model onto the latest telemetry snapshot's free VRAM.
- Entry: `placement_propose`, main.rs:4811. UI: yes.
- In: `model_id: string, split: number[], backend_id?: string`. Out: `PlacementProjection {model_id, split, per_gpu_mib, per_gpu_fits, fits, sys_mib, gpu_weights_mib}` plus command-added snapshot metadata `{free_mib, current_mib, additional_mib, snapshot_age_ms, gpu_indices, down_indices, weights_only}`. When `backend_id` names the serving backend, the projection credits the backend's own GPU-resident weights (applying restarts it and frees them first) and, under expert offload, divides only GPU-resident weights.
- Side effects: none (pure projection over a snapshot; catalog models resolve from `manager_models::curated_models`, others from the on-disk file size or a params-based estimate).

### `placement_apply` — mutate
- What: Validate a tensor split and write it to the backends serving a model (or one named backend).
- Entry: `placement_apply`, main.rs:4999. UI: yes.
- In: `model_id: string, split: number[], backend_id?: string`. Out: `{task_id, backends: string[], persisted}`.
- Side effects: validates split length against the telemetry GPU count; writes `flags.tensor_split` (implies `SplitMode::Layer` only when the mode was unset); persists config; journals backend-scoped activation entries; takes effect on the backends' next restart.

## 9. Tasks and diagnostics

### `tasks_list` — read
- What: All tracked background task records.
- Entry: `tasks_list`, main.rs:5176. UI: **no**.
- In: none. Out: `[TaskRecord {id, kind, detail, status, ...}]` (in-memory, session-only; kinds include `wizard_step`, `audit`, `backend_start/stop/restart`, `vscode_install_extensions`).
- Side effects: none.

### `task_abort` — mutate (bookkeeping only)
- What: Mark a task record as Aborted.
- Entry: `task_abort`, main.rs:5163. UI: **no**.
- In: `task_id: string`. Out: `{ok: true, task_id}`; NotFound error for an unknown id.
- Side effects: **flips the record's status only — it does not cancel the underlying work.** No cancellation token is wired to steps, downloads, or processes; real cancellation exists only per-operation (`cancel_download`, `stop_backend`, `gateway_stop`). A REST abort endpoint modeled on this command would claim an effect it does not have.

### `diagnostics_export_bundle` — mutate (file-producing)
- What: Export a diagnostics zip: config snapshot, per-scope recent logs (up to 5,000 lines each, including file-backed gateway and wizard scopes), telemetry snapshot, versions file.
- Entry: `diagnostics_export_bundle`, main.rs:5186. UI: yes.
- In: none. Out: `{path}` — a **server-side file path** (`<data_dir>/diagnostics/diagnostics-<ms>.zip`), not the bytes.
- Side effects: writes the zip under the data dir; may wait up to ~2.5 s for a first telemetry poll so the bundle is not empty. Bundle logs are raw and unredacted.

## 10. MCP servers and VS Code

MCP server definitions are built in (`manager_mcp::default_servers` —
GitHub and Google Drive), each with a mode: local | remote | disabled.
Modes are held **in memory only** — there is no MCP section in
`AppConfig`, so mode choices do not survive an app restart.

### `mcp_list` — read
- What: MCP servers with live status and per-key credential presence.
- Entry: `mcp_list`, main.rs:5310. UI: **no**.
- In: none. Out: `[{id, name, mode, transport, running, env_keys, credential_presence: [[key, present]]}]`.
- Side effects: `cmdkey` subprocess presence checks (values never read); reads supervisor state.

### `mcp_test_connection` — read (active probe)
- What: Test one MCP server: local = is its supervised process running; remote = HTTP health probe (5 s).
- Entry: `mcp_test_connection`, main.rs:5359. UI: **no**.
- In: `id: string`. Out: `{id, ok, detail}`.
- Side effects: network probe for remote servers; none for local/disabled.

### `mcp_set_mode` — lifecycle
- What: Set an MCP server's mode; switching to Local spawns its supervised process, switching away stops it.
- Entry: `mcp_set_mode`, main.rs:5406. UI: **no**.
- In: `id: string, mode: "local"|"remote"|"disabled"`. Out: `{id, mode}`.
- Side effects: spawns/stops a supervised MCP server process (env values injected at spawn from Credential Manager references); mode stored in memory only (see section note).

### `vscode_install_extensions` — long-running
- What: Install the pinned Continue.dev + Cline VS Code extensions.
- Entry: `vscode_install_extensions`, main.rs:5452. UI: **no**.
- In: none. Out: the wizard `WizardReport` for a single-step run of the `vscode-extensions` step.
- Side effects: runs `code --install-extension` subprocesses on the server; records a task.

### `vscode_write_config` — mutate
- What: Write the Continue editor config (`~/.continue/config.yaml` + `.env`) pointing at the local gateway.
- Entry: `vscode_write_config`, main.rs:5495. UI: **no**.
- In: `backup: bool`. Out: `{staged, canonical, backup_path, legacy_backup_path, old_models, new_models}`.
- Side effects: always stages copies under `<data_dir>/vscode/`; with `backup: true` also writes the canonical files in the user's home — existing `config.yaml`/`config.json` are first renamed to timestamped backups, and the gateway master key is merged into `~/.continue/.env` (the yaml references it as `${{ secrets.LITELLM_API_KEY }}`; no literal secret in the yaml). Ensures (generates + stores) the master key on first use.

## 11. MXC sandbox

MXC is a default-deny sandbox policy for tool execution (inference stays
on the host). The policy document lives at the configured path
(`cfg.mxc.path`); with no file, the built-in default policy applies.
Secrets never appear in the policy — credentials are Credential Manager
target names only.

### `get_mxc_policy` — read
- What: The active MXC policy with its provenance.
- Entry: `get_mxc_policy`, main.rs:2817. UI: yes.
- In: none. Out: `{policy: {version, filesystem: [{path, access}], network: [{host, ports, decision}], credentials: [{name, target}], default_deny, ui_access}, source: "file"|"default", mode}`.
- Side effects: reads the policy file.

### `update_mxc_policy` — mutate
- What: Validate and store a new sandbox policy.
- Entry: `update_mxc_policy`, main.rs:2835. UI: **no** (the Settings MXC card displays the policy; editing is not wired).
- In: `policy: Policy JSON`. Out: the `ValidationReport`.
- Side effects: validation-refused writes never reach disk; a valid policy is written atomically (temp file + rename) to `cfg.mxc.path`. This changes what tool execution the sandbox allows — security-relevant write.

### `mxc_set_mode` — mutate
- What: Set the MXC enforcement mode (learning | enforced | disabled).
- Entry: `mxc_set_mode`, main.rs:5616. UI: yes.
- In: `mode: MxcMode`. Out: `{ok: true, mode, persisted: true}`.
- Side effects: persists config. Note: the app *records* the mode; the MXC sandbox runtime applies it when that runtime executes tools.

### `mxc_run_self_test` — read
- What: Evaluate the fixed 12-probe set (benign + malicious cases) against the current policy.
- Entry: `mxc_run_self_test`, main.rs:5631. UI: yes.
- In: none. Out: `{cases: [{name, description, kind, expected, actual, passed}], summary: {total, passed, failed}}`.
- Side effects: none — pure policy evaluation; nothing is executed and no traffic is sent.

### `mxc_get_activity_report` — read
- What: Learning-mode activity observations (observed access decisions).
- Entry: `mxc_get_activity_report`, main.rs:5644. UI: yes.
- In: `since_ms?: u64`. Out: `{entries: [{timestamp, decision, description, kind, path_or_host, port, count}], source}` (`source` is the report path, or `"none"` with empty entries when no report exists).
- Side effects: reads `<data_dir>/mxc-activity-report.json`.

### `mxc_propose_tightening` — read
- What: Propose policy tightenings from a learning report against the current policy.
- Entry: `mxc_propose_tightening`, main.rs:5666. UI: **no**.
- In: `report: JSON` (a learning report document). Out: `{suggestions}`.
- Side effects: none (pure computation, `manager_mxc::suggest_rules`).

## 12. Windows ML and updates

### `winml_probe` — read (active probe)
- What: Probe the WinMLServer secondary backend and record its limits.
- Entry: `winml_probe`, main.rs:5683. UI: **no**.
- In: none. Out: the probe report (`WinMlRegistration`-adjacent: availability + `WinMlLimits` — no tensor split, max 1 registered model, coarse GPU selection, experimental).
- Side effects: HTTP probe of `http://127.0.0.1:<winml port>` (default 8090), 10 s transport timeout.

### `winml_register` — mutate
- What: Register a model with the Windows ML backend (tool-runner-class models only; the registry refuses ineligible classes).
- Entry: `winml_register`, main.rs:5701. UI: **no**.
- In: `model_id: string`. Out: the `WinMlRegistration {model_id, port, endpoint, registered_at, responding_adapter, limits}`.
- Side effects: network registration call to the WinML server; registry state in app memory.

### `winml_unregister` — mutate
- What: Unregister the Windows ML backend's model.
- Entry: `winml_unregister`, main.rs:5725. UI: **no**.
- In: none. Out: `{ok: true, removed: bool}`.
- Side effects: network call to the WinML server; clears the in-memory registration.

### `winml_status` — read
- What: WinML enablement, port, registered model, and the known limits.
- Entry: `winml_status`, main.rs:5733. UI: **no**.
- In: none. Out: `{enabled, port, model_id, registered: WinMlRegistration|null, limits}`.
- Side effects: none.

### `updates_check_all` — read (stub; always errors)
- What: Check all components for updates — **not implemented**: returns `Unsupported("Update checking ships in P6…")` on every call.
- Entry: `updates_check_all`, main.rs:5755. UI: **no**.
- In: none. Out: error string, always.
- Side effects: none.

### `updates_apply` — mutate (stub; always errors)
- What: Apply a component update — **not implemented**: always returns the same `Unsupported` error as `updates_check_all`.
- Entry: `updates_apply`, main.rs:5765. UI: **no**.
- In: `component_id: string`. Out: error string, always.
- Side effects: none today; the intended effect is component replacement on disk.

### `updates_pin` — mutate
- What: Pin a component to a version (or unpin with null) in the update policy.
- Entry: `updates_pin`, main.rs:5778. UI: **no**.
- In: `component_id: string, version?: string`. Out: `{ok: true, persisted: true, component_id, pinned_version}`.
- Side effects: persists config (`updates.components`).

### `updates_set_auto` — mutate
- What: Toggle a component's auto-update flag in the update policy.
- Entry: `updates_set_auto`, main.rs:5807. UI: **no**.
- In: `component_id: string, enabled: bool`. Out: `{ok: true, persisted: true, component_id, auto_update}`.
- Side effects: persists config.

## 13. Capabilities that exist in the backend but are not exposed in the UI

### 13.1 Registered commands the UI never invokes (25)

These are reachable today only through the Tauri bridge by name; no screen
calls them. For the REST design they are real, implemented capabilities —
several whole feature areas (MCP, WinML, updates policy) exist *only* here:

`update_config`, `backend_preview_diff`, `setup_get_prerequisites`,
`setup_retry_step`, `setup_skip_step`, `setup_mark_manual`,
`gateway_test_routing`, `gateway_recent_requests`, `tasks_list`,
`task_abort`, `mcp_list`, `mcp_test_connection`, `mcp_set_mode`,
`vscode_install_extensions`, `vscode_write_config`, `update_mxc_policy`,
`mxc_propose_tightening`, `winml_probe`, `winml_register`,
`winml_unregister`, `winml_status`, `updates_check_all`, `updates_apply`,
`updates_pin`, `updates_set_auto`.

### 13.2 Crate-level capabilities with no command at all

- **Model fit check** — `manager_models::fit_check` (Fits / FitsWithSplit / TooLarge against free VRAM per GPU) is implemented, but `list_models` hardcodes `fit_check_available: false` in v0.1.x; fit verdicts in the UI come from the placement projection instead.
- **Editor MCP config generation** — `manager_mcp::editor_mcp_config` generates the `mcpServers` JSON for Continue/Cline; used only in crate tests. Nothing writes it anywhere.
- **Config backup-fallback signal** — `manager_config::load_detailed` reports `used_backup` (the config was recovered from the `.bak` last-known-good copy); the app loads with plain `load` at startup and the fact never surfaces.
- **Supervisor graceful drain** — `SupervisedProcess::drain` (30 s default) exists only as part of stop/restart; there is no standalone drain-a-process capability.
- **Global routing-strategy resolution** — `manager_gateway::resolve_global_strategy` has no command; the UI reimplements it in TypeScript (`effectiveGatewayStrategy` in `ui/src/api.ts`). Two implementations of one rule — the API should have exactly one, server-side.
- **Catalog management** — the curated model catalog (`manager_models::curated_models`, `CATALOG_REVISION`) is compiled in; `ensure_catalog_models` (main.rs) seeds/repairs stored model metadata against it implicitly on config writes. There is no catalog read endpoint, no refresh trigger, and no way to see the catalog except through `list_models`.
- **Orphan backend adoption** — at launch the app adopts llama-server processes started by a previous session (tracked by pid). Adoption is automatic and invisible as a capability; adopted backends show an `adopted` flag in `list_backends` and are stoppable, but there is no adopt/release operation.
- **Gateway metrics polling** — a background loop scrapes LiteLLM's Prometheus endpoint every 15 s into `GatewayStatsStore` (`src-tauri/src/gateway_stats.rs`), producing the trailing-hour `requests_1h` / `errors_1h` / `throughput_toks` on `list_backends` and the in-flight gauge `gateway_quiesce_wait` reads. No command exposes the store, the raw metrics, or the poller's health.
- **MXC policy decision point** — `manager_mxc::decide` (the actual allow/prompt/deny evaluation the sandbox runtime uses) has no command; only its offline derivatives (self-test, tightening proposals) are exposed.
- **Credential writes** — the app *reads* credential presence (`cmdkey`) in several places and *writes* exactly two credentials, both inside the gateway machinery: the LiteLLM master key and the Postgres password (generate + store in Windows Credential Manager on first use). There is no general credential read/write capability, and secret values never appear in config or policy files — references only. (`gateway_master_key`, §7, is the one place a secret value crosses the command boundary.)

## 14. UI flows that compose multiple commands

The desktop UI builds these operations client-side from the commands
above. A REST API should expose each as a single operation (or an explicit
job), not require the client to replay the composition:

- **Apply activation plan** (`applyActivationPlan`, `ui/src/views/static.ts`; the top banner in `ui/src/main.ts` runs the same routine): for each id in `backend_restart_ids` → `restart_backend`; then, if `gateway_pending`: (`gateway_quiesce_wait` in Drain mode) → `gateway_stop` → `run_wizard_step("start-gateway")` → `gateway_activation_complete`.
- **Restart gateway** (Gateway view): `gateway_quiesce_wait` (Drain path) or nothing → `gateway_stop` → `run_wizard_step("start-gateway")`.
- **Start gateway** (Gateway view / wizard): `run_wizard_step("start-gateway")` alone — all gateway startup logic lives inside that step.
- **Dashboard load** (`getDashboard`, `ui/src/api.ts`): `get_status` + `list_backends` + `get_gpu_telemetry` + `gateway_status` + `get_config` + `get_gateway_config` in parallel, merged client-side (including placement derivation from config and per-backend VRAM-held attribution from telemetry processes).
- **Placement flow** (GPUs/Backends views): `placement_propose` (preview) → `model_assign` (only when the selected backend does not already serve the model) → `placement_apply`.
- **Edit-flags apply with restart**: `backend_apply_config` with `RestartNow` (records a restart task) → the view separately calls `restart_backend`.
- **Model download lifecycle**: `download_model` → progress/state observed by polling `list_models` and listening for `download-progress` events → the backend watcher auto-verifies on completion (no client call).
- **Wizard run**: per-step `run_wizard_step` with `setup_get_steps` polling for the checklist, `setup_get_commands` for the manual fallback block, and `setup_mark_complete` at the end.
- **Diagnostics hand-off**: `diagnostics_export_bundle` → `open_folder(returned path)`.

### Push events (Tauri events, not commands)

Emitted today: `telemetry-update` (from `get_gpu_telemetry`),
`wizard-progress` (step/audit runs), `download-progress` (download
worker), `backend-status` (start/stop/restart). Declared in
`src-tauri/src/main.rs` (`pub mod events`) but **never emitted**:
`log-line`, `telemetry-tick`, `task-progress`, `gateway-request`,
`alert-raised`, `vram-changed`, `config-changed`. The desktop UI is
therefore mostly poll-driven despite the event names; a REST design
should decide deliberately between polling resources and a real
stream (SSE/WebSocket) rather than inheriting this split.

## 15. Counts, and capabilities with awkward fits for REST

### Count by class (82 registered commands)

| Class | Count | Commands |
|---|---|---|
| read | 32 | get_status, get_config, list_backends, test_backend, backend_get_config, backend_preview_diff, backend_get_launch_command, setup_get_prerequisites, setup_get_commands, setup_get_steps, setup_status, setup_step_help_url, gateway_master_key, list_models, get_logs, get_gateway_config, gateway_status, gateway_activation_plan, gateway_preview_yaml, gateway_recent_requests, get_gpu_telemetry, placement_propose, tasks_list, mcp_list, mcp_test_connection, get_mxc_policy, mxc_run_self_test, mxc_get_activity_report, mxc_propose_tightening, winml_probe, winml_status, updates_check_all |
| mutate | 36 | update_config, backend_apply_config, backend_add, backend_update_flags, backend_set_offload, backend_delete, setup_retry_step, setup_skip_step, setup_mark_manual, setup_mark_complete, open_url, open_folder, gateway_set_bind_host, model_set_short_name, model_delete, model_assign, model_add, model_set_enabled, update_gateway_config, gateway_activation_cancel, gateway_activation_complete, gateway_test_routing, telemetry_set_interval, telemetry_reset_peaks, telemetry_save_snapshot, placement_apply, task_abort, diagnostics_export_bundle, vscode_write_config, update_mxc_policy, mxc_set_mode, winml_register, winml_unregister, updates_apply, updates_pin, updates_set_auto |
| lifecycle | 8 | start_backend, stop_backend, restart_backend, cancel_download, model_pause_download, model_resume_download, gateway_stop, mcp_set_mode |
| long-running | 6 | run_wizard_step, run_audit, download_model, model_verify_checksum, gateway_quiesce_wait, vscode_install_extensions |

By UI wiring: 57 invoked by the UI, 25 registered but never invoked (§13.1).
Two of the 82 are stubs that always error (`updates_check_all`,
`updates_apply`). One UI wrapper targets a command name that does not
exist (`backend_test_request` vs the registered `test_backend`, §2).

### Awkward fits for REST — flagged for the API designer

1. **Long-blocking synchronous calls.** `run_wizard_step` blocks until the step finishes (budgets to 600 s; the start-gateway step internally reinstalls packages and confirms health). `model_verify_checksum` blocks for minutes on multi-GiB files. `gateway_quiesce_wait` blocks 5–600 s *by design*. These need an async job model (202 + job resource + polling/stream), not request/response timeouts.
2. **Background work with event-only progress.** `download_model` returns immediately; progress, pause state, the post-download verification phase, and terminal errors are observable only through `download-progress` events and by polling `list_models`. A REST client needs a first-class download resource with state, or a stream.
3. **No live log channel.** `get_logs` is a paged pull and works fine as REST, but live tailing does not exist — the `log-line` event is declared and never emitted. Streaming logs over the API is new machinery, not a port.
4. **Interactive elevation (UAC).** The start-gateway step's firewall reconciliation and install-llamacpp's host preparation run elevated scripts: a UAC prompt appears on the **server's console**, and a remote caller cannot see or answer it (a decline degrades to a warning). API-triggered steps that need elevation will block on a prompt nobody remote can see, or must pre-decline.
5. **Desktop-session actions.** `open_url` and `open_folder` act on the managed machine's interactive desktop (browser, Explorer). They have no remote meaning; exclude them or replace them with returned data (URLs, paths).
6. **Secret disclosure.** `gateway_master_key` returns the LiteLLM master key in plaintext (and creates it on first call). Over an admin API this is the most sensitive read in the system — highest role, audit logging, and arguably a reveal-once pattern rather than a plain GET.
7. **`task_abort` does not abort.** It flips a bookkeeping record's status; the underlying work continues. A REST design must not inherit this semantics — real cancellation exists only per-operation today.
8. **Busy/lock semantics instead of queueing.** Wizard execution serializes on one lock and concurrent calls return `busy` + the running step + elapsed seconds. The API should preserve this as an explicit conflict response, not hide it behind a blocking wait.
9. **Session-only state.** MCP server modes, the routing-test trace ring, task records, and the gateway stats windows live in app memory and vanish on app restart. REST resources modeled on them are ephemeral whether the design admits it or not; MCP modes in particular look like configuration but are not persisted.
10. **File-path responses.** `diagnostics_export_bundle` and `telemetry_save_snapshot` return server-side paths, not content. The API needs file transfer (or a two-step create-then-download) for both.
11. **Adopted processes.** Backends adopted from a previous app session are tracked by pid, not as children; stop/restart behavior differs subtly from session-started backends, and an API restart of the *manager itself* changes which processes are adopted vs owned.
12. **Untyped payloads and string errors.** Every command returns free-form JSON and errors are single strings with an embedded kind. The REST layer needs explicit DTOs, an error taxonomy with status codes, and payload versioning — none of which can be generated mechanically from the current boundary.
13. **Client-side compositions.** Apply-plan, gateway restart, dashboard assembly, and the placement flow (§14) are multi-call sequences with ordering and partial-failure states. Exposing the primitives alone would push fragile orchestration onto the Android client.
14. **Active probes disguised as reads.** `test_backend` consumes inference, `gateway_status` and `mcp_test_connection`/`winml_probe` make network probes with multi-second timeouts, and `get_gpu_telemetry` lazily starts a background poller. Rate and timeout behavior for these needs explicit design, not GET-and-forget.
