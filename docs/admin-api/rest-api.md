# Admin API — REST Contract

Status: implemented. The API shipped with the admin-API build
(PLAN.md §9) and this contract describes the shipped surface as of
v0.1.50. The source of truth for routes is the operation registry —
`crates/manager-app/src/registry.rs` (`for_each_operation!`): 161
operations, of which 132 carry a REST route and 29 are explicit
`None` exclusions (§6.16). Derived/composite and auth/admin routes
are API-layer routes in `crates/manager-api` (§6.17); the full live
surface is 175 route records (132 registry + 19 derived + 24
auth/admin).
Scope: an HTTP API served by the Service Manager on a dedicated **admin port**
on the managed machine (separate from the LiteLLM gateway port), exposing the
app's capabilities for read and control from a remote client (first consumer:
an Android app).

Original source of the capability audit: the Tauri commands
registered in `src-tauri/src/main.rs` (`tauri::generate_handler!`)
and the client wrappers and payload types in `ui/src/api.ts`. Where
this document names an app command, that is the operation the
endpoint wraps (registry name in `manager_app::registry`).

Companion documents (separate workstreams): the authN/authZ and roles plan
consumes the capability classes defined in §2.7; this contract assumes every
endpoint requires authentication and says nothing about the scheme.

---

## 1. Design rules

1. **The API is a thin, faithful wrapper over the app's command layer.** An
   endpoint exists because an app command exists, with the same arguments and
   the same result shape. The API invents no new behavior. The single
   exception is the activation-plan apply orchestration (§6.5), which today
   lives in the desktop UI layer.
2. **Payload field names are the app's payload field names** (snake_case
   JSON, exactly as the Tauri commands emit). An Android client and the
   desktop UI read the same shapes.
3. **Long-running work is an operation** (§3). Nothing that can run longer
   than ~30 s is a synchronous request.
4. **Unknown is never fabricated.** Fields the app omits when unmeasured
   (e.g. per-process VRAM on WDDM GPUs, gateway statistics before a metrics
   window exists) stay omitted in API responses.

## 2. Conventions

### 2.1 Base path and versioning

- Base: `https://<host>:<admin-port>/api/v1` (self-signed certificate,
  pinned by fingerprint; plain HTTP exists only on loopback in
  development)
- The admin port is new configuration (not the gateway port, not the app's
  internal IPC). It binds per operator configuration; see the auth plan for
  the exposure rules.
- Versioning is a single path segment (`v1`). Breaking shape changes ship as
  `v2` alongside `v1`; additive fields never bump the version.
- Discovery is split as built: `GET /api/v1/server` (§6.1) is the
  public identity document — an unauthenticated client can confirm
  it is talking to a Service Manager admin API and learn its
  versions before presenting credentials. `GET /api/v1/status`
  itself requires authentication, like every other operation route.

### 2.2 Format

- JSON only, `Content-Type: application/json`, except
  `GET /gateway/generated-config` (`text/yaml`) and
  `GET /diagnostics/bundles/{id}/content` (`application/zip`).
- Request bodies are JSON objects. Unknown fields are rejected with
  `INVALID_INPUT` (the app's patch objects are closed structs; a typo'd
  field must not silently no-op). That includes the fields inside a
  backend `patch` and inside a gateway-config patch.
- Query values are read by the parameter's declared type: `?limit=20` is
  a number, `?search=20` is text; a value that does not fit is
  `INVALID_INPUT`.
- Field names: snake_case, mirroring the app payloads. The desktop's
  camelCase invoke arguments (`stepId`, `applyMode`) do not cross the wire.

### 2.3 IDs

- Backends, models, gateway groups, routing rules, wizard steps: operator- or
  catalog-chosen string IDs, used verbatim as path segments (URL-encode when
  needed). Backend and model IDs are unique within their collection; group
  names are unique within the gateway config.
- Operations: server-generated opaque strings (the app's task IDs).
- GPUs: integer index (`0`, `1`, …) in nvidia-smi order, as in telemetry.

### 2.4 Timestamps and durations

- Timestamp fields in responses: RFC 3339 UTC strings — the format the app
  already uses for log records (`ts`) and model verification (`verified_at`).
- Time-window parameters: epoch milliseconds (`since_ms`, `until_ms`),
  mirroring `get_logs`.
- Durations and ages: numeric, unit in the field name (`duration_ms`,
  `uptime_secs`, `snapshot_age_ms`), as in the app payloads.

### 2.5 Errors

Every error response uses one envelope:

```json
{
  "error": {
    "code": "NOT_FOUND",
    "kind": "NotFound",
    "message": "not found: backend coder-fast",
    "details": null
  }
}
```

- `kind` is the app's `ApiError` variant name (`manager-app`):
  `Config`, `Supervisor`, `Wizard`, `Telemetry`, `Gateway`, `Models`,
  `Mxc`, `Mcp`, `WinMl`, `Services`, `NotFound`, `InvalidInput`,
  `ContractGap`, `Unsupported`, `Io`, `Json`, `Busy`.
- `code` is the stable machine-readable form (SCREAMING_SNAKE of `kind`),
  plus REST-layer codes that have no app counterpart: `BUSY` (a singleton
  resource is already running something), `OPERATION_FAILED` (an operation's
  terminal error, §3), and the auth codes (`UNAUTHENTICATED`,
  `FORBIDDEN`, `INVALID_CREDENTIALS`, `SESSION_REVOKED`,
  `PASSWORD_CHANGE_REQUIRED`; see §6.17), and `PLAN_EXISTS` (§6.19).
- `message` is the app's error string, unparsed, with the machine's data
  and home directories replaced, at most 4000 characters. Clients
  display it; they never branch on it.
- HTTP status mapping:

  | Condition | HTTP | code |
  |---|---|---|
  | Invalid argument / body fails validation | 400 | `INVALID_INPUT` |
  | Referenced backend/model/group/step/operation absent | 404 | `NOT_FOUND` |
  | Missing, invalid, expired or revoked bearer | 401 | `UNAUTHENTICATED` |
  | Role may not use the endpoint; API token on account self-service | 403 | `FORBIDDEN` |
  | Account flagged must-change-password (after an admin reset) on any route but `POST /auth/password`, `POST /auth/logout`, `GET /auth/me`; also `POST /auth/refresh` | 403 | `PASSWORD_CHANGE_REQUIRED` |
  | Singleton busy (wizard step already running, download already in flight, plan apply already running), or a lifecycle operation already in flight on the same service (app kind `Busy`, e.g. "gateway restart already in progress (started by activation 12 s ago)") | 409 | `BUSY` |
  | `POST /optimize/consult` without `replace` while a plan exists | 409 | `PLAN_EXISTS` |
  | Config patch fails validation (a gateway-config patch: 400 `INVALID_INPUT`) | 422 | `CONFIG` |
  | Capability deferred in the app itself | 501 | `UNSUPPORTED` |
  | App contract gap (a crate API cannot express the operation) | 501 | `CONTRACT_GAP` |
  | Everything else (`Supervisor`, `Gateway`, `Wizard`, `Telemetry`, `Models`, `Mxc`, `Mcp`, `WinMl`, `Services`, `Io`, `Json`) | 500 | per kind |

- Implementation note: the Tauri boundary flattens `ApiError` to its
  Display string. The REST layer must capture the structured kind before
  flattening; the strings alone are not a contract.

### 2.6 Lists, filtering, pagination

Only one app capability is a true paged query — logs — and the convention
follows it:

- Collection endpoints return the app's envelope key, complete and
  unpaged (backends, models, groups, tasks are operator-scale lists):
  `{ "backends": [ … ] }`, `{ "models": [ … ] }`, `{ "requests": [ … ] }`.
- `GET /logs` keeps `get_logs`' exact semantics: `offset` + `limit`
  paging over a filtered, timestamp-ordered set; the response carries
  `total_matching` (and its alias `total`), `truncated`, and the `scopes`
  list. Filters: `level`, `search`, `query`, `since_ms`, `until_ms`,
  `order=asc|desc` (default `desc`, newest first). See §6.8.

### 2.7 Capability classes (input to the roles plan)

Each endpoint is annotated with one class. These are coarse capability
buckets for the authN/authZ workstream to map to roles; they are not roles.

| Class | Meaning |
|---|---|
| `read` | Returns state; no side effects. |
| `control` | Starts/stops/restarts processes, runs steps, changes live state. |
| `config` | Persists configuration changes (may additionally require activation). |
| `destructive` | Deletes resources or files, or cancels saved work. |
| `secret` | Returns secret material. Exactly one endpoint: the gateway master key. |

## 3. Long-running operations: the operation model

**Decision: asynchronous operations.** Any call whose work can exceed ~30 s
— or whose duration is open-ended — returns `202 Accepted` with an
operation resource; the client polls it. Everything else is synchronous
with its worst-case duration documented per endpoint.

Why this model, over retained synchronous endpoints:

1. **It is the app's own model.** The app already records work as tasks:
   backend start/stop/restart return `{ task_id, backend_id }`;
   `run_wizard_step` records a `wizard_step` task; `backend_apply_config`
   with `RestartNow` returns a `restart_task_id`; `tasks_list` /
   `task_abort` enumerate and cancel them. The REST operation resource is
   the app's `TaskRecord` (`{ id, kind, status, detail, created_ms }`,
   main.rs:251) exposed over HTTP — not a new invention.
2. **The durations do not fit HTTP requests.** Wizard steps carry budgets
   up to 600 s; the start-gateway step's prisma generate and sustained-
   health confirmation run minutes; checksum verification of a multi-GiB
   GGUF runs ~15+ minutes; downloads run for hours. A synchronous design
   would fail on client and intermediary timeouts — the Android client
   especially (background execution limits, network changes mid-request).
3. **Busy state is representable.** The app's singleton locks (one wizard
   step at a time, one download per model) surface as `409 BUSY` on a
   duplicate POST and as the operation's `running` state — the same
   information the desktop UI shows ("step X has been running for Ns").

### 3.1 Operation resource

```json
{
  "id": "task id (opaque)",
  "kind": "wizard_step | backend_restart | model_download | model_verify | gateway_start | audit | diagnostics_bundle | …",
  "status": "running | done | failed | aborted",
  "detail": { },
  "created_ms": 1792200000000,
  "started_ms": 1792200000000,
  "finished_ms": null,
  "error": null,
  "error_code": null,
  "error_kind": null,
  "task_id": null
}
```

- `kind` of a `202` record is the operation's registry name
  (`run_wizard_step`, `restart_backend`, `download_model`, …, plus
  `gateway_start` and `activation_apply`); an app task has the app's
  own kind (`wizard_step`, `backend_restart`, `model_download`, …).
- A `failed` operation carries `error` (text), `error_code` and
  `error_kind`: the §2.5 code and kind a synchronous route would have
  answered, or `OPERATION_FAILED` when the operation's own payload
  reports the failure.
- One unit of work has up to two rows, which name each other: the `202`
  record's `task_id` is the app task it runs as (`null` when the work
  has none), and the task row's `operation_id` is the `202` record
  (`null` for work that did not come through this API).
- **Local paths.** `detail` may contain local paths (a download's
  `dest`, a diagnostics bundle's `path`, step output). Only
  administrators see them; for every other role the data directory
  reads `<data-dir>` and the home directory `<home>`, here and in the
  `task-progress` event. The other routes that deliberately carry
  local paths are `GET /backends/{id}/launch-command` (admin), the
  response of `POST /telemetry/snapshots` (admin, like `detail`) and
  `GET /config` (`data_dir`, `config_path`, model files: any role).
- `detail` is the kind-specific payload the app already attaches to its
  task records (e.g. `{ "step_id": "start-gateway" }`,
  `{ "backend_id": "coder" }`, `{ "model_id": "…" }`), extended at
  completion with the outcome the synchronous command would have returned
  (for a wizard step: the `WizardStepOutcome` — `state`, `message`,
  `log_tail`, `duration_ms`, `manual_command`).
- `status` vocabulary mirrors `TaskStatus` in the app (`running`, `done`,
  plus terminal failure/abort states the REST layer names explicitly —
  the app's enum is the implementation detail; the wire vocabulary above
  is the contract).
- Progress where the app has it: model downloads expose byte progress on
  the model resource itself (`downloading`, `paused`, `partial_bytes`,
  `verifying`, `download_error` in `GET /models/{id}`), so the operation
  for a download is a completion handle, not the progress channel —
  exactly as the desktop UI works (invoke, then poll `list_models`).
- Poll cadence guidance for clients: 1 s for the first 10 s, then 5 s;
  operations are retained in the list for the life of the process
  (mirroring `tasks_list`).
- **Terminal outcome rule (every `202` route).** An operation ends
  `failed` when its op returns an error, and also when its payload
  reports `busy: true` or `ok: false` — the one shared lifecycle rule
  (`manager_app::lifecycle::payload_failure`). A start-gateway step
  whose outcome is `failed` or `manual`, a refused (busy) run, a
  gateway restart that did not come back healthy, and an activation
  apply with a failed item are therefore all `failed` operations,
  never `done`. The `error` is the payload's `message`, and the
  `detail` keeps the whole payload (outcome, `manual_command`, items,
  `execution_id`).
- **Refused before it is queued.** What can be decided without waiting
  answers at once instead of `202`: a missing or wrongly typed argument
  `400`, an unknown backend, model or step `404`, and `409 BUSY` when
  the service, setup step, audit, plan apply, download or verification
  it needs is already being worked on (`details` names what is
  running). Both can still arise after the `202`: the operation then
  fails with the same `error_code`.
- **What `done` means.** A start or restart is `done` when the process
  is spawned; with `wait_ready_secs` only once it answers `/health`. A
  download is `done` when the file is complete and verified against its
  pin, `failed` or `aborted` otherwise.
- **Origin.** Lifecycle operations take an optional `origin` body
  field (`api`, `cli`, …) that lands in the service's lifecycle log
  markers and `service-lifecycle` events. The REST layer fills in
  `api` when the caller names none; the CLI sends `cli`.
- **Started work.** `POST /bench/runs` is the one `202` whose body is
  the app's own task record (`id` = the task's id) plus the
  operation's fields: refusals are synchronous errors, and the id
  polls and aborts under `/operations` like any other.
- Cancellation: `POST /operations/{id}/abort` (§6.10), with either id.
  Not every kind is abortable; the operation's terminal state says what
  happened.

### 3.2 What is synchronous, and its bound

Reads (all `GET`s except bundle content), config writes, placement
propose, routing test, MXC self-test and policy writes, and the gateway
quiesce wait (bounded by the caller's `timeout_secs`, clamped to 5–600).
The one synchronous call with model-scale latency is
`POST /backends/{id}/test` (a real inference request, at most 30 s).

## 4. Resource shapes

The major resources, with the app's field names. (Full per-field detail
lives in the app types cited; this is the wire contract summary.)

### 4.1 Backend (`list_backends` row)

```json
{
  "id": "coder",
  "enabled": true,
  "configured": true,
  "port": 8082,
  "model_file": "Qwen3-Coder-Next-UD-Q4_K_M.gguf",
  "state": "stopped | starting | running | stopping | failed",
  "pid": 12345,
  "adopted": false,
  "restarts": 0,
  "uptime_secs": 3600,
  "requests_1h": 0, "errors_1h": 0, "throughput_toks": 0,
  "n_ctx": 32768, "n_batch": 512,
  "tensor_split": [0.5, 0.5],
  "group": "coder",
  "gpu_ram_mib": 2458, "sys_ram_mib": 44564, "kv_ram_mib": 768,
  "rs_ram_mib": null, "failure_reason": null,
  "memory_basis": "exact | catalog | estimate",
  "offload": { "moe_layers": 48, "layers": 40, "expert_bytes": 0,
               "total_bytes": 0, "green_limit_layers": 48,
               "green_end": 1.0, "yellow_end": 1.0 },
  "mtp": { "enabled": true, "n_max": 2, "parallel": 1,
           "custom": false },
  "moe_cache": { "mib": 4096, "custom": false,
                 "offload_layers": 48, "moe_layers": 48 },
  "raw_flags": "--parallel 1",
  "flags_advanced": false,
  "predicted": { "prompt_per_second": 33.5,
                 "predicted_per_second": 22.5, "ttft_ms": 420,
                 "measured_at": "2026-10-18T14:03:00Z",
                 "source": "benchmark", "backend_id": "planner",
                 "origin_backend_id": null,
                 "model_id": "qwen3-27b-planner",
                 "model_file": "Qwen3.8-27B-IQ3_S.gguf",
                 "app_version": "0.1.42",
                 "driver_version": ["581.57"],
                 "llama_build": "b11658",
                 "inputs": { "n_ctx": 32768, "n_batch": 512,
                             "offload": "none",
                             "tensor_split": [1.0, 0.0],
                             "split_mode": "layer", "raw_flags": "",
                             "gpus": ["0:NVIDIA RTX A4000"] } }
}
```

Trailing-hour statistics and memory/offload blocks are present only when
the app reports them (a metrics window exists; the model resolves); they
are never zero-filled (§1 rule 4). The `mtp` block is present only for
MTP-capable backends (the catalog pins the capability); `n_max` is the
explicit `--spec-draft-n-max`, or null when MTP is on without one
(llama.cpp's default applies) or off; `parallel` is the explicit
`--parallel` slot count, or null when unset. The `moe_cache` block is
present only for offload-capable backends (the `offload` block's
population); `mib` is the saved `--moe-cache-mib`, or null when the
expert cache is off, and `offload_layers` is the backend's saved
offload position — the cache control is disabled at 0.
`configured: false` marks a
supervisor-only orphan process; orphans accept `stop` only. The
`predicted` block is present only when an exact-match measurement
exists for the backend's saved configuration (predicted-throughput
design §7): the standardized benchmark's headline figures plus the
measurement's provenance (date, source, app version, driver, llama
build, and the inputs snapshot it was measured under). It is absent —
never zero-filled — when no measurement matches, and it describes the
saved configuration regardless of run state.

### 4.2 Backend config (`backend_get_config`)

```json
{
  "id": "coder", "enabled": true, "model_file": "…", "port": 8082,
  "flags": { }, "raw_flags": "-ot exps=CPU", "flags_advanced": false,
  "restart_policy": { }, "overrides_global": false
}
```

A config patch (`BackendPatch`, `crates/manager-config/src/diff.rs:21`):
any subset of `enabled`, `model_file`, `port`, `n_ctx`, `n_batch`,
`tensor_split`, `split_mode`, `raw_flags` (`null` clears them),
`max_retries`, `backoff_base_secs`; `null` in any other field leaves it
alone. An unknown field or a wrong type is `400`, and `n_ctx`/`n_batch`
have the limits of `PUT …/flags`. A preview diff (`ConfigDiff`):
`{ backend_id, changes: [{ field, old, new }], valid }`.

### 4.3 Model (`list_models` row)

```json
{
  "id": "qwen3-coder-30b",
  "name": "Coder",
  "gguf_path": "…", "source_url": "https://huggingface.co/…",
  "quant": "UD-Q4_K_M", "params_b": 80.0,
  "sha256": "3a47…", "verified_at": "2026-10-16T…Z",
  "enabled": true, "on_disk": true, "size_bytes": 49301055488,
  "downloading": false, "verifying": false, "paused": false,
  "partial_bytes": null, "download_error": null,
  "catalog_size_bytes": 49301055488, "ctx_max": 262144,
  "moe_layers": null, "default_offload_layers": null,
  "catalog_moe_layers": 48, "catalog_default_offload_layers": 48,
  "mtp": null, "catalog_mtp": false,
  "file_moe_layers": 48, "file_nextn": false, "moe_reconcile": null,
  "fit_check_available": true
}
```

`source_url` is the record's source URL. `file_moe_layers` and
`file_nextn` are the on-disk file's parsed MoE block count and
in-file MTP signal (null when the file is absent or unparseable);
`moe_reconcile` is `{ "declared", "file" }` when the stored MoE
declaration disagrees with the file's parsed count and no catalog
pin governs the figure, else null — the declaration editor's
post-download proposal (deep-discovery design Part 2, P2-D7).

### 4.4 Gateway config (`get_gateway_config`)

```json
{
  "port": 4000,
  "bind_host": "0.0.0.0",
  "groups": [ { "name": "planner", "members": ["planner"],
                "strategy": "…", "fallbacks": [],
                "enabled": true, "enabled_members": 1 } ],
  "routing_rules": [ { "id": "…", "description": "…", "source": "…",
                       "litellm_fragment": "…" } ],
  "request_log_len": 0,
  "openrouter": { "enabled": false, "cred_ref": null,
                  "daily_cap_usd": 0, "model_allowlist": [] },
  "effective_strategy": "latency-based"
}
```

Group `enabled`/`enabled_members` are derived by the app (a group is
enabled iff at least one member backend is enabled); they are read-only
in the API as in the app. `group_strategies` (`[{group, strategy}]`)
is the strategy the generated config applies to each group it
includes. `effective_strategy` is the generated config's global
routing strategy, the default of groups without their own, computed by the
generator's own rule (`manager_gateway::effective_global_strategy`);
`null` when no config would be generated. Clients display it and never
re-derive it.

### 4.5 Gateway status (`gateway_status`)

```json
{
  "port": 4000, "bind_host": "0.0.0.0", "lan_ip": "192.168.5.23",
  "groups": 5, "routing_rules": 0, "up": true,
  "state": "restarting",
  "transition": { "kind": "restarting", "origin": "api", "since": "…" },
  "intent": "running", "pid": 12345, "found_by": "pid_file",
  "service_managed": false, "failure": null,
  "postgres": { "installed": true, "state": "running", "pid": 6789,
                "port": 5432, "port_mismatch": false, "transition": null },
  "activation_applying": null,
  "routing_log_len": 0, "openrouter_cred_present": null,
  "activation_pending": true, "activation_changes": 3,
  "openrouter_spend": {
    "enabled": true, "tier_active": true,
    "spent_today_usd": 4.1, "cap_usd": 5.0, "fraction": 0.82,
    "capped": false, "warn": true,
    "resets": "local_midnight", "budget_window": "1d"
  },
  "litellm_version": "1.x"
}
```

`openrouter_spend` (additive) is the app's measurement of today's
OpenRouter spend (local day, from the gateway's `litellm_spend_metric`
counters) against `gateway.openrouter.daily_cap_usd`. The cap itself is
enforced inside the gateway by LiteLLM's provider budget
(`router_settings.provider_budget_config.openrouter`, 1-day window);
`capped` is true when the tier is enabled and the cap is zero or
reached, `warn` at ≥80% of the cap.

- `port` is the saved port; `running_port` the one the running gateway
  answers on (`null` while down) and `port_pending` whether they differ.
- `groups`, `effective_strategy` and `group_strategies`
  (`[{group, strategy}]`) describe the running gateway's applied
  config; `saved_groups`, `saved_effective_strategy` and
  `saved_group_strategies` are present only where the saved
  configuration differs.
- `requests_1h`, `errors_1h`, `stats_window_secs`: client requests the
  gateway answered over the trailing hour (`null` until measured).
- `state` is the shared lifecycle state every surface renders:
  `running`, `starting`, `restarting`, `stopping`, `stopped`,
  `failed`, `degraded` (process alive but not answering), `unknown`.
  During a restart it reads `restarting` (never `stopped`). `up` is
  the bare liveness probe, kept for older clients.
- `transition` is the lifecycle operation in flight (`kind`,
  `origin`, `since`), null when settled. `intent` is what the
  operator last asked for (`running` | `stopped`); `failure`
  (`at`, `reason`, `exit_code`) is the last unexpected failure,
  cleared at the next start.
- `pid` / `found_by` (`pid_file` | `port_owner`) identify the
  gateway process; `service_managed` is true when the Windows
  service owns it.
- `postgres` is the app-managed database's own view (`state`:
  `not_installed` | `running` | `starting` | `stopped` | `failed`,
  plus `stopping` during its own stop); `port_mismatch` flags a
  running server on a port other than the configured one (the one
  case a gateway start restarts PostgreSQL).
- `activation_applying` is the server-side activation apply in
  flight (`source`, `mode`, `started_at`, `step`, `target`,
  `message`), null when idle.

`groups` counts enabled groups only (derived as in §4.4: at least one
member backend enabled) — the groups the gateway actually serves, not
the configured total (Pete, 2026-10-17, tracker #11).

### 4.6 Activation plan (`gateway_activation_plan`)

```json
{
  "pending": true, "gateway_up": true, "gateway_pending": false,
  "drift": false, "files_differ": false,
  "applies_at_next_start": false, "actionable": true,
  "backend_restart_ids": ["coder"],
  "changes": [ { "ts": "…", "kind": "…", "summary": "…",
                 "scope": "backend", "targets": ["coder"],
                 "activation": "restart" } ],
  "admin_approval_required": false, "admin_approval_reason": null,
  "snapshot_available": true
}
```

One edit is one change: a record the gateway and a backend both must
pick up is one row (scope `gateway`), and drift that a pending gateway
change explains is not listed again (`drift` is then `false`,
`files_differ` `true`). `targets` names the backends an edit touched, in
both scopes. `admin_approval_required` says that applying changes the
gateway's port or network access, for which Windows asks for
administrator approval (firewall rule).

When live config drift is the pending work, `changes` carries a
synthesized item with `kind: "gateway_drift"` (scope `gateway`,
activation `gateway_restart`) whose summary names the generated
file that differs — drift is counted like any other change, so a
pending plan never reports zero changes. `files_differ` is the
raw generated-vs-on-disk comparison in any gateway state
(informational while the gateway is down); `drift` remains its
pending-work form (gateway up only).

### 4.7 GPU telemetry (`get_gpu_telemetry`)

Per-GPU samples: `index`, `name`, `ts`, `utilization_pct`,
`vram_used_mib`, `vram_total_mib`, `temp_c`, `power_w` (nullable),
`power_absence` (nullable: `driver_not_available` when the driver
answered N/A for the power elements, `field_missing` when the source
output carried no power element at all; null when power was measured),
`stale`, `down`, `source` (`nvml` | `nvidiasmi`), `mem_util_pct`,
`fan_pct`, `power_limit_w`, `temp_peak_c`; plus per-process attribution:
`gpu_index`, `pid`, `name`, `vram_mib` (**null when not measured** —
WDDM devices), `backend_id`. The payload also carries the derived
per-GPU info list the desktop renders. Field-for-field the app's
payload; the API adds nothing.

### 4.8 GPU history (`get_gpu_history`)

Recorded per-GPU history behind the GPUs page charts, sampled from
the telemetry snapshot every 15 s and persisted in the data dir
(`gpu-history.json`): full-rate points for 48 h, 5-minute averaged
points for 7 days. Payload: `{ range, generated_at_ms, gpus }`;
each entry is `{ index, name, interval_ms, points }` where a point
is `{ ts_ms, temp_c, util_pct, throughput_toks, power_w }`.
`throughput_toks` and `power_w` are **null when not measured** at
that tick: power is null when the card reports no power reading,
and throughput is null when no running backend's rate attributes
to the GPU. Throughput attribution: a backend's gateway token rate
(the Dashboard's per-backend throughput figure) counts only while
the backend has a live supervised process; a single-GPU placement
attributes the full rate, a split placement pro-rates by the
tensor-split shares. Points are decimated to at most 600 per GPU
by consecutive-group averaging; `interval_ms` is the nominal
spacing of the returned points, and a gap in time between points
is an interval with no recording (the app was off, or the GPU was
not reported) — never an interpolated value.

### 4.8 Log page (`get_logs`)

```json
{
  "lines": [ { "ts": "…", "stream": "stdout", "line": "…",
               "source": "coder", "archived": false } ],
  "total_matching": 1234, "total": 1234, "truncated": true,
  "scopes": [ { "id": "coder", "previous": true },
              { "id": "gateway", "previous": false } ]
}
```


## 5. Endpoint index

151 implemented client-facing endpoints: 132 registry operation
routes + 19 derived/API-layer routes (§6.17). A further 24
auth/admin routes (§6.17) bring the live surface to 175 route
records. ("Op" = returns `202` + operation, §3.) Specified but not
implemented: the gateway master key (decision 4) and raw
`PATCH /config` (decision 11) — §6.16 accounts for every registry
operation with no route.

| Domain | Count | Endpoints |
|---|---|---|
| Server / status | 2 | `GET /server` (public identity), `GET /status` |
| Backends | 31 | list, create, get, delete, add preview, cache preview, config get/patch, diff preview, start/stop/restart (Op), test, tool-call test, flags, offload, offload override set/clear, mtp, parallel, cache, moe cache, raw flags, flags mode, group membership, launch command; measure, estimate, baseline (Op), tuning preview, tuning run (Op) |
| Models | 13 | list, create, get, delete, patch, download (Op) + pause/resume/cancel, verify (Op), assign; lookup, header |
| Placement | 2 | propose, apply |
| Gateway | 15 | status, config get/patch, generated config, generated routing, start (Op), restart (Op), stop (Op), database stop (Op), groups list + CRUD (4), routing test, routing recent — plus master key specified, NOT IMPLEMENTED (§6.5) |
| Log receiver | 4 | status, start, stop, restart (Op) (§6.5a) |
| Activation plan | 5 | get, apply (Op), quiesce-wait, cancel, execution history |
| Wizard / setup | 11 | steps, status, prerequisites, run (Op), retry, skip, mark-manual, commands, help URL, complete, audit (Op) |
| Logs | 2 | query, scopes |
| Telemetry | 8 | GPUs, history, interval, reset peaks, snapshots save/list/get/delete |
| Operations | 3 | list, get, abort |
| Diagnostics | 2 | create bundle (Op), download bundle |
| Events | 1 | `GET /events` (SSE stream, §6.17) |
| Config / settings | 2 | get, supervision mode — plus raw patch specified, NOT IMPLEMENTED (§6.12) |
| MXC | 6 | policy get/put, mode, self-test, activity, tightening proposals |
| Measurements | 2 | lookup (§6.18), delete (§6.18) |
| Updates | 12 | list, components, check, apply, pin, auto; app update status, check, checking, auto, download, restart |
| MCP | 3 | servers, preflight, connection test (§6.15) |
| Optimize | 15 | state; purpose; plan validate, item decision, item outcome, apply (Op), discard; experiment decision, run (Op), estimate (Op); consult (Op) + cancel, discover (Op), job cancel, tuning adopt (§6.19) |
| Bench | 9 | prompts list/save/delete, generate (Op) + cancel, run (Op) + cancel, results list/delete (§6.20) |
| Notifications and services | 3 | notification settings, test notification, Windows services list (§6.21) |
| VS Code, WinML | 0 | Excluded — §6.16 |

## 6. Endpoint reference

### 6.1 Status

#### `GET /api/v1/server` — public (API-layer route, no backing operation)
The deployment identity document (PLAN.md §3.8). No authentication
required. Response:

```json
{ "deployment_id": "…", "app": "llmdil", "app_version": "0.1.40",
  "api_version": "1",
  "features": { "operations": true, "events_sse": true,
                "auth": true } }
```

#### `GET /api/v1/status` — `read`
Maps to `get_status`. Response:

```json
{ "status": "ok", "version": "0.1.32",
  "backends_configured": 5, "gateway_port": 4000,
  "winml_enabled": false, "mxc_mode": "learning" }
```

### 6.2 Backends

#### `GET /api/v1/backends` — `read`
`list_backends` → `{ "backends": [ Backend, … ] }` (§4.1).

#### `GET /api/v1/backends/{id}` — `read`
The one row of `list_backends` with that id; `404` when absent. (The app
has no get-one command; the server filters the list — same data, no new
behavior.)

#### `POST /api/v1/backends` — `config`
`backend_add`. Body:

```json
{ "id": "coder-fast", "model_id": "…", "port": 8083,
  "group": "coder", "create_group": false, "enabled": true }
```

→ `201` `{ "backend_id": "coder-fast", "group": "coder" }`. The backend
is created with the app's seeded defaults; the gateway routes it after
its next restart, and the creation records an activation-plan change,
as in the app. Optional body fields carry the launch parameters the
app's add form offers: `n_ctx`, `n_batch` (validated as in
`backend_update_flags`), `offload_layers` with `offload_override`
(a guardrail-blocked position is refused unless the override is set,
and the override is journaled, as in `backend_set_offload`), and
`gpu_indices` (an explicit GPU selection, written as an even tensor
split, replacing the telemetry assignment), and `parallel` (a slot
count written into the created backend's raw flags as `--parallel N`,
validated as in `backend_set_parallel`). Each omitted field keeps
its creation default. `cache_k`, `cache_v` and `flash_attn` set the
cache types as in `PUT …/cache`. The response also carries `persisted`.

#### `POST /api/v1/backends/preview` — `read` — synchronous
`backend_add_preview`. Body: `{ "model_id", "gpu_indices"?, "n_ctx"?,
"parallel"?, "offload_layers"? }`. What `POST /backends` with these
values would carry; nothing is written. → `{ model_id, offload,
cache, tensor_split, split_mode, memory }`: `offload` as on the
backend row (null without expert offload), `cache` as below, `memory`
the requirement with `per_gpu[]` (`gpu`, `share`, `required_mib`,
`total_mib`, free-memory verdict) and `fits_now`. `offload_layers`
prices an explicit MoE position instead of the model's default; a
guardrail-blocked position is priced and marked, not refused; a
position past the model's layers is `400`, as is any position on a
model without expert offload.

#### `POST /api/v1/backends/{id}/cache/preview` — `read` — synchronous
`backend_cache_preview`. Body (all optional; absent = saved value):
`{ "n_ctx", "slots", "cache_k", "cache_v", "flash_attn",
"moe_cache_mib", "offload_layers", "gpu_indices" }`. → predicted
footprint (`weights_mib`, `sys_weights_mib`, `kv_mib`, `rs_mib`,
`moe_cache_mib`, `total_mib`), `targets[]` per GPU, `ladder[]`,
`best`, `fits_current`, `fits_now`. `offload_layers` and
`gpu_indices` price an unsaved position and placement (an even split
over the named GPUs); bounds as above.

#### `DELETE /api/v1/backends/{id}` — `destructive`
`backend_delete` → `{ "backend_id", "deleted": true }`. Refused
(`400 INVALID_INPUT`) while the backend is running, as in the app.

#### `POST /api/v1/backends/{id}/start` — `control` — **Op**
#### `POST /api/v1/backends/{id}/stop` — `control` — **Op**
#### `POST /api/v1/backends/{id}/restart` — `control` — **Op**
`start_backend` / `stop_backend` / `restart_backend`. Body (stop,
restart): `{ "graceful": true }` (required); every one also takes an
optional `origin` (§3.1). Start and restart take an optional
`wait_ready_secs`: the operation then ends only when the backend answers
`/health` (`detail.ready: true`), and fails with `SUPERVISOR` when the
time runs out (the backend keeps loading) or the process goes away. The
app commands already return `{ task_id, backend_id }`; the REST layer
returns `202` with the operation for that task. Start runs the app's
preflight (port, model file, placement) and fails the operation with the
preflight message.

#### `POST /api/v1/backends/{id}/test` — `control` — synchronous
`test_backend`. Body: `{ "prompt": "…" }`. One inference request
against the running backend. Response (`BackendTestResult`):

```json
{ "status": 200, "content": "…", "reasoning_content": null,
  "prompt_tokens": 12, "completion_tokens": 16, "latency_ms": 1823 }
```

The request asks for no thinking, temperature 0 and at most 16 tokens.
`reasoning_content` is set when the model reasoned anyway; a 2xx with
either text means the backend is generating.

A non-2xx from the backend is reported in `status`, not as an API
error, mirroring the app. Duration is model-bound, at most 30 s; other
calls, saves included, are not held up. This is the one synchronous control call; it is
kept synchronous because the app has no task wrapper for it and its
result is the response body itself.

#### `POST /api/v1/backends/{id}/test-tool-call` — `control` — synchronous
`backend_test_tool_call`. No body. Sends one real tool-call request
to the backend and saves the outcome as its `tool_call_probe`. →
`{ outcome (verified | unparsed | failed), finish_reason,
chat_template_tool_use, chat_template_caps, detail, latency_ms }`. A
backend that is not running is `failed`, not an error.

#### `POST /api/v1/backends/{backend_id}/measure` — `control` — **Op**
#### `POST /api/v1/backends/{backend_id}/estimate` — `control` — **Op**
#### `POST /api/v1/backends/{backend_id}/baseline` — `control` — **Op**
`optimize_measure_backend` (full profile), `optimize_estimate_backend`
(abbreviated), `optimize_run_baseline` (screening evaluation). The
backend must be running. One job at a time; a consultation on the
same backend refuses it (a failed operation naming the running one).
The detail is the Optimize state (§6.19); results land in
`measurements` and on the backend row's `predicted`.

#### `GET /api/v1/backends/{backend_id}/tuning` — `read`
#### `POST /api/v1/backends/{backend_id}/tuning/run` — `control` — **Op**
`optimize_tuning_preview` → `{ backend_id, model_id, incumbent, plan }`;
`optimize_tuning_run` runs the planned arms on temporary backends,
stopping the backends in the way and starting them again at the end.
The sweep is in the Optimize state's `tuning`; its winner is adopted
with `POST /optimize/tuning/{sweep_id}/adopt` (§6.19).

#### `GET /api/v1/backends/{id}/config` — `read`
`backend_get_config` → §4.2.

#### `PATCH /api/v1/backends/{id}/config` — `config`
`backend_apply_config`. Body:

```json
{ "patch": { "n_ctx": 65536, "tensor_split": [0.5, 0.5] },
  "apply_mode": "RestartNow | OnNextRestart" }
```

→ `{ "backend_id", "diff": ConfigDiff, "apply_mode",
"restart_task_id": "…" | null, "persisted": true }`. With
`RestartNow` the save is followed by the normal graceful restart;
`restart_task_id` names the operation the client polls, which ends
`done` (process spawned) or `failed` (the response itself stays `200`:
the config save is complete; only the restart is asynchronous). While
the backend is not running, `RestartNow` is `400` and saves nothing. The
save records activation-plan
changes exactly as the app does.

#### `POST /api/v1/backends/{id}/config/preview-diff` — `read`
`backend_preview_diff`. Body: `{ "patch": { … } }` → `ConfigDiff`
(§4.2). No state changes.

#### `PUT /api/v1/backends/{id}/flags` — `config`
`backend_update_flags`. Body: `{ "n_ctx": 32768, "n_batch": 512 }` →
`{ "backend_id", "persisted": true }`. Applies at the backend's next
start; the app records the activation-plan entry.

#### `PUT /api/v1/backends/{id}/offload` — `config`
`backend_set_offload`. Body: `{ "layers": 40 }`, with optional
`"allow_override": true` to accept a guardrail-blocked position
(the override is journaled, as in the app) →
`{ "backend_id", "layers": 40, "raw_flags": "…", "changed": true }`.
Only valid for offload-capable backends (the `offload` block in §4.1
says which); otherwise `400 INVALID_INPUT`, as in the app.

#### `PUT /api/v1/backends/{id}/offload-override` — `config`
`backend_set_offload_override`. Body: `{ "layers": 46, "placed_ccs":
["6.0"] }` → `{ "backend_id", "layers": 46, "moe_layers": 48,
"placed_ccs": ["6.0"], "has_override": true, "overridden": true,
"persisted": true }`. Records a deliberate guardrail override for
the backend's SAVED offload position — the activation-journal
record the offload guardrails define (docs/offload-guardrails.md),
keyed to exactly (backend, layers, placed compute-capability set).
The body names the key the caller confirmed (the saved `layers`
and the guardrail's `placed_ccs` from §4.1); the server re-derives
the key from the live config and placement and refuses any
mismatch with `400 INVALID_INPUT`, as it refuses a position that
is not guardrail-blocked — an override covers exactly the position
it was granted for and never widens. No config or flags change;
starts proceed under the override from now on.

#### `DELETE /api/v1/backends/{id}/offload-override` — `config`
`backend_clear_offload_override`. No body → `{ "backend_id",
"cleared": true, "layers": 46, "moe_layers": 48, "placed_ccs":
["6.0"], "has_override": false, "overridden": false, "persisted":
true }`. Drops the backend's standing guardrail override; the
saved position is blocked again at launch preflight. Idempotent:
clearing with no record standing succeeds with `"cleared": false`.

#### `PUT /api/v1/backends/{id}/mtp` — `config`
`backend_set_mtp`. Body: `{ "n_max": 2 }` →
`{ "backend_id", "n_max": 2, "raw_flags": "…", "changed": true }`.
Sets the backend's MTP speculative decoding draft length
(`--spec-type draft-mtp` with that `--spec-draft-n-max`, 1–4);
`{ "n_max": null }` turns MTP off. Only the MTP tokens in the
backend's raw flags change. Only valid for MTP-capable backends
(the `mtp` block in §4.1 says which); otherwise
`400 INVALID_INPUT`, as in the app.

#### `PUT /api/v1/backends/{id}/moe-cache` — `config`
`backend_set_moe_cache`. Body: `{ "mib": 4096 }` →
`{ "backend_id", "mib": 4096, "auto": false, "resolved_mib": null,
"raw_flags": "…", "changed": true }`. Sets the backend's MoE expert
cache (`--moe-cache-mib`): llama.cpp keeps the most recently used
experts of the offloaded layers in GPU memory and computes them
there. `{ "mib": null }` turns the cache off; `{ "auto": true }`
(with `mib` null) resolves the size from priced free VRAM — the
telemetry free space less other backends' accounted residency and
this backend's own requirement, less a standing reserve — and the
response's `resolved_mib` (and `mib`) carry the saved value. Only
the cache token in the backend's raw flags changes. Only valid
for offload-capable backends (the `moe_cache` block in §4.1 says
which); a nonzero size additionally requires a saved offload above
0 and a non-row split, otherwise `400 INVALID_INPUT`, as in the
app. The size is priced into the backend's memory requirement, so
a start whose cache cannot fit is refused with the numbers.
Applies at the backend's next start; the app records the
activation-plan entry.

#### `PUT /api/v1/backends/{id}/parallel` — `config`
`backend_set_parallel`. Body: `{ "slots": 1 }` →
`{ "backend_id", "slots": 1, "raw_flags": "…", "changed": true }`.
Sets the backend's parallel slot count (the canonical
`--parallel N`, 1–1024); `{ "slots": null }` removes the token,
returning the backend to llama.cpp's default (auto). Only the
parallel tokens in the backend's raw flags change. Applies at
the backend's next start; the app records the activation-plan
entry.

#### `PUT /api/v1/backends/{id}/raw-flags` — `config`
`backend_set_raw_flags`. Body: `{ "raw_flags": "--parallel 1 …" }`
(or `null` to clear) →
`{ "backend_id", "raw_flags": "…", "changed": true, "warnings": [ { "token", "message" }, … ] }`.
The advanced flags editor's save (tracker #60): the backend's raw
flags edited directly as text. Only valid while the backend is in
advanced flags mode (see `flags-mode` below); in structured mode
the token-owning writes own the string and this refuses with
`400 INVALID_INPUT`. Validation warnings ride the response and
never block the save. Applies at the backend's next start; the app
records the activation-plan entry.

#### `PUT /api/v1/backends/{id}/flags-mode` — `config`
`backend_set_flags_mode`. Body: `{ "advanced": true }` →
`{ "backend_id", "flags_advanced": true, "raw_flags": "…", "changed": true }`.
Turns a backend's advanced flags mode on or off (tracker #60). The
flip writes only the mode: the backend's flags do not change by a
single token in either direction. While the mode is on, the
structured flags writes (`flags`, `offload`, `mtp`, placement
apply, and the flags fields of `config` patches) refuse with
`400 INVALID_INPUT` naming the mode.

#### `PUT /api/v1/backends/{id}/group` — `config`
`gateway_set_backend_group`. Body: `{ "group": "coder" }`, or
`{ "group": null }` to ungroup → `{ "backend_id", "group",
"removed_from": [ … ], "changed", "persisted" }`. Move
semantics (predicted-throughput design §8.5): the backend's id
is removed from every group's member list and appended to the
target's — a backend is in at most one group, and a hand-edited
multi-group membership collapses, with every group it left
named in `removed_from`. Setting the group the backend already
solely occupies (or ungrouping an ungrouped backend) succeeds
with `changed: false` and no config write. The change persists
immediately and is journaled for the Activation Plan like every
other membership change; it takes effect at the next gateway
activation. `404` for an unknown backend or group; `400` for an
empty group name (pass `null` to ungroup).

#### `GET /api/v1/backends/{id}/launch-command` — `read`
`backend_get_launch_command` → `{ "id", "argv": [ … ] }` — the exact
argv the supervisor will run, including the CUDA visibility
restriction. Read-only; useful for support.

### 6.3 Models

#### `GET /api/v1/models` — `read`
`list_models` → `{ "models": [ Model, … ] }` (§4.3).

#### `GET /api/v1/models/{id}` — `read`
One row of `list_models`; `404` when absent. This is also the
download-progress channel (§3.1).

#### `POST /api/v1/models` — `config`
`model_add`. Body:

```json
{ "id": "my-model", "source_url": "https://…/model.gguf",
  "file_name": "model.gguf", "quant": "Q4_K_M", "params_b": null,
  "moe_layers": null, "default_offload_layers": null, "mtp": null,
  "sha256": null }
```

`file_name`, `quant`, `params_b` optional (file name defaults to the
URL's last segment). When `params_b` is omitted or non-positive, the
count resolves from the curated catalog (exact id match, then exact
source-URL match); when no source yields a count the call fails with
a validation error naming the missing count — a model is never
registered with an unknown or placeholder parameter count.
`moe_layers` and `default_offload_layers` are the operator MoE
declaration — the pair that gives a model the curated catalog does
not carry expert-offload controls (the creation default for its
backends and the Edit Flags slider's green limit resolve from it;
catalog pins win where the catalog carries them). Both or neither:
the layer count must be positive and the default must not exceed
it. `mtp` is the operator MTP capability declaration — the fact
that gives a model the catalog does not carry the MTP draft
control (backend Add/Edit and Edit Flags resolve the EFFECTIVE
capability: the catalog's pin where the catalog knows the model,
else this declaration); omitted or null leaves it undeclared.
`sha256` is an optional hash pin: a full 64-character hex SHA-256
the record starts pinned with (the desktop Add Model form passes
a Hugging Face–picked single file's `lfs.sha256`); a malformed
value is refused with a validation error. →
`201` `{ "model_id" }`. The model starts
enabled; with no pin supplied it starts with no pinned hash
(unverified until a pin exists — the app
treats a missing pin as "no verification possible", never as verified).

#### `POST /api/v1/models/lookup` — `read` — synchronous
`model_hf_lookup`. Body: `{ "query", "suggest"? }`. Asks Hugging Face.
With `suggest: true` → `{ suggest, repos: [{ repo_id, downloads }] }`;
otherwise full rows with each quantized file's `size_bytes`,
`sha256`, `source_url` and fit against free GPU memory. An empty
query, and a search that failed, are `400`.

#### `POST /api/v1/models/header` — `read` — synchronous
`model_hf_header`. Body: `{ "repo_id", "file" }`. → `{ repo_id, file,
catalog, sha256, header }`: the file's MoE geometry, MTP signal and
suggested default offload, or `header: null` when it cannot be read.

#### `DELETE /api/v1/models/{id}` — `destructive`
`model_delete` → `{ "model_id", "deleted": true, "bytes_freed": n }`.
Deletes the file and any partial; refused while a download is in
flight or any backend references the model.

#### `PATCH /api/v1/models/{id}` — `config`
Body: any of `{ "display_name": "Coder" }` (`model_set_short_name` —
the operator's persistent rename; empty string clears it),
`{ "enabled": false }` (`model_set_enabled` — excludes the model
from wizard downloads; file and config are kept), the MoE
offload declaration `{ "moe_layers": 48, "default_offload_layers": 16 }`
(`model_set_offload_spec` — the pair is applied atomically: when
either key is present both are read, an absent key counts as null,
and both null clears the declaration), and the MTP capability
declaration `{ "mtp": true }` (`model_set_mtp` — a boolean or
null; null clears the declaration). →
`{ "id", "name", "enabled", "moe_layers", "default_offload_layers", "mtp" }`
reflecting the applied fields.

#### `POST /api/v1/models/{id}/download` — `control` — **Op**
`download_model` → `202` operation, which runs until the transfer and
its verification ended (detail: `{ "model_id", "dest", "task_id",
"error", "verified" }`); its abort cancels the transfer and removes the
partial file. The transfer runs in the app's
background worker and resumes from any partial file. Progress: poll
`GET /models/{id}` (`downloading`, `partial_bytes`, then `verifying`,
then `verified_at` set or `download_error` set — a hash mismatch
deletes the file and lands in `download_error`). `409 BUSY` when a
download for this model is already in flight.

#### `POST /api/v1/models/{id}/download/pause` — `control`
`model_pause_download` → `{ "model_id", "paused": true }`. Partial
file kept.

#### `POST /api/v1/models/{id}/download/resume` — `control` — **Op**
`model_resume_download` → `202` operation, as download: it follows the
resumed transfer to its end. `404` when none is in flight.

#### `POST /api/v1/models/{id}/download/cancel` — `destructive`
`cancel_download` → `{ "model_id", "cancelled": true }`. The partial
file is discarded. Errors when no download is in flight.

#### `POST /api/v1/models/{id}/verify` — `control` — **Op**
`model_verify_checksum` (`400` without a pin). SHA-256 of the on-disk file against the
pinned hash. Minutes-scale on multi-GiB files (~17 min for 46 GiB on
the reference machine), hence an operation; its completion detail is
`{ "id", "ok": true|false }`.

#### `POST /api/v1/models/{id}/assign` — `config`
`model_assign`. Body: `{ "backend_id": "coder", "split": [0.5, 0.5] }`
→ `{ "task_id", "persisted": true }` (the app's own result shape; the
named task is the app's bookkeeping task, surfaced as an operation
reference for symmetry). Sets the backend's model file and tensor
split; the split is validated against the discovered GPU count.

### 6.4 Placement

#### `POST /api/v1/placement/propose` — `read` — synchronous
`placement_propose`. Body:
`{ "model_id": "…", "split": [0.5, 0.5], "backend_id": "coder"? }` →
the projection (§4-style payload from the app): `per_gpu_mib`,
`free_mib`, `current_mib`, `additional_mib`, `per_gpu_fits`, `fits`,
`snapshot_age_ms`, `gpu_indices`, `down_indices`, `sys_mib`,
`gpu_weights_mib`, `weights_only` (false when a backend is named). Pure evaluation against the
latest telemetry snapshot; changes nothing. Fails with `TELEMETRY`
when no telemetry exists yet.

#### `POST /api/v1/placement/apply` — `config`
`placement_apply`. Same body → `{ "task_id", "backends": [ … ],
"persisted": true }`. Writes the split to the backends serving the
model (or the one named backend); takes effect at their next restart
and records the activation-plan entries.

### 6.5 Gateway

#### `GET /api/v1/gateway/status` — `read`
`gateway_status` → §4.5.

#### `GET /api/v1/gateway/config` — `read`
`get_gateway_config` → §4.4.

#### `PATCH /api/v1/gateway/config` — `config`
`update_gateway_config` for all fields except `bind_host`, which maps
to `gateway_set_bind_host` (the app keeps it a separate command
because it always requires a gateway restart and reconciles the
firewall rule at the next start). Body is a JSON merge patch over the
§4.4 shape, e.g.:

```json
{ "groups": [ … ], "openrouter": { "enabled": true },
  "bind_host": "127.0.0.1" }
```

→ `{ "ok": true, "persisted": true }` (plus
`"restart_required": true` when `bind_host` changed). Group
membership and routing changes record activation-plan entries, as in
the app; their summary names what changed (each group by name) and
`targets` the backends concerned. The patch is closed: an unknown or
read-only field (`effective_strategy`, `group_strategies`), a wrong type
or an invalid value is `400` and saves nothing, `bind_host` included.
The computed `enabled`/`enabled_members` of a group may be sent back and
are ignored. The granular group endpoints below are conveniences over
this patch.

#### `GET /api/v1/gateway/groups` — `read`
The `groups` array of `GET /gateway/config`.

#### `POST /api/v1/gateway/groups` — `config`
#### `PATCH /api/v1/gateway/groups/{name}` — `config`
#### `DELETE /api/v1/gateway/groups/{name}` — `destructive`
Create, patch (`members`, `strategy`, `fallbacks`, `purpose`), or
remove one group by name. `name` is the group's identity (groups have
no separate id in the app); renaming a group is delete + create.
The rules are the app's own: a name is lowercase letters, digits and
hyphens, at most 48 characters, not `openrouter` and not taken (`400`);
`DELETE` refuses a group with members (`400`) unless `?cascade=true`,
which removes the group with its membership. `POST` with a name alone
and `DELETE` without a cascade run `gateway_create_group` and
`gateway_delete_group`; the other forms are read-modify-write over
`update_gateway_config`. Unlike `PUT /backends/{id}/group` (§6.2), a
`members` list does not move a backend out of its other groups.

#### `GET /api/v1/gateway/generated-config` — `read`
`gateway_preview_yaml` → `200`, `Content-Type: text/yaml`, body the
generated LiteLLM `config.yaml` exactly as the gateway will read it
at its next start. (The desktop returns it wrapped as
`{ "yaml": "…" }`; the API returns the document itself.)

#### `GET /api/v1/gateway/generated-routing` — `read`
The second generated file, `llmdil_routing.py` (`routing_py` of
`gateway_preview_yaml`) → `200`, `Content-Type: text/x-python`. `404`
when no routing rule or overflow setting needs one.

#### `GET /api/v1/gateway/master-key` — `secret` — NOT IMPLEMENTED in v1 (decision 4: the master key is never mirrored over REST; the operation is routeless)
`gateway_master_key` → `{ "master_key": "…" }`. The LiteLLM master
key: the API key local clients present to the gateway and the admin
UI password. **Highest-sensitivity endpoint in the API** — the only
`secret`-class endpoint; the roles plan should gate it separately
from general read. Note the app command generates and stores the key
in Windows Credential Manager on first use, so this GET can have a
one-time side effect.

#### `POST /api/v1/gateway/start` — `control` — **Op** (authorization class D, inherited from the start-gateway step it aliases)
The app has no standalone gateway-start command: starting is the
wizard's `start-gateway` step. This endpoint is
`POST /setup/steps/start-gateway/run` under a domain name (same
operation). It enforces the prisma pin, runs `prisma generate`,
probes the database, spawns with the health-watchdog flag, and
confirms sustained health — minutes-scale, hence an operation. The
operation detail carries the step outcome, including
`manual_command` on failure. A step that ends `failed` or `manual`,
or a run refused because another gateway operation is in flight,
is a `failed` operation (§3.1). The step stops the running gateway
itself just before it spawns the new one, and adopts a running
PostgreSQL server rather than restarting it.

#### `POST /api/v1/gateway/restart` — `control` — **Op**
`gateway_restart`. Body (all optional): `{ "drain": true,
"origin": "api" }`. One server-side restart, shared with the desktop
Gateway view and the activation apply: it takes the gateway's
`restarting` transition (another gateway operation in flight fails
the operation with `Busy`), optionally drains in-flight requests
(up to 120 s; a timeout restarts anyway and is reported in
`quiesce`), and runs the start-gateway step under that transition.
**PostgreSQL is not stopped or restarted** (only a PostgreSQL port
change restarts it, inside the step). While it runs,
`GET /gateway/status` reads `state: "restarting"`. The gateway's own
log (`gateway` scope) carries `[lifecycle]` markers: restart
requested (by whom), PostgreSQL kept running, drain, stopped, and
restarted or restart failed. Completion detail: `{ ok, task_id,
outcome, message, manual_command, activation_pending_before,
quiesce? }`; `ok: false` fails the operation (§3.1).

#### `POST /api/v1/gateway/stop` — `control` — **Op**
`gateway_stop`. Body (all optional): `{ "stop_postgres": false,
"origin": "api" }`. Stops **the gateway only** by default — PostgreSQL
keeps running. With `stop_postgres: true` the app-managed PostgreSQL
server is stopped afterwards, under its own lifecycle transition and
markers. The operator's intent (stopped) is recorded before the kill,
so the stop is never reported as a crash. Process mode finds the
gateway by its pid file, else by the port owner, stops it gracefully
and confirms the exit and the port release; service mode stops the
Windows service and waits for it to reach Stopped. Completion detail:
`{ "ok": true, "stopped": true|false, "pid": 123|null, "found_by":
"pid_file"|"port_owner"|"service"|null, "postgres_stopped": bool }`
(`stopped: false` means it was not running). An operation since the
service-lifecycle work (it was synchronous before, which the CLI
could not parse).

#### `POST /api/v1/gateway/database/stop` — `control` — **Op**
`postgres_stop`. Body (optional): `{ "origin": "api" }`. Stops the
app-managed PostgreSQL server (`pg_ctl stop -m fast`) under its own
`stopping` transition, with markers in the `postgres` lifecycle log.
Refused (`INVALID_INPUT`, the operation fails) while the gateway is
running or has an operation in flight: stop the gateway first, or use
`POST /gateway/stop` with `stop_postgres: true`. Completion detail:
`{ "ok": true, "stopped": true|false }`.

#### `POST /api/v1/gateway/routing/test` — `read`
`gateway_test_routing`. Body: `{ "group": "planner" }` (the app's
request object; `group` is required, a string) → the routing decision trace.
Dry-run: the decision is recorded in the session routing log, as in
the app; no live request is sent.

#### `GET /api/v1/gateway/routing/recent` — `read`
`gateway_recent_requests` → `{ "requests": [ … ] }`, newest first —
the routing-test traces recorded this session. Live requests are not
logged (the app's semantics, kept).

Routing rules have no dedicated app commands: they are part of the
gateway config (§4.4) and change only through `PATCH /gateway/config`.
`GET /gateway/config` is their read path. (See §7.)

### 6.5a Log receiver

#### `GET /api/v1/log-receiver` — `read`
`log_receiver_status` → `{ service_name, registered, state, lifecycle,
transition, exit_code, failure_reason, ingest_port, ingest_ok, pid,
receiver_present, ingest_token_seeded, read_token_seeded, endpoint,
endpoint_is_local, streaming_enabled, shipper }`. The two local paths
the window shows (`receiver_exe`, `data_dir`) are not in the API's
answer for any role.

#### `POST /api/v1/log-receiver/start` — `control` — **Op**
#### `POST /api/v1/log-receiver/stop` — `control` — **Op**
#### `POST /api/v1/log-receiver/restart` — `control` — **Op**
`log_receiver_start` / `log_receiver_stop` / `log_receiver_restart`.
Body (optional): `{ "origin": "api" }`. Unelevated SCM control of the
deployed receiver service (its registration grants the operator
start/stop): stop waits for Stopped, start waits for Running and then
up to 10 s for the ingest port (127.0.0.1:8473) to accept connections,
and restart does both inside one `restarting` transition, so no
re-deploy (and no UAC prompt) is needed. Markers go to the receiver's
`receiver.lifecycle.log`. Refused (`INVALID_INPUT`) when the receiver
is not deployed; a second operation while one is in flight fails with
`Busy`; a start that falls back to Stopped fails with the service
host's exit-code reason. Completion detail: `{ ok, service_name,
action, detail, ingest_ok (null for stop), lifecycle, pid }`, where
`lifecycle` is `running`, `degraded` (running but the ingest port
refuses connections), `stopped` or `failed`.

### 6.6 Activation plan

#### `GET /api/v1/activation-plan` — `read`
`gateway_activation_plan` → §4.6.

#### `POST /api/v1/activation-plan/apply` — `control` — **Op**
Body: `{ "mode": "drain" | "now" }`.

- `now`: restart each backend in `backend_restart_ids` (each through
  the shared backend restart, waiting for readiness), then (when
  `gateway_pending`) restart the gateway through the shared gateway
  restart (`POST /gateway/restart` semantics — PostgreSQL stays up).
- `drain`: the same, preceded by the quiesce wait (in-flight gateway
  requests drain to zero, bounded at 120 s; on timeout the apply
  proceeds and the operation detail reports the in-flight count it
  proceeded with).

→ `202` operation. The route delegates to the app's single
server-side apply (`activation_apply`, the same op the desktop
calls) with `source: "api"`:

- The plan is re-read when the apply starts; steps run in order and
  stop at the first backend failure (completed restarts are not
  rolled back).
- **Failure semantics:** the operation is `failed` when any item
  failed — a backend restart, or a gateway restart that was refused,
  failed or needs a manual step (§3.1). The gateway side of the plan
  is completed only after a successful gateway start, so a failed
  apply never reports pending changes as live. The `error` is the
  apply's `message`; the `detail` keeps the full payload, including
  `manual_command` when the gateway step attached one.
- Every apply that ran something is recorded once in the activation
  history with `source: "api"` (`execution_id` in the detail), with
  status `completed`, `completed_with_failures` (a gateway failure)
  or `failed` (a backend abort).
- A second apply while one is running is `409 BUSY`, naming the
  running apply's source, step and elapsed time.
- Nothing pending: `{ ok: true, applied: true, status:
  "nothing_pending" }`; nothing runs and nothing is recorded.

Completion detail: `{ ok, applied (alias of ok, for older
clients), status, message, mode, source, restarted_backends,
gateway_restarted, execution_id, items, manual_command?, quiesce? }`.
While the apply runs, `GET /activation-plan` and `GET /gateway/status`
carry `activation_applying`.

#### `POST /api/v1/activation-plan/quiesce-wait` — `control` — synchronous
`gateway_quiesce_wait`. Body: `{ "timeout_secs": 120 }` →
`{ "drained": true, "in_flight": 0, "timed_out": false }`. Bounded
by the caller's timeout (clamped to 5–600 s) — the documented exception to
the 30 s synchronous guideline.

#### `POST /api/v1/activation-plan/cancel` — `destructive`
`gateway_activation_cancel` → `{ "cancelled": true }` or
`{ "cancelled": false, "reason": "…" }`. Restores the pre-change
config snapshot and clears the plan; refused (error) when the config
has since changed through another path, as in the app.

`gateway_activation_complete` is **not** a client endpoint: it is
bookkeeping the server performs after a successful apply or gateway
(re)start. Listed here so the mapping is complete.

#### `GET /api/v1/activation-plan/history` — `read`
`gateway_activation_history`. The durable record of past activation
plan executions (an apply is recorded whole when it ends, including
an aborted one): `{ "executions": [ … ] }`, newest first, each entry
a summary — `id`, `started_at`, `finished_at`, `status`
(`completed` | `completed_with_failures` | `failed`), `source`,
`mode`, the plan state (`drift`, `gateway_up`, `gateway_pending`),
and the item counts (`items`, `applied`, `skipped`, `failed`).
With `?execution_id=<id>`, returns `{ "execution": … }` — that
execution's full record: the plan as computed at apply time, one
item per change (`ts`, `kind`, `summary`, `scope`, `targets`,
`activation`) with its `outcome` (`applied` | `skipped` |
`failed`) and `detail` (skip reason or error text) — or
`"execution": null` when the id has aged out of the retained
history (the newest 50 executions are kept). A missing or corrupt
history file reads as an empty history, never an error.

`gateway_activation_record_execution` is **not** a client endpoint:
it is the bookkeeping write the desktop apply funnel reports when an
apply ends, like `gateway_activation_complete`. Listed here so the
mapping is complete. `activation_apply` itself has no route of its
own: `POST /activation-plan/apply` is its REST form.

### 6.7 Wizard / setup

Step state (`WizardStepState`): `{ id, title, state, … }` as
`setup_get_steps` returns; step outcomes per §3.1.

#### `GET /api/v1/setup/steps` — `read`
`setup_get_steps` → `{ "steps": [ … ], "done": n, "total": n }`
(the app payload's progress fields).

#### `GET /api/v1/setup/status` — `read`
`setup_status` → `{ "setup_completed": bool, … }`.

#### `GET /api/v1/setup/prerequisites` — `read`
`setup_get_prerequisites` → the app's prerequisite report.

#### `POST /api/v1/setup/steps/{step_id}/run` — `control` — **Op**
`run_wizard_step`. → `202` operation (`kind: wizard_step`, detail
`{ "step_id" }`); the completion detail is the `WizardStepOutcome`
(`state`, `message`, `log_tail`, `duration_ms`,
`manual_command` when the step attaches one). `409 BUSY` when any
step is already running, with the error `details` naming the running
step and its elapsed seconds — the app's busy payload
(`{ busy, running_step, elapsed_secs }`) carried in the envelope.
**Elevation caveat: §7.**

#### `POST /api/v1/setup/steps/{step_id}/retry` — `control` — **Op**
`setup_retry_step`. Same operation semantics as run.

#### `POST /api/v1/setup/steps/{step_id}/skip` — `config`
`setup_skip_step`. Body: `{ "note": "…" }` → the step's new state.

#### `POST /api/v1/setup/steps/{step_id}/mark-manual` — `config`
`setup_mark_manual` → marks the step done-by-hand (the app's payload
fields).

#### `GET /api/v1/setup/steps/{step_id}/commands` — `read`
`setup_get_commands` → `[{ "…": "…" }]` — the exact commands the step
will run (or ran), for operator review before running a step
remotely.

#### `GET /api/v1/setup/steps/{step_id}/help-url` — `read`
`setup_step_help_url` → `{ "step_id", "url": "…" | null }`. The URL
is data; opening it is the client's business (§7).

#### `POST /api/v1/setup/complete` — `config`
`setup_mark_complete` → `{ "ok": true, "persisted": true }`.

#### `POST /api/v1/setup/audit` — `control` — **Op**
`run_audit` → `202` operation; completion detail is the audit
result: `{ "done", "total", "steps": [ … ] }`. `409 BUSY` while
another audit runs (also one started in the app).

### 6.8 Logs

#### `GET /api/v1/logs` — `read`
`get_logs`, semantics unchanged (§2.6):

| Param | Meaning |
|---|---|
| `scope` (required) | a process/scope id (`coder`, `planner`, …), `gateway`, `wizard`, or `all`; archived scope ids from `scopes` are accepted |
| `level` | heuristic filter: `error` \| `warn` \| `info` \| `debug`; anything else is `400` |
| `search` | case-insensitive substring over line text |
| `query` | case-insensitive substring over line text and source |
| `since_ms`, `until_ms` | epoch-ms bounds, inclusive; a line whose own time is unknown (`ts_estimated: true`) is kept when it can lie inside |
| `order` | `desc` (default, newest first) \| `asc` |
| `offset`, `limit` | paging; `limit` defaults to 200 |

→ §4.8.

#### `GET /api/v1/logs/scopes` — `read`
The `scopes` array of the logs payload (`[{ id, previous }]`).
Implemented by the server from the same sources as `get_logs`.

### 6.9 Telemetry

#### `GET /api/v1/telemetry/gpus` — `read`
`get_gpu_telemetry` → §4.7 (latest snapshot, with `stale` flags;
poll-based — there is no stream, §7).

#### `PUT /api/v1/telemetry/interval` — `config`
`telemetry_set_interval`. Body: `{ "ms": 2000 }` → `{ "ok": true,
"poll_interval_ms" }`. `ms` is 0 (off) or 500 to 60000, else `400`.
Persists immediately; the running poller's cadence changes at app
restart (the app's semantics, kept and documented).

#### `POST /api/v1/telemetry/reset-peaks` — `control`
`telemetry_reset_peaks` → `{ "ok": true }`.

#### `POST /api/v1/telemetry/snapshots` — `control`
`telemetry_save_snapshot` → `{ "id", "created_ms", "path" }`. The
snapshot is a file on the managed machine; its path is metadata, shown
whole to administrators only (§3.1).

#### `GET /api/v1/telemetry/snapshots` — `read`
#### `GET /api/v1/telemetry/snapshots/{id}` — `read`
#### `DELETE /api/v1/telemetry/snapshots/{id}` — `destructive`
The saved snapshots, newest first: `{ "snapshots": [{ "id",
"created_ms", "size_bytes" }] }`; one of them: `{ "id", "created_ms",
"samples" }`; and its deletion: `{ "id", "deleted": true }`. No paths;
an id the list does not name is `404`.

#### `GET /api/v1/telemetry/history/{range}` — `read`
`get_gpu_history` → §4.8. `range` is `1h`, `24h`, or `7d` (any
other value fails with `INVALID_INPUT`); the path placeholder
merges into the argument object by name, like every route
argument.

### 6.10 Operations

#### `GET /api/v1/operations` — `read`
`tasks_list` merged with the `202` records → `[ Operation, … ]`
(§3.1), a bare array, newest first.

#### `GET /api/v1/operations/{id}` — `read`
One operation, by its `202` id or its app task id. `404` when absent.

#### `POST /api/v1/operations/{task_id}/abort` — `control`
`task_abort` (the registry route template names the placeholder
`task_id`; a `202` id is as good) → `{ "ok": true, "task_id",
"status", … }`, the operation's resulting state. The abort of a `202`
record cancels the app task it runs as and waits (about 10 s) for the
work to stop: `stopped: true`. Work without a task, or that does not
stop, is marked `aborted` with `forced: true` and an `abort_note` in its
detail; it may still be running, and what it did is kept as
`late_outcome`. An already-terminal operation is not changed: its
`status` with `already_finished: true`.

### 6.11 Diagnostics

#### `POST /api/v1/diagnostics/bundles` — `control` — **Op**
`diagnostics_export_bundle`. Builds the zip on the managed machine
(config snapshot, per-scope recent logs, telemetry snapshot,
versions file). The operation's completion detail carries
`{ "bundle_id", "path" }` (path is host-local metadata).

#### `GET /api/v1/diagnostics/bundles/{bundle_id}/content` — `read`
`200`, `Content-Type: application/zip`, the bundle bytes.
**Adapted mapping (§7):** the app command only writes a file and
returns its path; the REST server additionally serves the bytes so
a remote client can actually retrieve the bundle. Bundle ids are
server-assigned at creation and retained for the process lifetime.

### 6.12 Config / settings

#### `GET /api/v1/config` — `read`
`get_config` → the full `AppConfig` JSON plus `config_path`:
`version`, `data_dir`, `view_modes`, `backends`, `gateway`,
`models`, `telemetry`, `mxc`, `winml`, `postgres` (port only — the
password lives in Credential Manager and never appears), `updates`,
`notifications`, `setup_completed`, `catalog_seeded`,
`defaults_repaired`, `catalog_revision`. Secrets are not part of
this payload; the one credential-adjacent field is the OpenRouter
`cred_ref` (a Credential Manager target *name*, not a value).

#### `PATCH /api/v1/config` — `config` — NOT IMPLEMENTED in v1 (decision 11: raw config writes are excluded; section-scoped writes go through their domain endpoints)
`update_config`. Body: a JSON merge patch over the §6.12 shape. The
server merges, validates, and persists atomically, installing the
new config in memory only when the save succeeds — the app's exact
semantics. → `{ "ok": true, "persisted": true }`; validation failure
→ `422 CONFIG` with memory and disk unchanged. This is the broadest
write surface in the API (it can reach every config section,
including the dead `notifications` field); the domain endpoints
above are the preferred path, and the roles plan should treat raw
config patch as its own capability.

#### `PUT /api/v1/settings/supervision` — `config`
`settings_set_supervision`. Body: `{ "mode": "process" | "service" }`.
→ `{ "ok", "mode", "persisted" }`. Saves which supervision the next
start, stop or restart uses; nothing is started, stopped or
registered (registering services needs the console, §6.16). The one
settings section without a domain endpoint of its own. `view_modes`
has no route: no screen reads it.

### 6.13 MXC

#### `GET /api/v1/mxc/policy` — `read`
`get_mxc_policy` →
`{ "policy": { "version", "filesystem": [{ "path", "access" }],
"network": [{ "host", "ports": [], "decision" }],
"credentials": [{ "name", "target" }], "default_deny",
"ui_access" } | null, "source": "file | default", "mode": "…" }`.

#### `PUT /api/v1/mxc/policy` — `config`
`update_mxc_policy`. Body: the policy object → the app's result
(`{ ok, persisted }`-shaped). 

#### `PUT /api/v1/mxc/mode` — `config`
`mxc_set_mode`. Body: `{ "mode": "learning | enforced | disabled" }`
→ `{ "ok": true, "mode": "…", "persisted": true }`.

#### `POST /api/v1/mxc/self-test` — `read` — synchronous
`mxc_run_self_test` →
`{ "cases": [{ "name", "description", "kind", "expected",
"actual", "passed" }], "summary": { "total", "passed", "failed" } }`.
Pure policy evaluation; nothing is executed.

#### `GET /api/v1/mxc/activity` — `read`
`mxc_get_activity_report`. Query: `since_ms` (optional) →
`{ "entries": [{ "timestamp", "decision", "description", "kind",
"path_or_host", "port", "count" }], "source": "…" }`. An absent
report is an empty list with `source: "none"`, not an error.

#### `POST /api/v1/mxc/tightening-proposals` — `read`
`mxc_propose_tightening`. Body: an activity report object (the shape
`GET /mxc/activity` returns) → the app's proposed tightened policy.
Evaluation only; applying a proposal is `PUT /mxc/policy`.

### 6.14 Updates

#### `GET /api/v1/updates` — `read`
The `updates` section of `GET /config` (per-component
`pinned_version`, `auto_update`, `last_checked`).

#### `GET /api/v1/updates/components` — `read`
`updates_list`. `{ "checked_at": time | null, "components": [...] }`,
the rows of the check below without contacting any feed: upstream
versions and feed errors from the last check (`checked_at`), installed
and pinned versions as of now.

#### `POST /api/v1/updates/check` — `read`
`updates_check_all`. Contacts the feeds and returns the rows with
`checked_at`; the result is kept (`GET /updates/components`).

#### `POST /api/v1/updates/components/{component_id}/apply` — `destructive`
`updates_apply` (admin). Body: `{ "version": "…" }` (optional) → `200`
with `task_id` (an operation to poll; `null` when there is nothing to
do). `501 UNSUPPORTED` for the report-only components `postgres` and
`mxc`.

#### `PUT /api/v1/updates/components/{component_id}/pin` — `config`
`updates_pin`. Body: `{ "version": "…" | null }` →
`{ "ok", "persisted", "component_id", "pinned_version" }`.

#### `PUT /api/v1/updates/components/{component_id}/auto` — `config`
`updates_set_auto`. Body: `{ "enabled": true }` →
`{ "ok", "persisted", "component_id", "auto_update" }`. `true` is
refused (`UNSUPPORTED`) for a component whose row says
`auto_update_supported: false`, which today is every one.

#### App self-update — `GET /api/v1/app-update` — `read`
`app_update_status`. LLMdil's own update state, distinct from the
per-component pins above: `{ "current_version", "check_enabled",
"auto_update", "last_checked_unix_ms", "last_check_outcome",
"last_check_error", "available", "staged", "last_stage_error",
"last_apply", "banner" }`. `banner` is non-null exactly when a
SHA-256-verified installer for a newer version is staged and
waiting on a restart. Runtime state lives in
`<data_dir>/app-update/state.json`, never in the config.

#### `POST /api/v1/app-update/check` — `control`
`app_update_check`. Runs one check against GitHub Releases
(drafts and prereleases are never candidates; no downgrade) and
returns the status object. A failed check is an outcome in the
payload (`last_check_outcome: "failed"` with `last_check_error`),
not an HTTP error. With auto-update on, a found update is staged
inline.

#### `PUT /api/v1/app-update/checking` — `config`
`app_update_set_checking`. Body: `{ "enabled": true }` → the
status object. Persists `app_update.check_enabled` in the config.

#### `PUT /api/v1/app-update/auto` — `config`
`app_update_set_auto`. Body: `{ "enabled": true }` → the status
object. Persists `app_update.auto_update` in the config.

#### `POST /api/v1/app-update/download` — `control`
`app_update_download`. Downloads the available release's
installer into `<data_dir>/app-update/` and verifies its SHA-256
against the digest the GitHub API publishes for the asset; only a
verified file is recorded as staged. The running installation is
never touched.

#### `POST /api/v1/app-update/restart` — `control`
`app_update_restart`. Records the pending apply, spawns the
detached apply helper (this executable with
`--llmdil-apply-update`), and exits the process. The helper waits
for the exit, runs the staged installer silently, records the
outcome in the state file, and relaunches. → `{ "ok",
"restarting", "version" }`.

### 6.15 MCP

The reads and the connection test are routed. Writing the registry
is not: see §6.16.

#### `GET /api/v1/mcp/servers` — `read`
`mcp_list` → an array of servers: `id`, `display_name`, `transport`,
`exposure`, `command`, `args`, `url`, `env_keys`, `cred_ref` (a
Credential Manager target *name*), `allowed_tools`, `metered`,
`rpm_limit`, `running`, `process_state`, `blocked_reason`,
`credential_required`, `credential_present`, `credential_status`.
Never a credential value. The registry part is what `GET /config`
already shows.

#### `GET /api/v1/mcp/preflight` — `read`
`mcp_start_preflight` → `{ servers: [ … ] }`: the gateway servers the
next gateway start would leave out because their credential is
missing.

#### `POST /api/v1/mcp/servers/{id}/test` — authorization class X (admin) — synchronous
`mcp_test_connection` → `{ id, ok, detail, … }`. Tests a server as its
exposure runs it. Admin only: the server may be a command the app
runs.

### 6.16 Registry exclusions — every operation with no route

The registry holds 161 operations: 132 routed and 29 with route
`None`. A `None` route is an explicit exclusion, never an oversight
(registry module docs). PLAN.md decisions 10 and 11 were reversed by
the owner except for what follows; the security-sensitive ones are
pinned by a test (`auth_matrix.rs`
`security_exclusions_have_no_route`). The complete list:

| Reason | Operations |
|---|---|
| **Security:** the raw patch reaches every section of the configuration, MCP commands and the Admin API's own settings included (decision 11) | `update_config` (C) |
| **Security:** the master key's value never crosses the API (decision 4) | `gateway_master_key` (SDisclosure) |
| **Security:** registering or exposing an MCP server chooses a command the app starts, or a remote endpoint that gets a credential; storing one writes a secret | `mcp_save_server` (X), `mcp_set_exposure` (X), `mcp_delete_server` (X), `mcp_store_credential` (X) |
| **Security:** asks for elevation and the account password at the console | `services_register` (D), `services_unregister` (D) |
| **Security:** asks for elevation at the console and creates tokens; the endpoint flip sends the logs somewhere else | `log_receiver_deploy` (D), `log_receiver_remove` (D), `log_receiver_flip_endpoint` (C) |
| Acts on the desktop session (§7.1–7.2) | `open_url` (L), `open_folder` (L) |
| Writes to the user's editor profile, the master key included | `vscode_install_extensions` (X), `vscode_write_config` (X) |
| WinML — peripheral (decision 10) | `winml_probe` (R), `winml_status` (R), `winml_register` (D), `winml_unregister` (D) |
| No 1:1 route — served inside a composition | `gateway_set_bind_host` (C, inside `PATCH /gateway/config`); `model_set_short_name` (C), `model_set_enabled` (C), `model_set_offload_spec` (C) and `model_set_mtp` (C), inside `PATCH /models/{id}`; `gateway_activation_complete` (L, server bookkeeping after apply/(re)start, §6.6); `gateway_activation_record_execution` (C, execution bookkeeping recorded by the desktop apply funnel, §6.6); `gateway_create_group` (C) and `gateway_delete_group` (D), served by the derived group routes (§6.5/§6.17); `activation_apply` (L), served by `POST /activation-plan/apply` (§6.6) |

The PostgreSQL port has no operation of its own and so no route: it
is part of the raw patch.

### 6.17 API-layer routes (no backing registry operation)

These routes live in `crates/manager-api`, not in the registry.
The derived routes are enumerated in `DERIVED_ROUTES`
(`manager-api/src/http.rs`) and distributed through §5/§6 above:
`GET /server`, `GET /backends/{id}`, `GET /models/{id}`,
`GET /operations/{id}`, `GET /logs/scopes`, `GET /gateway/groups`,
`GET /updates`, `PATCH /models/{id}`, `POST /gateway/groups`,
`PATCH /gateway/groups/{name}`, `DELETE /gateway/groups/{name}`,
`POST /gateway/start`, `POST /activation-plan/apply`,
`GET /gateway/generated-routing`, `GET /telemetry/snapshots`,
`GET|DELETE /telemetry/snapshots/{id}`, `GET /diagnostics/bundles/{bundle_id}/content`, and:

#### `GET /api/v1/events` — `read` (any authenticated role)
Server-Sent Events stream of the app event bus
(`manager_app::event_bus`), each event with a process-global id.
Live-only: nothing is replayed on reconnect; ids let a client
detect gaps, and every fact the stream carries is also reachable
by GET (PLAN.md §9). It is an API-layer route, **not** a registry
operation.

The bus carries every event **whichever surface caused it**: an
operation run from the desktop UI (Tauri IPC) emits exactly the
events the same operation run through this API does — the tee sits
in the registry dispatch both paths share, not in HTTP handlers.
Each `data` object carries an additive `origin`: `desktop` (desktop
IPC), `api` (this API), or `app` (background work: pollers,
monitors, watchers). Event SSE `event:` names and producers:

| Event | Producer | Payload (same shape as) |
|---|---|---|
| `backend-status` | backend start/stop/restart (beside `service-lifecycle`) | `{id, event, state, origin, error?}` — `origin` here is the lifecycle origin (`ui`, `dashboard`, `activation`, …), so the bus's additive origin is not added |
| `service-lifecycle` | every lifecycle transition of every managed service (gateway, postgres, log-receiver, `backend:<id>`): operation begin/end and monitor-observed changes | `{key, kind, id?, event, state, origin, detail?, at}` — `origin` is the lifecycle origin, as for `backend-status` |
| `download-progress` | model download worker | `{model_id, downloaded_bytes, total_bytes, fraction}` |
| `wizard-progress` | setup step / audit runs | `{task_id, step_id, state, done, total, message}` |
| `telemetry-update` | each `get_gpu_telemetry` call | `{samples, at_ms}` |
| `task-progress` | a task created (`running`) or settled (`done`/`failed`/`aborted`, incl. a forced abort) | one `GET /operations` row (`TaskRecord`) |
| `config-changed` | any non-read operation that **succeeded** and changed the saved config (before/after diff) | `{op, sections: [top-level section names], at_ms}` — names only, never values |
| `gateway-request` | a successful `gateway_test_routing` (the decision it recorded) | one `GET /gateway/routing/recent` row |
| `alert-raised` | the notification monitor's failure / activation transitions (independent of toast settings; completions are not alerts) | `{category: failures\|activation, title, body, at_ms}` |
| `telemetry-tick` | the GPU telemetry poller, observed once per completed poll, at most 1/s | `{samples, at_ms}` — `get_gpu_telemetry`'s samples in crate shape (no processes, sparklines, or peaks); `at_ms` is the poll time |
| `vram-changed` | the same polls: a GPU's used VRAM moved ≥ max(256 MiB, 2 % of the card) from the last reported level (first observation is a silent baseline; down GPUs skipped) | `{index, name, vram_used_mib, vram_total_mib, previous_mib, delta_mib, at_ms}` |
| `log-line` | new lines in the supervisor's captured process output (the rings `get_logs` reads), followed every 500 ms | `{scope, level, line, ts_ms, stream, archived?}`; at most 50 per scope per pass, the rest summarized in one `{…, skipped: n}` notice line |
| `app_update_changed` | `app_update_*` operations | the `app_update_status` shape |

Semantics: a failed operation emits no dispatch-level event
(`config-changed`, `gateway-request`); events an operation emitted
before failing are delivered as emitted. `config-changed` covers
changes made by operations only (not background watchers, e.g. the
post-download verification stamp) and is attributed to the
operation that observed the diff. `telemetry-tick`, `vram-changed`
and `log-line` come from a background producer loop that runs only
while at least one subscriber is connected; it observes the
telemetry poller and the supervisor without starting either
(telemetry flows once the poller is running — it starts on first
telemetry use). `log-line` covers supervised processes only: the
file-backed `gateway` and `wizard` scopes of `get_logs` are not
streamed, and lines present when the stream starts are not
replayed. Every payload is data the `read` class can already GET,
so the stream needs no per-role filtering.

The remaining 24 API-layer routes are the auth/admin surface
(`AUTH_ROUTES` in `manager-api/src/auth_routes.rs`), specified by
`auth-plan.md`: `POST /auth/login`, `POST /auth/totp`,
`POST /auth/refresh`, `POST /enroll`, `GET /auth/me`,
`POST /auth/logout`, `POST /auth/password`, `GET /auth/sessions`,
`DELETE /auth/sessions/{id}`, `POST /auth/totp/enroll`,
`POST /auth/totp/verify`, `POST /auth/totp/disable`,
`GET|POST /admin/users`, `PATCH|DELETE /admin/users/{id}`,
`POST /admin/users/{id}/reset-password`,
`GET /admin/users/{id}/sessions`,
`DELETE /admin/users/{id}/sessions/{session_id}`,
`GET|POST /admin/api-tokens`, `DELETE /admin/api-tokens/{id}`,
`POST /admin/enrollment-codes`, `GET /admin/audit` — all under
`/api/v1`.

**Must-change-password (SEC-03).** `POST /admin/users/{id}/reset-password`
sets a temporary password, flags the account must-change, and revokes
every session **and API token** of the user in one store write. Until
the user changes the password:

- `POST /auth/login` (and `POST /auth/totp` for a TOTP account, which
  still completes) returns tokens for a short session (15 minutes for
  both tokens, not 30 days). Token responses from login, TOTP, enroll
  and refresh carry an additive top-level `"must_change_password":
  <bool>` alongside `user.must_change_password`; `GET /auth/me` carries
  the same top-level field.
- Every authenticated route except `POST /auth/password`,
  `POST /auth/logout` and `GET /auth/me` answers **403
  `PASSWORD_CHANGE_REQUIRED`** — for session tokens and API tokens
  alike (the flag is read from the user record on every request).
  `POST /auth/refresh` is refused the same way, without rotating.
  Each refusal is audited (`result: "password_change_required"`).
- `POST /auth/password` (`{ "current_password", "new_password" }`)
  clears the flag; the calling session stays valid (and becomes
  refreshable) and the account's other sessions are revoked.

**TOTP enrollment step-up (SEC-04).** `POST /auth/totp/enroll` takes
`{ "password": "<account password>" }` for first enrollment as well as
for replacing a live factor; a missing or wrong password answers 401
`INVALID_CREDENTIALS` and counts toward the account lockout. A
must-change account gets `PASSWORD_CHANGE_REQUIRED` first.

### 6.18 Measurements

The Optimize measurement store (predicted-throughput design §11):
two routed operations, added because the history view's per-entry
delete and the backend forms' draft lookup are cross-surface by
nature. The rest of the Optimize domain is §6.19.

#### `POST /api/v1/measurements/lookup` — `read`
`measurement_lookup`. The body is the request object itself — a draft
configuration: `backend_id` (edit form: the saved backend, with any
of `n_ctx`, `n_batch`, `offload_layers`, `mtp_position`,
`parallel_slots`, `raw_flags` overridden) or `model_id` (add form:
a not-yet-created backend, with `gpu_indices` selecting the
placement). → `{ "fingerprint", "exact": <predicted> | null,
"near": <predicted + "differs": […]> | null }`, where `<predicted>`
is the §4.1 `predicted` shape. `exact` is the newest measurement
under the draft's fingerprint, whatever backend or experiment
produced it; `near` is present only when there is no exact match:
the closest same-model measurement whose recorded inputs differ in
one or two named fields. Absences are nulls, never zero-filled.

#### `DELETE /api/v1/measurements` — `delete`
`measurement_delete`. Body: `{ "fingerprint", "measured_at" }` —
an entry is identified by the pair. Removes that one entry from
the store; history is otherwise append-only. → `{ "deleted": true,
"fingerprint", "measured_at" }`; `404` when no such entry exists.

### 6.19 Optimize

The *state* below is `optimize_get_state`'s payload: `purpose`,
`groups`, `backends`, `context_summary`, `context`,
`default_backend`, `plan` (items, experiments and their live
validation; null without one), `measurements`, `candidates`,
`eval_baseline`, `consulting`, `consult_progress`, `consult_error`,
`applying`, `measuring`, `job`, `job_error`, `restore_failures`,
`running_ops`, `tuning`. Every route here answers it, or carries it
as the operation's detail.

| Route | Class | Operation | Body |
|---|---|---|---|
| `GET /api/v1/optimize` | R | `optimize_get_state` | |
| `PUT /api/v1/optimize/purpose` | C | `optimize_save_purpose` | `{ "scope": "deployment \| group \| backend", "target"?, "text" }` |
| `POST /api/v1/optimize/plan/validate` | C | `optimize_validate_plan` — re-checks the plan and records it as valid for the current configuration | |
| `PUT /api/v1/optimize/plan/items/{item_id}` | C | `optimize_set_item_state` | `{ "decision": "accepted \| denied \| undecided" }` |
| `PUT /api/v1/optimize/plan/items/{item_id}/outcome` | C | `optimize_set_item_outcome` | `{ "outcome": "applied \| failed \| not_attempted", "error"? }` |
| `POST /api/v1/optimize/plan/apply` (**Op**) | D | `optimize_apply_plan` | `{ "destructive"?, "dry_run"? }` |
| `DELETE /api/v1/optimize/plan` | D | `optimize_discard_plan` | |
| `PUT /api/v1/optimize/experiments/{experiment_id}` | C | `optimize_set_experiment_state` | `{ "decision" }` |
| `POST /api/v1/optimize/experiments/run`, `…/estimate` (**Op**) | L | `optimize_run_experiments`, `optimize_estimate_experiments` — the accepted ones | |
| `POST /api/v1/optimize/consult` (**Op**) | L | `optimize_consult` | `{ "backend_id"?, "sample"?, "directive"?, "candidate_repos"?, "replace"? }` |
| `POST /api/v1/optimize/consult/cancel` | L | `optimize_cancel_consult` → `{ "cancelled" }` | |
| `POST /api/v1/optimize/discover` (**Op**) | L | `optimize_discover` | `{ "directive" }` |
| `POST /api/v1/optimize/job/cancel` | L | `optimize_cancel_job` → `{ "cancelled" }`: measure, estimate, baseline, experiments, tuning | |
| `POST /api/v1/optimize/tuning/{sweep_id}/adopt` | C | `optimize_tuning_adopt` | `{ "adopt_anyway"? }` |

Measure, estimate, baseline and tuning are under the backend (§6.2).

- **Replacing a plan.** A consultation replaces the stored plan and
  its decisions. While a plan exists `POST /optimize/consult`
  answers `409 PLAN_EXISTS` unless the body has `"replace": true`
  (sample consultations too). The window asks instead.
- **Apply** runs the accepted items one after the other: the
  non-destructive ones, or with `destructive` only the deletes. An
  item that fails stops the rest and the operation ends `failed`,
  with `apply_result` (`steps`, `applied`, `failed`, `not_attempted`,
  `skipped`) in the detail. It cannot be aborted; what was applied
  stays. `dry_run` only reports the steps.
- **Abort.** Consult, discover, measure, estimate, baseline,
  experiments and tuning are tasks with a real cancel: abort the
  task in `/operations`, or use the two cancel routes. A cancelled
  operation ends `failed` or `aborted`; a tuning or experiment run
  starts the backends it stopped again, and `restore_failures` names
  those it could not.

### 6.20 Bench

| Route | Class | Operation | Body → answer |
|---|---|---|---|
| `GET /api/v1/bench/prompts` | R | `bench_prompts_list` | → `{ prompts }` |
| `POST /api/v1/bench/prompts` | C | `bench_prompt_save` — creates, or replaces the entry with this `id` | `{ "id"?, "name", "purpose", "body" }` → `{ id, prompts }` |
| `DELETE /api/v1/bench/prompts/{id}` | D | `bench_prompt_delete` — a shipped prompt comes back with its shipped text | → `{ prompts }` |
| `POST /api/v1/bench/prompts/generate` (**Op**) | L | `bench_prompt_generate` — asks a running backend for a draft; saves nothing | `{ "purpose", "description", "size": "short \| standard", "backend_id"? }` → detail `{ text, backend_id, model }` |
| `POST /api/v1/bench/prompts/generate/cancel` | L | `bench_prompt_generate_cancel` | → `{ cancelled }` |
| `POST /api/v1/bench/runs` (`202`) | L | `bench_run_start` | below |
| `POST /api/v1/bench/runs/{sweep_id}/cancel` | L | `bench_run_cancel` | → `{ cancelled }` |
| `GET /api/v1/bench/results` | R | `bench_results_list` | → `{ sweeps }`; the running one carries `progress` |
| `DELETE /api/v1/bench/results/{sweep_id}` | D | `bench_result_delete` | → `{ sweeps }`; `400` while it runs |

`POST /bench/runs` body: `{ "backend_id", "prompt_id", "temperature"?,
"preset"? (kv | spec | combined) or "dimensions", "draft_model_id"?,
"baseline_arm_index"?, "correctness_control"?, "confirm_at_06"?,
"stop_backend_ids"? }`. It answers as soon as the sweep is accepted:
`202` `{ id, kind: "bench-run", status: "running", detail, sweep_id,
sweeps }`. `id` is the task for `/operations` (poll, abort); progress
and results are under `sweep_id` in `GET /bench/results`. Refused at
once: `404` unknown backend or prompt; `400` temperature outside 0–2,
unknown preset, running backends in the way that `stop_backend_ids`
does not name (the message lists them), or an `offload_expr` with
anything but expert-offload flags; `409 BUSY` while another sweep or
measurement runs. The named backends are stopped as the sweep's first
step (`progress.phase` = `stopping backends`) and are not started
again; if one cannot be stopped the sweep ends `failed` and the task
says why. The sweep's final `status` is `complete`, `partial` or
`failed`. A sweep record names its model file with the local path (as
`data_dir` in `GET /config` does).

### 6.21 Notifications and Windows services

#### `PUT /api/v1/notifications` — `config`
`notifications_set_config`. Body: `{ "enabled", "operations"?,
"failures"?, "activation"? }` — replaces the preferences; an absent
category is on, an unknown field `400`. Read them in `GET /config`.

#### `POST /api/v1/notifications/test` — `control` — synchronous
`notifications_send_test`: one toast **on the managed machine**. →
`{ ok, delivery_confirmed: false, windows_setting, detail }`; `501
UNSUPPORTED` when no desktop window is attached.

#### `GET /api/v1/services` — `read`
`services_list` → `{ mode, host_present, services: [{ target, kind,
service_name, registered, state, lifecycle, transition, pid,
child_pid, exit_code, failure_reason }] }`. The Windows account and
the local path the window shows are not in the API's answer.
Registering and unregistering stay at the console (§6.16).

## 7. Capabilities that do not map cleanly

1. **`open_url` — no endpoint.** It opens a browser on the managed
   machine: a desktop side effect with no meaning for a remote
   client (and a hazard — an API call that pops windows on the
   host). The data behind it is exposed (`GET /setup/steps/{id}/
   help-url`); opening is the client's own action.
2. **`open_folder` — no endpoint.** Same class: reveals a folder in
   the host's file manager. Remote retrieval of the one artifact
   clients actually want (the diagnostics bundle) is
   `GET /diagnostics/bundles/{id}/content` instead.
3. **Elevation prompts (wizard steps).** Several steps perform
   machine-level installs, and the start-gateway step reconciles
   the Windows firewall rule with an elevation prompt when the rule
   state changes. Over REST the prompt still appears — on the
   managed machine's console. A step that needs elevation cannot be
   answered from the API: with no interactive user at the host it
   blocks until its budget expires and fails the operation. The API
   faithfully exposes this (the operation fails with the step's
   outcome and `manual_command`), but operating the wizard remotely
   on an unattended machine is inherently limited to steps that do
   not prompt. This is a property of the underlying capability, not
   of the mapping.
4. **Path-returning commands.** `telemetry_save_snapshot` and
   `diagnostics_export_bundle` return host filesystem paths, as
   metadata for administrators (§3.1). Snapshots are read and deleted
   by id (§6.9). The bundle gets a real download endpoint
   (§6.11) because remote retrieval is its purpose; that serving
   step is an API-layer addition, not an app behavior.
5. **Activation apply orchestration** (§6.5): composed in the
   desktop UI today; the server must own it. The only place the API
   is more than a wrapper.
6. **`gateway_activation_complete`**: internal bookkeeping, not a
   client endpoint (§6.6).
7. **Streaming: polling is the contract; SSE is additive.** No app
   capability streams: logs are paged queries, telemetry is poll
   snapshots, download progress is state polling, and the poll
   model remains the correctness path throughout. As built, an
   SSE channel was added on top of it — `GET /api/v1/events`
   (§6.17), an API-layer route fed by the app event bus (every
   operation event from desktop and API callers alike, plus
   background telemetry, VRAM, log, task, and alert producers).
   No capability needs the stream to map completely, and a client
   that never opens it loses nothing.
8. **File pickers: none.** No audited command depends on a file
   dialog — model registration takes a URL, bundle export takes no
   destination. Nothing is lost to their absence remotely.
9. **`updates_apply`**: implemented since; `501` only for the
   report-only components (§6.14).
10. **Routing rules**: no dedicated commands exist; they move only
    as part of the gateway config patch. If rule-level CRUD is ever
    wanted, it is new app behavior first, API second.

## 8. Open questions for review

Resolved since the draft: all four questions below were decided
in `PLAN.md` §4 (port/exposure, decision 1–2; peripheral domains
excluded, decision 10, since reversed except §6.16; operation retention = process lifetime,
decision 15; raw config patch excluded, decision 11). The list is
kept as the review record.

1. **Admin port exposure**: bind loopback-only, LAN, or Tailscale
   only? (Auth plan territory, but it shapes whether `GET /status`
   can be an unauthenticated discovery probe.)
2. **Peripheral domains** (§6.15): confirm exclusion from v1.
3. **Operation retention**: operations currently live for the
   process lifetime (as app tasks do). Is a bounded history
   (e.g. last 100) acceptable to state in the contract?
4. **Raw config patch** (`PATCH /config`): keep in v1 given the
   domain endpoints cover the same ground with narrower blast
   radius?
