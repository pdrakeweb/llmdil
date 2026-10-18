# Admin API — REST Contract (Draft)

Status: design draft for Pete's review. No code exists yet.
Scope: an HTTP API served by the Service Manager on a dedicated **admin port**
on the managed machine (separate from the LiteLLM gateway port), exposing the
app's capabilities for read and control from a remote client (first consumer:
an Android app).

Source of the capability audit: the 95 Tauri commands registered in
`src-tauri/src/main.rs` (`tauri::generate_handler!`, lines 5882–5977) and the
client wrappers and payload types in `ui/src/api.ts`. Where this document
names an app command, that is the implementation the endpoint wraps.

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

- Base: `http://<host>:<admin-port>/api/v1`
- The admin port is new configuration (not the gateway port, not the app's
  internal IPC). It binds per operator configuration; see the auth plan for
  the exposure rules.
- Versioning is a single path segment (`v1`). Breaking shape changes ship as
  `v2` alongside `v1`; additive fields never bump the version.
- `GET /api/v1/status` doubles as the discovery/teach-the-client probe: an
  unauthenticated-UI client can confirm it is talking to a Service Manager
  admin API and learn its version before presenting credentials (the
  response itself still requires auth once auth ships — see open question 3).

### 2.2 Format

- JSON only, `Content-Type: application/json`, except
  `GET /gateway/generated-config` (`text/yaml`) and
  `GET /diagnostics/bundles/{id}/content` (`application/zip`).
- Request bodies are JSON objects. Unknown fields are rejected with
  `INVALID_INPUT` (the app's patch objects are closed structs; a typo'd
  field must not silently no-op).
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

- `kind` is the app's `ApiError` variant name (`src-tauri/src/main.rs:119`):
  `Config`, `Supervisor`, `Wizard`, `Telemetry`, `Gateway`, `Models`,
  `Mxc`, `Mcp`, `WinMl`, `NotFound`, `InvalidInput`, `ContractGap`,
  `Unsupported`, `Io`, `Json`.
- `code` is the stable machine-readable form (SCREAMING_SNAKE of `kind`),
  plus REST-layer codes that have no app counterpart: `BUSY` (a singleton
  resource is already running something), `OPERATION_FAILED` (an operation's
  terminal error, §3), and the auth codes the auth plan will define
  (`UNAUTHENTICATED`, `FORBIDDEN`).
- `message` is the app's error string, unparsed. Clients display it; they
  never branch on it.
- HTTP status mapping:

  | Condition | HTTP | code |
  |---|---|---|
  | Invalid argument / body fails validation | 400 | `INVALID_INPUT` |
  | Referenced backend/model/group/step/operation absent | 404 | `NOT_FOUND` |
  | Singleton busy (wizard step already running, download already in flight, plan apply already running) | 409 | `BUSY` |
  | Config patch fails validation | 422 | `CONFIG` |
  | Capability deferred in the app itself | 501 | `UNSUPPORTED` |
  | Everything else (`Supervisor`, `Gateway`, `Wizard`, `Telemetry`, `Models`, `Mxc`, `Mcp`, `WinMl`, `Io`, `Json`, `ContractGap`) | 500 | per kind |

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
  "error": null
}
```

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
- Cancellation: `POST /operations/{id}/abort` (§6.10). Not every kind is
  abortable — a wizard step mid-subprocess may only be abandonable; the
  operation's terminal state says what happened.

### 3.2 What is synchronous, and its bound

Reads (all `GET`s except bundle content), config writes, placement
propose, routing test, MXC self-test and policy writes, and the gateway
quiesce wait (bounded by the caller's `timeout_secs`, max 120). The one
synchronous call with model-scale latency is `POST /backends/{id}/test`
(a real inference request): documented at §6.2 with a 300 s client-side
guidance timeout.

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
  "memory_basis": "exact | catalog | estimate",
  "offload": { "moe_layers": 48, "layers": 40, "expert_bytes": 0,
               "total_bytes": 0, "green_limit_layers": 48,
               "green_end": 1.0, "yellow_end": 1.0 }
}
```

Trailing-hour statistics and memory/offload blocks are present only when
the app reports them (a metrics window exists; the model resolves); they
are never zero-filled (§1 rule 4). `configured: false` marks a
supervisor-only orphan process; orphans accept `stop` only.

### 4.2 Backend config (`backend_get_config`)

```json
{
  "id": "coder", "enabled": true, "model_file": "…", "port": 8082,
  "flags": { }, "raw_flags": "-ot exps=CPU",
  "restart_policy": { }, "overrides_global": false
}
```

A config patch (`BackendPatch`, `crates/manager-config/src/diff.rs:21`):
any subset of `enabled`, `model_file`, `port`, `n_ctx`, `n_batch`,
`tensor_split`, `split_mode`, `raw_flags` (nullable), `max_retries`,
`backoff_base_secs`. A preview diff (`ConfigDiff`):
`{ backend_id, changes: [{ field, old, new }], valid }`.

### 4.3 Model (`list_models` row)

```json
{
  "id": "qwen3-coder-30b",
  "name": "Coder",
  "gguf_path": "…", "quant": "UD-Q4_K_M", "params_b": 80.0,
  "sha256": "3a47…", "verified_at": "2026-10-16T…Z",
  "enabled": true, "on_disk": true, "size_bytes": 49301055488,
  "downloading": false, "verifying": false, "paused": false,
  "partial_bytes": null, "download_error": null,
  "catalog_size_bytes": 49301055488, "ctx_max": 262144,
  "fit_check_available": true
}
```

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
                  "daily_cap_usd": 0, "model_allowlist": [] }
}
```

Group `enabled`/`enabled_members` are derived by the app (a group is
enabled iff at least one member backend is enabled); they are read-only
in the API as in the app.

### 4.5 Gateway status (`gateway_status`)

```json
{
  "port": 4000, "bind_host": "0.0.0.0", "lan_ip": "192.168.5.23",
  "groups": 5, "routing_rules": 0, "up": true,
  "routing_log_len": 0, "openrouter_cred_present": null,
  "activation_pending": true, "activation_changes": 3
}
```

`groups` counts enabled groups only (derived as in §4.4: at least one
member backend enabled) — the groups the gateway actually serves, not
the configured total (Pete, 2026-10-17, tracker #11).

### 4.6 Activation plan (`gateway_activation_plan`)

```json
{
  "pending": true, "gateway_up": true, "gateway_pending": false,
  "drift": false, "applies_at_next_start": false, "actionable": true,
  "backend_restart_ids": ["coder"],
  "changes": [ { "ts": "…", "kind": "…", "summary": "…",
                 "scope": "backend", "targets": ["coder"],
                 "activation": "restart" } ],
  "snapshot_available": true
}
```

### 4.7 GPU telemetry (`get_gpu_telemetry`)

Per-GPU samples: `index`, `name`, `ts`, `utilization_pct`,
`vram_used_mib`, `vram_total_mib`, `temp_c`, `power_w` (nullable),
`stale`, `down`, `source` (`nvml` | `nvidiasmi`), `mem_util_pct`,
`fan_pct`, `power_limit_w`, `temp_peak_c`; plus per-process attribution:
`gpu_index`, `pid`, `name`, `vram_mib` (**null when not measured** —
WDDM devices), `backend_id`. The payload also carries the derived
per-GPU info list the desktop renders. Field-for-field the app's
payload; the API adds nothing.

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

89 client-facing endpoints. ("Op" = returns `202` + operation, §3.)

| Domain | Count | Endpoints |
|---|---|---|
| Status | 1 | `GET /status` |
| Backends | 14 | list, create, get, delete, config get/patch, diff preview, start/stop/restart (Op), test, flags, offload, launch command |
| Models | 11 | list, create, get, delete, patch, download (Op) + pause/resume/cancel, verify (Op), assign |
| Placement | 2 | propose, apply |
| Gateway | 13 | status, config get/patch, generated config, master key, start (Op), stop, groups CRUD (4), routing test, routing recent |
| Activation plan | 4 | get, apply (Op), quiesce-wait, cancel |
| Wizard / setup | 11 | steps, status, prerequisites, run (Op), retry, skip, mark-manual, commands, help URL, complete, audit (Op) |
| Logs | 2 | query, scopes |
| Telemetry | 4 | GPUs, interval, reset peaks, snapshot |
| Operations | 3 | list, get, abort |
| Diagnostics | 2 | create bundle (Op), download bundle |
| Config / settings | 2 | get, patch |
| MXC | 6 | policy get/put, mode, self-test, activity, tightening proposals |
| Updates | 5 | list, check, apply, pin, auto |
| Peripheral (MCP, VS Code, WinML) | 9 | §6.14 — candidates for exclusion from API v1 |

## 6. Endpoint reference

### 6.1 Status

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
split, replacing the telemetry assignment). Each omitted field keeps
its creation default.

#### `DELETE /api/v1/backends/{id}` — `destructive`
`backend_delete` → `{ "backend_id", "deleted": true }`. Refused
(`409`/`Supervisor`) while the backend is running, as in the app.

#### `POST /api/v1/backends/{id}/start` — `control` — **Op**
#### `POST /api/v1/backends/{id}/stop` — `control` — **Op**
#### `POST /api/v1/backends/{id}/restart` — `control` — **Op**
`start_backend` / `stop_backend` / `restart_backend`. Body (stop,
restart): `{ "graceful": true }`. The app commands already return
`{ task_id, backend_id }`; the REST layer returns `202` with the
operation for that task. Start runs the app's preflight (port, model
file, placement) and fails the operation with the preflight message.

#### `POST /api/v1/backends/{id}/test` — `control` — synchronous
`test_backend`. Body: `{ "prompt": "…" }`. One inference request
against the running backend. Response (`BackendTestResult`):

```json
{ "status": 200, "content": "…", "prompt_tokens": 12,
  "completion_tokens": 64, "latency_ms": 1823 }
```

A non-2xx from the backend is reported in `status`, not as an API
error, mirroring the app. Duration is model-bound: clients should
allow up to 300 s. This is the one synchronous control call; it is
kept synchronous because the app has no task wrapper for it and its
result is the response body itself.

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
`RestartNow`, `restart_task_id` names an operation the client polls
(the response itself stays `200`: the config save is complete; only
the restart is asynchronous). The save records activation-plan
changes exactly as the app does.

#### `POST /api/v1/backends/{id}/config/preview-diff` — `read`
`backend_preview_diff`. Body: `{ "patch": { … } }` → `ConfigDiff`
(§4.2). No state changes.

#### `PUT /api/v1/backends/{id}/flags` — `config`
`backend_update_flags`. Body: `{ "n_ctx": 32768, "n_batch": 512 }` →
`{ "backend_id", "persisted": true }`. Applies at the backend's next
start; the app records the activation-plan entry.

#### `PUT /api/v1/backends/{id}/offload` — `config`
`backend_set_offload`. Body: `{ "layers": 40 }` →
`{ "backend_id", "layers": 40, "raw_flags": "…", "changed": true }`.
Only valid for offload-capable backends (the `offload` block in §4.1
says which); otherwise `400 INVALID_INPUT`, as in the app.

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
  "file_name": "model.gguf", "quant": "Q4_K_M", "params_b": null }
```

`file_name`, `quant`, `params_b` optional (file name defaults to the
URL's last segment). When `params_b` is omitted or non-positive, the
count resolves from the curated catalog (exact id match, then exact
source-URL match); when no source yields a count the call fails with
a validation error naming the missing count — a model is never
registered with an unknown or placeholder parameter count. →
`201` `{ "model_id" }`. The model starts
enabled with no pinned hash (unverified until a pin exists — the app
treats a missing pin as "no verification possible", never as verified).

#### `DELETE /api/v1/models/{id}` — `destructive`
`model_delete` → `{ "model_id", "deleted": true, "bytes_freed": n }`.
Deletes the file and any partial; refused while a download is in
flight or any backend references the model.

#### `PATCH /api/v1/models/{id}` — `config`
Body: any of `{ "display_name": "Coder" }` (`model_set_short_name` —
the operator's persistent rename; empty string clears it) and
`{ "enabled": false }` (`model_set_enabled` — excludes the model
from wizard downloads; file and config are kept). →
`{ "id", "name", "enabled" }` reflecting the applied fields.

#### `POST /api/v1/models/{id}/download` — `control` — **Op**
`download_model` → `202` operation (`kind: model_download`, detail
carries `{ "model_id", "dest" }`). The transfer runs in the app's
background worker and resumes from any partial file. Progress: poll
`GET /models/{id}` (`downloading`, `partial_bytes`, then `verifying`,
then `verified_at` set or `download_error` set — a hash mismatch
deletes the file and lands in `download_error`). `409 BUSY` when a
download for this model is already in flight.

#### `POST /api/v1/models/{id}/download/pause` — `control`
`model_pause_download` → `{ "model_id", "paused": true }`. Partial
file kept.

#### `POST /api/v1/models/{id}/download/resume` — `control` — **Op**
`model_resume_download` → `202` operation, as download.

#### `POST /api/v1/models/{id}/download/cancel` — `destructive`
`cancel_download` → `{ "model_id", "cancelled": true }`. The partial
file is discarded. Errors when no download is in flight.

#### `POST /api/v1/models/{id}/verify` — `control` — **Op**
`model_verify_checksum`. SHA-256 of the on-disk file against the
pinned hash. Minutes-scale on multi-GiB files (~17 min for 46 GiB on
the reference machine), hence an operation; its completion detail is
`{ "id", "ok": true|false }`. Models without a real pin fail with
`MODELS` (nothing to verify against), as in the app.

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
`gpu_weights_mib`, `weights_only: true`. Pure evaluation against the
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
the app. The granular group endpoints below are conveniences over
this patch.

#### `GET /api/v1/gateway/groups` — `read`
The `groups` array of `GET /gateway/config`.

#### `POST /api/v1/gateway/groups` — `config`
#### `PATCH /api/v1/gateway/groups/{name}` — `config`
#### `DELETE /api/v1/gateway/groups/{name}` — `destructive`
Read-modify-write over `update_gateway_config`: create, patch
(`members`, `strategy`, `fallbacks`), or remove one group by name.
`name` is the group's identity (groups have no separate id in the
app). Derived endpoints — there is no group command in the app; the
server performs the same merge the desktop UI performs. Renaming a
group is delete + create.

#### `GET /api/v1/gateway/generated-config` — `read`
`gateway_preview_yaml` → `200`, `Content-Type: text/yaml`, body the
generated LiteLLM `config.yaml` exactly as the gateway will read it
at its next start. (The desktop returns it wrapped as
`{ "yaml": "…" }`; the API returns the document itself.)

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
`manual_command` on failure.

#### `POST /api/v1/gateway/stop` — `control`
`gateway_stop` → `{ "ok": true, "stopped": true|false }`
(`stopped: false` with `ok: true` means it was not running).

#### `POST /api/v1/gateway/routing/test` — `read`
`gateway_test_routing`. Body: `{ "group": "planner" }` (the app's
request object; `group` is required) → the routing decision trace.
Dry-run: the decision is recorded in the session routing log, as in
the app; no live request is sent.

#### `GET /api/v1/gateway/routing/recent` — `read`
`gateway_recent_requests` → `{ "requests": [ … ] }`, newest first —
the routing-test traces recorded this session. Live requests are not
logged (the app's semantics, kept).

Routing rules have no dedicated app commands: they are part of the
gateway config (§4.4) and change only through `PATCH /gateway/config`.
`GET /gateway/config` is their read path. (See §7.)

### 6.6 Activation plan

#### `GET /api/v1/activation-plan` — `read`
`gateway_activation_plan` → §4.6.

#### `POST /api/v1/activation-plan/apply` — `control` — **Op**
Body: `{ "mode": "drain" | "now" }`.

- `now`: restart each running backend in `backend_restart_ids`, then
  (when `gateway_pending`) stop and restart the gateway via the
  start-gateway step.
- `drain`: the same, preceded by the quiesce wait (in-flight gateway
  requests drain to zero, bounded at 120 s; on timeout the apply
  proceeds and the operation detail reports the in-flight count it
  proceeded with).

→ `202` operation; completion clears the plan
(`gateway_activation_complete`, server-side, as the desktop flow
does after success).

**This is the one endpoint that is not a thin command wrapper:**
today the orchestration (restart backends → quiesce → stop → start
step → complete) is composed in the desktop UI (`applyActivationPlan`
in `ui/src/api.ts` / `main.ts`) from the primitives below. The API
server must own that composition. The primitives remain individually
exposed where they have standalone meaning (quiesce-wait, gateway
stop/start, backend restart).

#### `POST /api/v1/activation-plan/quiesce-wait` — `control` — synchronous
`gateway_quiesce_wait`. Body: `{ "timeout_secs": 120 }` →
`{ "drained": true, "in_flight": 0, "timed_out": false }`. Bounded
by the caller's timeout (max 120 s) — the documented exception to
the 30 s synchronous guideline.

#### `POST /api/v1/activation-plan/cancel` — `destructive`
`gateway_activation_cancel` → `{ "cancelled": true }` or
`{ "cancelled": false, "reason": "…" }`. Restores the pre-change
config snapshot and clears the plan; refused (error) when the config
has since changed through another path, as in the app.

`gateway_activation_complete` is **not** a client endpoint: it is
bookkeeping the server performs after a successful apply or gateway
(re)start. Listed here so the mapping is complete.

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
result: `{ "done", "total", "steps": [ … ] }`. `already_running`
in the app becomes `409 BUSY` pointing at the in-flight audit
operation.

### 6.8 Logs

#### `GET /api/v1/logs` — `read`
`get_logs`, semantics unchanged (§2.6):

| Param | Meaning |
|---|---|
| `scope` (required) | a process/scope id (`coder`, `planner`, …), `gateway`, `wizard`, or `all`; archived scope ids from `scopes` are accepted |
| `level` | heuristic filter: `error` \| `warn` \| `info` \| `debug` |
| `search` | case-insensitive substring over line text |
| `query` | case-insensitive substring over line text and source |
| `since_ms`, `until_ms` | epoch-ms bounds, inclusive |
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
`telemetry_set_interval`. Body: `{ "ms": 2000 }` → `{ "ok": true }`.
Persists immediately; the running poller's cadence changes at app
restart (the app's semantics, kept and documented).

#### `POST /api/v1/telemetry/reset-peaks` — `control`
`telemetry_reset_peaks` → `{ "ok": true }`.

#### `POST /api/v1/telemetry/snapshots` — `control`
`telemetry_save_snapshot` → `{ "path": "…" }`. The snapshot is a file
on the managed machine; the API returns its path as metadata (the
diagnostics bundle is the way to get telemetry off the machine —
§6.11). See §7 on path-returning commands.

### 6.10 Operations

#### `GET /api/v1/operations` — `read`
`tasks_list` → `{ "operations": [ Operation, … ] }` (§3.1), newest
first.

#### `GET /api/v1/operations/{id}` — `read`
One operation. The app has no get-by-id command; the server filters
`tasks_list`. `404` when absent.

#### `POST /api/v1/operations/{id}/abort` — `control`
`task_abort` → the operation's resulting state. Aborting is
best-effort per kind (§3.1); an already-terminal operation returns
its state unchanged.

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

#### `POST /api/v1/updates/check` — `control`
`updates_check_all` → per-component check results (the app's
payload).

#### `POST /api/v1/updates/components/{component_id}/apply` — `control`
`updates_apply`. **Specified but deferred:** component updates are
deferred in the app itself (`ApiError::Unsupported` — "Updates are
P6"). The endpoint returns `501 UNSUPPORTED` until the app
implements it; the contract reserves the path.

#### `PUT /api/v1/updates/components/{component_id}/pin` — `config`
`updates_pin`. Body: `{ "version": "…" | null }` →
`{ "ok", "persisted", "component_id", "pinned_version" }`.

#### `PUT /api/v1/updates/components/{component_id}/auto` — `config`
`updates_set_auto`. Body: `{ "enabled": true }` →
`{ "ok", "persisted", "component_id", "auto_update" }`.

### 6.15 Peripheral capabilities (MCP, VS Code, WinML)

These app capabilities exist but sit outside the manager's core
domains. They are specified here for completeness of the audit;
**recommendation: exclude from API v1** and add when a client need
exists (the Android control app has no use for VS Code setup or MCP
mode toggles).

| Endpoint | Class | App command |
|---|---|---|
| `GET /api/v1/mcp/servers` | read | `mcp_list` |
| `POST /api/v1/mcp/servers/{id}/test` | control | `mcp_test_connection` |
| `PUT /api/v1/mcp/servers/{id}/mode` | config | `mcp_set_mode` — body `{ "mode": "…" }` |
| `POST /api/v1/vscode/extensions` | control | `vscode_install_extensions` |
| `POST /api/v1/vscode/config` | config | `vscode_write_config` — body `{ "backup": true }` |
| `GET /api/v1/winml/status` | read | `winml_status` |
| `POST /api/v1/winml/probe` | control | `winml_probe` |
| `POST /api/v1/winml/register` | config | `winml_register` — body `{ "model_id": "…" }` |
| `POST /api/v1/winml/unregister` | config | `winml_unregister` |

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
   `diagnostics_export_bundle` return host filesystem paths. The
   snapshot endpoint returns the path as metadata only (there is no
   snapshot download endpoint — snapshots also ride inside
   diagnostics bundles). The bundle gets a real download endpoint
   (§6.11) because remote retrieval is its purpose; that serving
   step is an API-layer addition, not an app behavior.
5. **Activation apply orchestration** (§6.5): composed in the
   desktop UI today; the server must own it. The only place the API
   is more than a wrapper.
6. **`gateway_activation_complete`**: internal bookkeeping, not a
   client endpoint (§6.6).
7. **Streaming: none required, none offered.** No app capability
   streams: logs are paged queries, telemetry is poll snapshots,
   download progress is state polling. API v1 keeps the poll model
   throughout (operations, model state, telemetry snapshots). A
   future SSE/WebSocket channel for telemetry and log tailing is
   possible without breaking this contract, but no capability needs
   it to map completely.
8. **File pickers: none.** No audited command depends on a file
   dialog — model registration takes a URL, bundle export takes no
   destination. Nothing is lost to their absence remotely.
9. **`updates_apply`**: deferred in the app (`Unsupported`); the
   endpoint is a reserved path returning `501` (§6.14).
10. **Routing rules**: no dedicated commands exist; they move only
    as part of the gateway config patch. If rule-level CRUD is ever
    wanted, it is new app behavior first, API second.

## 8. Open questions for review

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
