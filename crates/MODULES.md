# Crate map — implementation contract

Rust workspace for LLMdil. Fourteen library
crates under `crates/` (workspace members) plus the Tauri shell in
`src-tauri/` (excluded from the workspace; path-deps to the crates).
The UI shell is in `ui/` (Vite + plain TypeScript; framework-independent
Tauri command boundary per spec).

This map was written at scaffold time and is kept current as the crate
inventory: the workspace is fully implemented (862 workspace tests at
v0.1.40), and the function lists below describe the implemented API
surface, not stubs. Conventions: every crate exposes its public API
from `src/lib.rs`. Shared versions live in the root
`[workspace.dependencies]`. Spec references below are to
`docs/technical-specification.md` (the original design record; where it
and the code differ, the code governs).

---

## manager-config — spec §10

Versioned, serde-typed application configuration. Single JSON file at
`%APPDATA%\local-llm-service-manager\config.json`. Secrets never stored —
only credential references.

**Types:** `AppConfig` (version, data_dir, view_modes, backends, gateway,
models, telemetry, mxc, winml, updates, notifications), `ViewMode`
(Appliance|Console|Topology), `BackendConfig` (id, enabled, model_file, port,
flags, raw_flags, restart_policy, overrides_global), `ServerFlags`
(n_ctx, n_batch, tensor_split, split_mode), `SplitMode` (None|Layer|Row),
`RestartPolicy`, `GatewayConfig` (port, groups, routing_rules, openrouter,
request_log_len), `ModelGroup` (name, members, strategy, fallbacks),
`RoutingStrategy` (SimpleShuffle|LeastBusy|LatencyBased),
`RoutingRule`, `CloudTierConfig`, `ModelConfig`, `TelemetryConfig`,
`MxcPolicyRef` (path, mode), `MxcMode` (Learning|Enforced|Disabled),
`WinMlConfig`, `UpdatePolicy`, `ComponentUpdate`, `NotificationConfig`,
`ConfigError`.

**Functions:**
`load(path: &PathBuf) -> Result<AppConfig, ConfigError>` (migrate, validate,
fall back to `.bak`); `load_detailed(path) -> Result<LoadReport, ConfigError>`
(same, plus `used_backup: bool` so the UI can banner the fallback per spec
§10); `save(config, path)` (validate, `.bak` rotation);
`validate(config)` (version == CURRENT_VERSION, ports unique/in range, backend
ids unique/non-empty/no-`:`, splits sum to 1.0, backend model refs exist —
compared separator- and case-insensitively); `migrate(raw:
serde_json::Value)`; `default_config()` (implemented); `default_data_dir()`
(implemented); `CURRENT_VERSION: u32 = 1`.

`ModelConfig` carries the spec §10 additive fields `source_url: String` and
`assigned_backends: Vec<String>` (both `#[serde(default)]`, so older files
without them still deserialize).

**`diff` module (spec §14 `backend_preview_diff` / `backend_apply_config`):**
`BackendPatch` (all-optional patch struct; `raw_flags: Option<Option<String>>`
distinguishes untouched/clear), `FieldChange` (field, old, new as JSON),
`ConfigDiff` (backend_id, changes, valid); `diff_config(backend_id, current,
patch)` (pure preview); `apply_config_diff(config, backend_id, patch)`
(applies, re-validates whole config, rolls the backend back on failure).

**`diagnostics` module (spec §14 `diagnostics_export_bundle`):**
`export_diagnostics(config, log_scopes, telemetry_snapshot_json, versions,
dest_dir)` — writes `diagnostics-<ms>.zip` (config.json, logs/\<scope\>.log,
telemetry.json, versions.txt); scope names sanitized against path traversal.
Takes plain data so this crate does not depend on supervisor/telemetry.

---

## manager-mcp — spec §8

MCP server registry: GitHub + Google Drive (`default_servers()`), modes
local | remote | disabled. Depends on `manager-supervisor` for lifecycle.
The seed also carries the curated gateway-exposed servers
(`brave_search`, `web_fetch`) that map to the gateway MCP surface by
exposure — see `docs/mcp-capabilities-design.md`; the gateway's source
of truth is `AppConfig.mcp.servers` in manager-config, generated into
the LiteLLM config by manager-gateway.

**Types:** `McpMode` (Local|Remote|Disabled), `McpTransport` (Stdio|Http),
`ServerSpec` (id, name, transport, mode, command/args, env_keys — keys only,
url, cred_ref — Credential Manager target name only, package),
`McpServerStatus` (id, name, mode, transport, running, env_keys), `McpError`.

**Functions:** `default_servers()`; `process_id(server_id)` (`"mcp-<id>"`);
`process_spec_for(server)` (Some only for local stdio; env values injected
by the caller at spawn); `spawn_local` / `stop_local` (via Supervisor);
`list_status(supervisor, servers)`; `credential_presence(server, exists)`
(maps env keys to presence via a caller predicate — values never read);
`editor_mcp_config(servers)` (`mcpServers` JSON for Continue/Cline; env
values emitted empty).

---

## manager-wizard — spec §4.1

Setup wizard step state machine. Ten ordered auto-run steps; audit mode
re-runs check-type steps without reinstalling.

**Types:** `StepState` (Queued|Running|Done|Failed|Skipped|Manual),
`RunMode` (Install|Audit), `WizardContext` (config, data_dir, mode, ops:
`Arc<dyn SystemOps>` — injectable system effects), `StepOutcome` (state,
message, log_tail, duration_ms), `WizardStep` trait (async: `id()`,
`name()`, `prerequisites() -> Vec<String>` — owned; the scaffold draft said
`&[String]`, evolved during implementation so steps aren't forced to store
the vec — semantics unchanged, `remediation_hint()` added for audit
remediation offers, `planned_commands(ctx)` — mode-aware, must equal spawned
argv (download/http-get/http-post are display conventions, documented on
`PlannedCommand`), `run(ctx)`), `PlannedCommand` (argv, cwd, env), `StepReport` (+
message, note), `WizardReport` (with `progress() -> (done, total)` and
`fraction()`), `Wizard` runner, `WizardError` (incl. `DuplicateStepId`), `SystemOps` trait
(run_command, spawn_detached, download, extract_zip, http_get, http_post,
read_file, write_file, file_exists, sha256_file, gpu_inventory,
credential_exists, home_dir), `RealSystemOps` (production impl, with connect/
HTTP timeouts), `FakeSystemOps` (hermetic test harness), `GpuDescriptor`,
`CommandOutput`, `HttpResponse`, `LlamaCppPin` (spec §3.4 cuda-12.4 pin +
Pascal validation).

**Functions:** `Wizard::new(steps, mode) ->
Result<Wizard, WizardError>` (duplicate step ids rejected, no panic),
`Wizard::run_all(ctx)` (ordered auto-run, prerequisite-gated),
`Wizard::run_step(ctx, id)`, `Wizard::retry(id)`, `Wizard::skip(id, note)`,
`Wizard::mark_manual(id, path, version)`, `Wizard::progress()`,
`Wizard::get_step(id)` (public step lookup for the "Show commands" UI),
`steps::continue_config_yaml(config)` (Continue `config.yaml` content:
every gateway group as an `openai` model whose `apiKey` references
`${{ secrets.LITELLM_API_KEY }}`), `steps::merge_continue_config_yaml`,
`steps::merge_env_contents`, and `steps::write_continue_files` (the
config.yaml + `.env` pair, written by the VscodeExtensions step; the
Tauri `vscode_write_config` command writes the same pair with
backup-first semantics).

---

## manager-supervisor — spec §2.2, §6

Process supervision: one llama-server per backend, LiteLLM gateway, local
MCP servers, optional WinMLServer. Graceful drain (30 s default, then kill),
exponential capped crash backoff, per-scope stdout/stderr ring buffers.

**Types:** `HealthStatus` (Healthy|Degraded|Unhealthy), `HealthProbe` trait
(async `probe()`), `HttpHealthProbe` (url, timeout), `BackoffPolicy`
(base, max, max_retries) with `delay_for_attempt()` (implemented) and
`allows_retry()` (implemented), `LogStream` (Stdout|Stderr), `LogRecord`,
`RingBuffer<T>` (implemented: new/push/len/recent), `SupervisedProcess`,
`Supervisor`, `SupervisorError`, `LogLevelFilter` (heuristic level match for
`logs_query`), `TestResult` (status/content/token usage/latency).

**Functions:** `SupervisedProcess::spawn(id, program, args, env,
log_ring)`, `drain(timeout)`, `kill()`; `Supervisor::new(backoff)`,
`spawn_backend(id)`, `stop(id, graceful)`, `restart(id, graceful)`,
`launch_argv(id)` — must equal the wizard's `planned_commands` output.
`Supervisor::logs_query(scope, level, search, limit)` (spec §14: newest-first
log query over live scopes and retained `dead_rings`; heuristic
`LogLevelFilter`), `test_request(base_url, prompt)` (spec §14
`backend_test_request`: minimal `/v1/chat/completions` probe returning
`TestResult`). Stopped/reaped processes retain their log rings in
`dead_rings` until the id is spawned again.

---

## manager-telemetry — spec §5

GPU telemetry. NVML primary, `nvidia-smi --query --xml` load-bearing
fallback. STALE after [`STALE_AFTER_SECS`] = 10 s without a successful poll;
last-known values shown, never synthesized. Source shown per GPU in UI.

**Types:** `TelemetrySourceKind` (Nvml|NvidiaSmi), `GpuInfo`
(index, name, uuid, compute_capability, total_vram_mib, source),
`GpuSample` (index, name, ts, utilization_pct, vram_used_mib,
vram_total_mib, temp_c, power_w, stale, source), `ProcessSample`
(pid, name, vram_mib, backend_id), `TelemetrySource` trait
(async `sample()`, `processes()`), `NvmlSource`, `NvidiaSmiSource`
(with `parse_xml(&str)`), `TelemetryStore`, `TelemetryError`,
`PeakMark<T>` (value + timestamp), `PeakMarks` (per-GPU resettable peaks:
`utilization_pct`, `vram_util_pct`, `vram_used_mib`, `temp_c`,
`power_w` — each `None` until the first sample raises it).

**Functions:** `TelemetrySource::sample/processes`,
`TelemetrySource::sample_with_processes` (default: two calls; overridden by
`NvidiaSmiSource` and `FallbackSource` for single-acquisition ticks),
`NvidiaSmiSource::parse_xml`, `TelemetryStore::new/update/snapshot`
(snapshot applies staleness), `TelemetryStore::peaks(index)` (peak marks
with timestamps, shown in the UI), `TelemetryStore::reset_peaks()` (spec §5:
peak marks are resettable), `default_telemetry_stack()` (NVML primary +
nvidia-smi fallback; the fallback is the default on machines without NVML).

---

## manager-gateway — spec §7

LiteLLM `config.yaml` generation from `AppConfig`. Generated file is never
hand-edited; UI previews a diff before apply.

**Types:** `LiteLlmModelEntry` (model_name, litellm_params),
`LiteLlmParams` (api_base, api_key, strategy), `OpenRouterTarget`
(enabled, model_allowlist, daily_cap_usd), `GatewayError`.
Reuses `manager_config::{AppConfig, ModelGroup, RoutingStrategy}`.

**Functions:** `generate_config_yaml(config) -> Result<String>`,
`model_entries_for_group(group, config)`,
`pick_strategy(group, latency_data_available)` (degrade latency-based →
simple-shuffle when no latency data). Generation also emits, when the
config calls for them: the routing-rules callback file
(`llmdil_routing.py`, with native `context_window_fallbacks`) and the
`mcp_servers:` block for armed gateway MCP servers (secrets as
`os.environ/` references) — see `docs/routing-rules-design.md` and
`docs/mcp-capabilities-design.md`.

**Routing trace + request ring (spec §14 `gateway_test_routing` /
`gateway_recent_requests`):** `RoutingDecision` (at, group, backend,
strategy used after degradation, human-readable reason),
`trace_routing(group, stats, latency_data_available, seed)` (pure: runs
`pick_backend` and describes the decision; the caller records it),
`RoutingLog` (in-memory ring, `ROUTING_LOG_CAP` = 50, `record` /
`recent` newest-first / `clear`; owned by the Tauri `AppState`).

Group/strategy defaults (spec §7): planner/latency-based → coder →
openrouter; coder-fast/least-busy → coder → openrouter;
coder/simple-shuffle → openrouter; tool-runner/least-busy (+ WinML :8090 if
registered) → openrouter.

---

## manager-models — spec §4.6

Model library: curated GGUF catalog, downloads with progress/pause/resume/
cancel, SHA-256 verification before "installed", fit-check vs discovered VRAM.

**Types:** `ModelSpec` (id, name, quant, size_bytes, sha256, url,
params_b, ctx_max, enabled_by_default, expert_bytes, moe_layers,
default_offload_layers, offload_restrictions, mtp — the MTP capability
pin, set only on `qwen3.8-27b-mtp` at `CATALOG_REVISION` 3),
`DownloadProgress` (downloaded_bytes, total_bytes; `fraction()`
implemented), `DownloadHandle` (async `pause/resume/cancel`),
`FitVerdict` (Fits{gpu} | FitsWithSplit{split} | TooLarge{needed_mib,
free_mib}), `ModelError`.

**Functions:** `download(spec, dest, on_progress)` (HTTP range
resume), `verify_sha256(path, expected_hex)`,
`fit_check(spec, free_vram_mib_per_gpu)`, `delete_model_file(model,
backends)` (guarded delete, spec §14 `model_delete`: refuses with
`ModelError::InUse` when any backend's `model_file` points at the model's
`gguf_path` or appears in its `assigned_backends`; otherwise deletes the
file — a missing file is not an error — and returns `DeleteReport` (id,
path, file_existed, bytes_freed)).

**`placement` module (spec §14 `placement_propose` / `placement_apply`):**
`PlacementProjection` (model_id, split, per_gpu_mib, per_gpu_fits, fits,
verdict, sys_mib, gpu_weights_mib), `validate_split(split, n_gpus)`,
`propose_placement(spec, split, free_vram_mib_per_gpu)` (explicit-split
projection; allocation = size × fraction rounded up),
`propose_placement_resident(spec, split, free_vram_mib_per_gpu,
gpu_resident_bytes, sys_bytes)` (same projection over the GPU-resident
weight share under expert offload),
`apply_placement(backend: &mut BackendConfig, split)` (validates, writes
`flags.tensor_split`; implies `SplitMode::Layer` only when mode is `None` —
never clobbers an explicit `Row`). Dependency direction: `manager-models`
→ `manager-config`; `manager-config` never depends back.

**`gguf` module:** `parse_gguf_layout(header_prefix)` — parses a GGUF
header (metadata + tensor directory) into `GgufLayout` (tensor_bytes,
expert_bytes, block_count, KV geometry incl. `full_attention_interval`
folding); `GgufError::Incomplete` tells the caller to read a longer
prefix. **`memory` module:** `expert_offload(raw_flags)` (detects
`-ot exps=CPU` / `--n-cpu-moe` in backend raw flags) and
`memory_requirement(...)` → `MemoryRequirement` (gpu_weights_mib,
sys_weights_mib, kv_mib, basis Exact/Catalog/Estimate) — the per-backend
GPU/system RAM requirement the backend cards report and the placement
projection consumes.

**`mtp` module:** MTP speculative decoding in raw flags — decode/rewrite
of `--spec-type draft-mtp` and `--spec-draft-n-max` mirroring the
`memory` module's offload pair (state decoded from the saved raw flags;
the writer rewrites only its own tokens; `--parallel` is never
rewritten). **`guardrails` module:** per-model × per-architecture
expert-offload guardrails pinned in the catalog
(`ModelSpec::offload_restrictions`; see `docs/offload-guardrails.md`) —
e.g. Qwen3-Coder-Next-80B on compute capability (6,0) is endpoints-only
(off or full offload).

---

## manager-mxc — spec §11

MXC sandbox policy: default-deny, tool execution only (inference stays on
host). Secrets never in policy JSON — `CredentialRef` holds only the
Credential Manager target name. Begin in Learning mode, derive enforced
policy from the activity report.

**Types:** `PolicyDecision` (Allow|Prompt|Deny), `FsAccess`
(Read|Write|ReadWrite|Deny), `FsRule` (path, access), `NetRule`
(host, ports, decision), `CredentialRef` (name, target), `Policy`
(version, filesystem, network, credentials, default_deny) with
`default_policy()` (implemented), `AccessKind`
(FileRead|FileWrite|Network), `AccessRequest`, `ValidationReport`,
`LearningEntry`, `LearningReport`, `MxcError`.

**Functions:** `decide(policy, request)` (default-deny on no match),
`validate(policy)` (no secrets, absolute paths, ports in range,
default-deny set, no allow-all), `parse_learning_report(json)`,
`filter_since(report, since_ms)` (keep entries at/after the cutoff;
unparseable timestamps kept).

**`self_test` module (spec §4.1 wizard step 9, §14 `mxc_run_self_test`):**
`self_test(policy) -> Vec<SelfTestCase>` — a fixed set of 12 benign
(project file read/write, tool-runtime read, loopback gateway/backend,
GitHub API) and malicious (SSH key read/write, credential-store read,
Documents read, non-loopback egress, unknown-host HTTP) probes evaluated
with `decide`; each case carries expected vs actual and `passed`. Harmless
by construction: pure policy evaluations, nothing executed.
`self_test_with_env` (explicit `%VAR%` resolver), `summarize_self_test`,
`ProbeKind` (Benign|Malicious), `SelfTestCase`, `SelfTestSummary`.

---

## manager-winml — spec §6.5

Windows ML secondary backend: optional WinMLServer on :8090, registered in
LiteLLM as a plain OpenAI-compatible backend. Single-GPU tool-runner-class
models only; never planner/coder-30b. Researched limits bannered in UI.

**Types:** `GpuSelection` (SystemDefault|TargetGpuCoarse),
`WinMlLimits` (supports_tensor_split, max_registered_models,
gpu_selection, experimental) with `known_limits()` (implemented:
no tensor split, 1 model, coarse GPU selection, experimental),
`WinMlRegistration` (model_id, port, endpoint, registered_at,
responding_adapter, limits), `WinMlError`.

**Functions:** `probe()` (record limits verbatim), `register(config,
model_id)` (refuse ineligible model classes), `unregister()`,
`health(port)`.

---

## src-tauri — spec §2, §14

Tauri 2 shell (excluded from the Cargo workspace). Its command adapters
and `generate_handler!` list are generated from the manager-app
operation registry (`for_each_operation!`) — the shell declares no
operations of its own. `tauri.conf.json`: productName "LLMdil", version
0.1.40, identifier `com.pdrakeweb.llm-manager` (unchanged from the
pre-rename product), publisher pdrakeweb, category DeveloperTool, NSIS
per-user install, English, LICENSE, Start Menu shortcut, no desktop
shortcut, `installer-hooks.nsi` (uninstall asks before removing
`%APPDATA%\local-llm-service-manager`; finish offers Launch). The NSIS
template is vendored (`installer-template.nsi`) with the upgrade-options
page patches. See `src-tauri/INSTALLER.md` for wizard pages and silent
flags (`/S`, `/D=`). Does not compile on the Linux VM (no GTK/WebKit);
kept syntactically valid — run its check/clippy inside `src-tauri/`
before any release, as the workspace commands exclude it.

## ui/

Vite + plain TypeScript (no framework). `npm run build` emits `ui/dist`.
`src/main.ts` is the shell (router, sidebar, polling); views live in
`src/views/` (twelve routes: dashboard with Appliance | Console |
Topology modes, backends, models, gateway — including routing rules —
MCP servers, activation plan, Optimize, logs, GPUs, service map,
settings, wizard). All backend access
goes through `src/api.ts` invoke wrappers — a smoke script
(`ui/scripts/smoke.mjs`) guards the invoke-name contract against the
registry.

## manager-app — admin API plan §3.1

Service core extracted from the Tauri shell (admin API plan, phase 0):
`AppState` and its task/download registries, all 108 operation
implementations at v0.1.40 (`op_*`, framework-free, typed `ApiError`), the
activation journal (`activation`), and the gateway stats sampler
(`gateway_stats`). App self-update lives in `app_update`: the
GitHub-Releases check, installer staging with SHA-256
verification, the state file under `<data_dir>/app-update/`, and
the apply-helper entry the shell runs before the UI starts. `registry` holds the single operation
declaration (`for_each_operation!`) from which the `Op` enum,
`OP_SPECS`, `dispatch()`, the Tauri command adapters, and the REST
router are all generated — see `docs/admin-api/PLAN.md` §3.1.
Events leave through the `EventSink` trait (Tauri sink in the
shell, broadcast sink in manager-api).

## manager-present

Canonical number presentation for user-facing quantities (Pete,
2026-10-15: raw floats like 15.9921875 GB are not a useful
presentation). Pure functions, no dependencies: `trim1` (one decimal
at most, trailing ".0" dropped, whole numbers at magnitude >= 100),
`gb`, `bytes`, `mib`, `count`, `percent`, `rate`, `latency_ms`,
`duration_secs`. The TypeScript UI mirrors the same rules in
`ui/src/charts.ts` (`trim1`/`fmtGb`); wizard step messages format
through this crate.

## manager-api — admin API plan

Admin REST API (axum). The router is generated from manager-app's
operation registry (`OP_SPECS`): every registry operation with a route
becomes an endpoint (66 routed operations at v0.1.40); route = None
operations are desktop-only. Owns the listener lifecycle (bind
`0.0.0.0:47821` when enabled, TLS with a self-signed certificate whose
SHA-256 fingerprint is pinned at enrollment, or loopback plaintext in
dev), the auth layer (argon2id password hashing, opaque rotating
sessions with reuse → family revocation, lockout, TOTP + recovery
codes, enrollment codes; users in `admin-users.json`), the
role/capability authorization table (from the registry's classes), the
mutation audit log, and the SSE event stream at `GET /events`. See
`docs/admin-api/` for the plan and the as-built route table.

## manager-optimize — docs/optimize-design.md

Core logic for the Optimize page; Tauri-free and pure over its inputs.
`plan` (plan document types, tri-state item decisions, apply outcomes,
item fingerprints), `parse` (salvage parser from a backend's raw
consultation response into a `Plan`, including the plan-build model
grounding pass: unregistered models get a chained `model.add` or the
item is excluded into `Plan.excluded_items` with a reason), `context`
(the allowlist `OptimizeContext` builder — secrets excluded by
construction — plus the consultation prompt builder and candidate
ranking). The stateful runners (consultation, measurements,
experiments) live in manager-app (`optimize`, `optimize_eval`,
`optimize_experiments`).

## manager-services — docs/windows-services-design.md

Windows service supervision. Every backend and the gateway can be
registered as a Windows service whose ImagePath is the in-repo shim
(`llm-service-host.exe`) plus a pointer to a JSON service definition in
`<data_dir>/services/`. Definitions are generated from the same specs
process supervision spawns from, so service mode cannot drift from
process mode's flags, env, or ports; updating a definition is a file
rewrite (no elevation, no SCM call) and takes effect at the service's
next start. SCM create/delete is the only elevated operation; services
run as the user's account with Manual start. Per-backend
`supervision.mode` = `process` (default) | `service`.
