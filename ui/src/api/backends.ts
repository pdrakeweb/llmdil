import { isBusyError } from "../lifecycle-state";
import type { TransitionWire } from "../lifecycle-state";
import { errOf, invoke, isTauri } from "./core";
import type { CallResult } from "./core";
import type { PredictedThroughput } from "./optimize";

/* ---------------- Backends (runtime state + supervision) ---------------- */

/** Outcome of a backend tool-call probe (MCP design §7.2), as the
 *  persisted config spells it (serde lowercase). */
export type ToolCallProbeOutcome = "verified" | "unparsed" | "failed";

/** A backend's persisted tool-call probe: the outcome plus when it
 *  ran (Unix epoch ms). Absent/null on a row = never probed. */
export interface ToolCallProbeData {
  outcome: ToolCallProbeOutcome;
  at_ms: number;
}

/** Result of backend_test_tool_call (manager-supervisor's
 *  ToolCallProbeResult): the probe outcome, the /props template
 *  evidence, and a human-readable detail line. */
export interface BackendToolCallTestResult {
  outcome: ToolCallProbeOutcome;
  finish_reason: string | null;
  chat_template_tool_use: boolean;
  chat_template_caps: unknown;
  detail: string;
  latency_ms: number;
}

export interface BackendRow {
  id: string;
  enabled: boolean;
  /** True when the backend comes from the app configuration (as opposed
   *  to a supervisor-only process unknown to the config). */
  configured: boolean;
  port: number;
  model_file: string;
  /** The unified lifecycle state (wire string of LifecycleState):
   *  running | starting | restarting | stopping | stopped | failed |
   *  degraded | unknown | not_installed — the same shared view in
   *  process and service mode. Normalize with fromWire(). */
  state: string;
  /** Same value as `state` (the field every lifecycle payload names
   *  `lifecycle`); absent on older payloads. */
  lifecycle?: string;
  /** The lifecycle operation in flight ({kind, origin, since}), or null. */
  transition?: TransitionWire | null;
  /** True when the row's state comes from the SCM (service mode). */
  service_managed?: boolean;
  pid: number | null;
  /** The llama-server child pid of a service-managed row (the shim
   *  is `pid`); null otherwise. */
  child_pid?: number | null;
  /** True when this session adopted the process (started by a previous
   *  launch; the app tracks it by pid rather than as its own child). */
  adopted: boolean;
  /** Supervisor crash-backoff attempt count for this backend. It counts
   *  crash-restart backoff attempts, not starts: nothing consumes it yet,
   *  so it reads 0 until a restart monitor exists (v0.1.2 has none). */
  restarts: number;
  uptime_secs: number | null;
  /** Gateway statistics from LiteLLM's Prometheus counters
   *  (present only once the metrics poller has snapshots; absent
   *  renders as "—", not zero). Requests/errors/health are the
   *  trailing hour; throughput_toks is the LIVE rate over the
   *  recent window — absent when the current rate is not
   *  measured (e.g. a request is in flight and its tokens are
   *  not counted yet), never a fabricated 0. */
  requests_1h?: number;
  errors_1h?: number;
  throughput_toks?: number;
  /** Trailing-hour success share (0–100), derived from the window
   *  in list_backends; absent when the window holds no requests. */
  health?: number;
  /** Sparkline series from the metrics snapshots: tokens/s for
   *  this backend's rows, requests/min for the gateway total the
   *  dashboard sums them into. Absent until two snapshots exist. */
  spark?: number[];
  req_spark?: number[];
  /** Configured llama-server sizing flags (configured rows only). */
  n_ctx?: number;
  n_batch?: number;
  /** Configured tensor split fractions in nvidia-smi index order
   *  (configured rows only); empty when the backend is unsplit. */
  tensor_split?: number[];
  /** The backend's saved raw llama-server flags, or null when it
   *  carries none (configured rows only). The card displays the
   *  string itself in advanced flags mode, and the flags panel
   *  pre-fills its text editor from it. */
  raw_flags?: string | null;
  /** Advanced flags mode (tracker #60): the backend's raw flags
   *  are edited directly as text and are the single source of
   *  truth; the structured flags form is disabled for the backend
   *  and the structured write paths refuse server-side. Absent
   *  (structured mode) on rows from older payloads. */
  flags_advanced?: boolean;
  /** Gateway group this backend belongs to, or null when ungrouped. */
  group?: string | null;
  /** Placement in short form ("GPU 0", "GPU 0+1", "CPU"),
   *  derived server-side from the saved configuration (Groups
   *  page, predicted-throughput design §8.2). Absent when the
   *  placement cannot be determined from the config. */
  placement?: string;
  /** Memory requirement under the backend's current configuration
   *  (configured rows whose model resolves): GPU-resident weights,
   *  system-RAM weights (expert offload), the KV cache at the
   *  configured context, and the recurrent-state cache at the
   *  effective slot count when the model's geometry is known. */
  gpu_ram_mib?: number;
  sys_ram_mib?: number;
  kv_ram_mib?: number | null;
  rs_ram_mib?: number | null;
  /** MoE expert-cache term of the requirement (`--moe-cache-mib`),
   *  MiB — 0 when the backend sets no cache. Itemized on the card
   *  beside the KV/state figures so the card's components are the
   *  start gate's components. */
  moe_cache_mib?: number;
  /** Three-state fit of the backend's requirement against its
   *  placed GPUs' total VRAM and driver models (server-computed
   *  from the same requirement model as the start gate). Absent
   *  when placement or telemetry cannot judge the backend. */
  fit?: BackendFit;
  /** The supervisor's recorded reason for the backend's last
   *  spawn/load failure, when one was recorded (e.g. the
   *  device-unavailable fail-fast verdict); null otherwise. */
  failure_reason?: string | null;
  /** How the requirement was derived: exact | catalog | estimate. */
  memory_basis?: string;
  /** Expert-offload slider state (configured rows whose model is
   *  offload-capable — MoE with a known expert share, MoE layer
   *  count, and a catalog green limit); absent otherwise. Byte
   *  counts let the form price any slider position with the same
   *  arithmetic as the backend memory requirement. */
  offload?: BackendOffload;
  /** MTP draft control state (configured rows whose model is
   *  MTP-capable — the catalog pins the capability); absent
   *  otherwise. The state is decoded from the backend's saved
   *  raw flags: MTP is on iff `--spec-type draft-mtp` is present. */
  mtp?: BackendMtp;
  /** MoE expert-cache control state (configured rows whose model
   *  is offload-capable — the offload slider's eligibility);
   *  absent otherwise. Decoded from the backend's saved raw
   *  flags: the cache is on iff `--moe-cache-mib N` (N > 0) is
   *  present. */
  moe_cache?: BackendMoeCache;
  /** Predicted throughput for the backend's SAVED
   *  configuration (predicted-throughput design §7): the
   *  exact-match measurement, in the measurement_lookup shape.
   *  Absent when no exact measurement exists — never
   *  null-as-zero — and independent of run state. */
  predicted?: PredictedThroughput;
  /** Parallel slots state (configured rows — the flags form's
   *  Parallel slots field renders for every backend, so this is
   *  not capability-gated); absent on supervisor-only orphan
   *  rows. Decoded from the backend's saved raw flags. */
  parallel?: BackendParallel;
  /** KV cache / flash-attention state (configured rows — the
   *  flags form's cache controls render for every backend, so
   *  this is not capability-gated); absent on supervisor-only
   *  orphan rows. Decoded from the backend's saved raw flags. */
  cache?: BackendCache;
  /** The backend's persisted tool-call probe (MCP design §7.2),
   *  or null when it has never been probed — the card renders null
   *  as "not measured", never as a measured state. Absent on
   *  supervisor-only orphan rows, which carry no probe. */
  tool_call_probe?: ToolCallProbeData | null;
  /** Transient offload-probe state for this backend, or null when
   *  no probe is running or has reported. */
  offload_probe?: BackendOffloadProbe | null;
}

/** Guardrail for a backend's expert-offload positions, derived from
 *  the GPU's compute capability. `status` describes the SAVED slider
 *  position; `overridden` means the status is blocked but a journaled
 *  override covers it, so starts proceed. */
export interface BackendOffloadGuardrail {
  /** GPU compute capability, e.g. "6.0". */
  compute_capability: string;
  allowed: "endpoints_only";
  status: "verified" | "untested" | "blocked";
  /** Closest verified slider position to the saved position. */
  nearest_verified_layers: number;
  has_override: boolean;
  overridden: boolean;
  /** The placed GPUs' compute capabilities ("6.0", …) — the set
   *  an override key names, in the order the backend places on
   *  them. The override confirmation and the set operation carry
   *  exactly this set. */
  placed_ccs: string[];
}

/** A backend's offload-probe state (list_backends payload). */
export interface BackendOffloadProbe {
  state: "probing" | "passed" | "failed";
  /** Slider position the probe covers. */
  layers: number;
  detail: string | null;
}

/** A backend's expert-offload slider state (list_backends payload). */
export interface BackendOffload {
  /** MoE layers in total (the slider's maximum). */
  moe_layers: number;
  /** Current position: MoE layers on system RAM. */
  layers: number;
  /** Routed-expert weight bytes (all MoE layers). */
  expert_bytes: number;
  /** Total model weight bytes. */
  total_bytes: number;
  /** The green limit in layers (the catalog default offload). */
  green_limit_layers: number;
  /** Slider band boundaries as fractions of the range. */
  green_end: number;
  yellow_end: number;
  /** System-RAM weight bytes at each slider position 0..=moe_layers,
   *  priced by the backend with true per-block sums. May be absent in
   *  older payloads — callers fall back to a uniform expert share. */
  sys_bytes_by_position?: number[];
  /** The `--n-cpu-moe` value each slider position 0..=moe_layers
   *  writes: llama.cpp reads it as a block-index bound, so for a
   *  model with leading or interleaved dense blocks it differs
   *  from the MoE-layer count. Absent in older payloads — callers
   *  fall back to the position itself. */
  n_cpu_moe_by_position?: number[];
  /** True when the saved offload spelling in raw flags is not the
   *  canonical slider form for its position (hand-written, e.g.
   *  --cpu-moe or -ncmoe). */
  custom: boolean;
  /** Offload guardrail for this backend's GPU, or null when none applies. */
  guardrail: BackendOffloadGuardrail | null;
}

/** A backend's MTP draft control state (list_backends payload).
 *  Present only when the backend's model is MTP-capable. */
export interface BackendMtp {
  /** Whether `--spec-type draft-mtp` is in the saved raw flags. */
  enabled: boolean;
  /** The explicit `--spec-draft-n-max`, or null when MTP is on
   *  without one (llama.cpp's default then applies) or off. */
  n_max: number | null;
  /** The explicit `--parallel` slot count in the saved raw
   *  flags, or null when unset. MTP drafting needs 1; any other
   *  explicit value is a conflict the form surfaces — the MTP
   *  control itself never rewrites it; the Parallel slots field
   *  (backend_set_parallel) is where it is set. */
  parallel: number | null;
  /** True when the saved MTP spelling is not the canonical form
   *  for its state (a bare `--spec-type draft-mtp` with no length
   *  token, an `=` spelling, ...). */
  custom: boolean;
}

/** A backend's MoE expert-cache control state (list_backends
 *  payload). Present only when the backend is offload-capable —
 *  the same eligibility as the offload slider (an MoE model with
 *  a resolvable layout). Decoded server-side from the backend's
 *  saved raw flags. */
export interface BackendMoeCache {
  /** The saved `--moe-cache-mib` value, or null when the cache
   *  is off (no token, or the pin's disabled value 0). */
  mib: number | null;
  /** True when the saved spelling is not the canonical form
   *  for its state (an `=` spelling, a decoded 0 token). */
  custom: boolean;
  /** The backend's saved offload position (MoE layers on
   *  system RAM). The control is disabled at 0: with no experts
   *  in system RAM there is nothing for the cache to hold. */
  offload_layers: number;
  /** The model's MoE layer count. */
  moe_layers: number;
}

/** The three-state fit classification shared by every fit
 *  surface: within total VRAM, over total only by WDDM paging,
 *  or over total on a hard-bounded (TCC / unreported) GPU. */
export type FitStateName = "fits" | "wddm_paged" | "does_not_fit";

/** One placed GPU's fit row (list_backends `fit.per_gpu`). */
export interface BackendFitGpu {
  gpu: number;
  required_mib: number;
  total_mib: number;
  driver_model: "wddm" | "tcc" | null;
  state: FitStateName;
  /** MiB paging to system RAM (WDDM-paged GPUs only; else 0). */
  paged_mib: number;
}

/** A backend's three-state fit (list_backends payload): the worst
 *  per-GPU state, the total paged amount, and the per-GPU rows. */
export interface BackendFit {
  state: FitStateName;
  paged_mib: number;
  per_gpu: BackendFitGpu[];
}

/** A backend's parallel-slots state (list_backends payload,
 *  configured rows). Decoded server-side from the backend's
 *  saved raw flags by the same decode pair the Parallel slots
 *  field writes through. */
export interface BackendParallel {
  /** The explicit `--parallel` slot count in the saved raw
   *  flags, or null when unset — llama.cpp's default (auto,
   *  -1) then applies. An explicit -1 is a saved auto. */
  slots: number | null;
  /** True when the saved parallel spelling is not the canonical
   *  `--parallel N` form for its state (an `-np` or `=` spelling,
   *  repeated tokens, a non-canonical position). */
  custom: boolean;
}

/** A KV cache type the cache controls offer (the pinned
 *  llama.cpp build accepts more; v1 offers these three). */
export type CacheTypeName = "f16" | "q8_0" | "q4_0";
/** The pinned build's flash-attention tri-state. */
export type FlashAttnName = "auto" | "on" | "off";

/** A full cache selection: K type, V type, flash-attention mode. */
export interface CacheSelectionLike {
  k: CacheTypeName;
  v: CacheTypeName;
  flash_attn: FlashAttnName;
}

/** A backend's KV cache / flash-attention state (list_backends
 *  payload, configured rows). Decoded server-side from the
 *  backend's saved raw flags by the same decode pair the cache
 *  controls write through. */
export interface BackendCache {
  /** The decoded K cache type (llama.cpp's f16 default when no
   *  token is saved). */
  k: CacheTypeName;
  /** The decoded V cache type. */
  v: CacheTypeName;
  /** The decoded flash-attention mode (auto when unset). */
  flash_attn: FlashAttnName;
  /** Whether any cache token is present in the saved flags —
   *  false for a pre-feature backend running at the defaults
   *  with no tokens. */
  present: boolean;
  /** True when the saved spelling is not the canonical form for
   *  its state (a `-ctk` or `=` spelling, a partial selection). */
  custom: boolean;
}

/** One rung of the predicted-best ladder (backend_cache_preview
 *  / backend_add_preview payloads). */
export interface CacheRungLike extends CacheSelectionLike {
  kv_mib: number;
  total_mib: number;
  fits: boolean;
}

/** The cache preview payload: the predicted footprint of one
 *  selection plus the predicted-best ladder for the placement. */
export interface CachePreview {
  selection: CacheSelectionLike;
  /** True for the pairing llama.cpp refuses (quantized V with
   *  flash attention off); the set op rejects it. */
  v_quantized_fa_off: boolean;
  weights_mib: number;
  sys_weights_mib: number;
  /** KV MiB at the selection, or null when the model's KV
   *  geometry is unknown (never zero-filled). */
  kv_mib: number | null;
  rs_mib: number | null;
  /** MoE expert-cache MiB priced into `total_mib` (the draft
   *  value the form sent, or the saved token) — itemized in the
   *  fit block so the rows sum to the total. */
  moe_cache_mib: number;
  total_mib: number;
  basis: string;
  /** Per placed GPU: its total VRAM, its required share of the
   *  total (the same distribution the fit verdict judges), and
   *  its own verdict against the headroom reserve. The share and
   *  verdict are null only when the server could not distribute
   *  the total over the targets. `fit_state` is the three-state
   *  classification against total VRAM (the start gate's basis)
   *  and `paged_mib` the WDDM paging amount for that GPU. */
  targets: { gpu: number; total_mib: number; required_mib: number | null; fits: boolean | null; driver_model?: "wddm" | "tcc" | null; fit_state?: FitStateName | null; paged_mib?: number }[] | null;
  headroom_reserve_percent: number;
  fits_current: boolean | null;
  ladder: CacheRungLike[];
  best: CacheRungLike | null;
}

/** list_backends — configured backends merged with live supervisor state. */
export async function getBackends(): Promise<CallResult<BackendRow[]>> {
  if (!isTauri()) return { data: [], live: false };
  try {
    const r = await invoke<{ backends: BackendRow[] }>("list_backends");
    return { data: Array.isArray(r.backends) ? r.backends : [], live: true };
  } catch (e) {
    return { data: [], live: true, error: errOf(e) };
  }
}

/** Options for the backend lifecycle wrappers. A bare boolean (the
 *  older signature) is `allowWddmOvercommit`. */
export interface BackendActionOpts {
  /** The WDDM-overcommit confirmation (the card's armed second click). */
  allowWddmOvercommit?: boolean;
  /** Who asked (ui, dashboard, activation, bench, optimize, …): named in
   *  the backend's lifecycle log markers. The server defaults to ui. */
  origin?: string;
  /** Stop/restart: drain in-flight requests first (default true). */
  graceful?: boolean;
  /** Restart only: wait up to this many seconds for /health before
   *  the call returns. */
  waitReadySecs?: number;
}

/** The start/stop/restart result. `busy` is set when the server refused
 *  because another lifecycle operation holds the backend (the error
 *  then names it). */
export type BackendActionResult = CallResult<{ task_id: string; backend_id: string }> & { busy?: boolean };

async function backendAction(
  cmd: "start_backend" | "stop_backend" | "restart_backend",
  id: string,
  optsOrAllow?: boolean | BackendActionOpts,
): Promise<BackendActionResult> {
  const opts: BackendActionOpts = typeof optsOrAllow === "boolean" ? { allowWddmOvercommit: optsOrAllow } : (optsOrAllow ?? {});
  if (!isTauri()) return { data: { task_id: "sample-task", backend_id: id }, live: false };
  try {
    const args: Record<string, unknown> = cmd === "start_backend" ? { id } : { id, graceful: opts.graceful ?? true };
    // Optional arguments ride only when set, preserving the previous
    // invoke shape for plain calls.
    if (opts.allowWddmOvercommit && cmd !== "stop_backend") args.allowWddmOvercommit = true;
    if (opts.origin) args.origin = opts.origin;
    if (cmd === "restart_backend" && typeof opts.waitReadySecs === "number") args.waitReadySecs = opts.waitReadySecs;
    const data = await invoke<{ task_id: string; backend_id: string }>(cmd, args);
    return { data, live: true };
  } catch (e) {
    const error = errOf(e);
    return { data: { task_id: "", backend_id: id }, live: true, error, busy: isBusyError(error) };
  }
}

export function startBackend(id: string, opts?: boolean | BackendActionOpts) { return backendAction("start_backend", id, opts); }
export function stopBackend(id: string, opts?: BackendActionOpts) { return backendAction("stop_backend", id, opts); }
export function restartBackend(id: string, opts?: boolean | BackendActionOpts) { return backendAction("restart_backend", id, opts); }

/** backend_update_flags — change a backend's context length and
 *  batch size. Persisted immediately; applies on the next start. */
export async function updateBackendFlags(id: string, nCtx: number, nBatch: number): Promise<CallResult<{ backend_id: string; persisted: boolean }>> {
  if (!isTauri()) return { data: { backend_id: id, persisted: false }, live: false };
  try {
    const data = await invoke<{ backend_id: string; persisted: boolean }>("backend_update_flags", { id, nCtx, nBatch });
    return { data, live: true };
  } catch (e) {
    return { data: { backend_id: id, persisted: false }, live: true, error: errOf(e) };
  }
}

/** backend_set_offload — set a backend's expert offload (the Edit
 *  Flags slider): `layers` MoE layers move to system RAM (0 = none,
 *  the model's full MoE layer count = the `-ot exps=CPU` form).
 *  Only the offload tokens in the backend's raw flags change.
 *  Persisted immediately; applies on the next start. When
 *  `allowOverride` is true it is sent as `allowOverride` (Tauri 2
 *  binds the Rust `allow_override` parameter from the camelCase key;
 *  the snake_case key was silently dropped, code review v0.1.60 UI-4), recording
 *  an explicit override for a guardrail-blocked position; the
 *  argument is omitted entirely when falsy, preserving the previous
 *  invoke shape for other callers. */
export async function setBackendOffload(id: string, layers: number, allowOverride?: boolean): Promise<CallResult<{ backend_id: string; layers: number; raw_flags: string | null; changed: boolean }>> {
  if (!isTauri()) return { data: { backend_id: id, layers, raw_flags: null, changed: false }, live: false };
  try {
    const args: Record<string, unknown> = { id, layers };
    if (allowOverride) args.allowOverride = true;
    const data = await invoke<{ backend_id: string; layers: number; raw_flags: string | null; changed: boolean }>("backend_set_offload", args);
    return { data, live: true };
  } catch (e) {
    return { data: { backend_id: id, layers, raw_flags: null, changed: false }, live: true, error: errOf(e) };
  }
}

/** The result of the deliberate guardrail-override operations. */
export interface OffloadOverrideResult {
  backend_id: string;
  layers: number | null;
  moe_layers: number | null;
  placed_ccs: string[] | null;
  has_override: boolean;
  overridden: boolean;
  /** Clear only: whether a standing record was removed. */
  cleared?: boolean;
  persisted: boolean;
}

/** backend_set_offload_override — record a deliberate guardrail
 *  override for a backend's SAVED offload position (the activation
 *  journal record #37 defined). `layers` and `placedCcs` name the
 *  exact key the operator confirmed on the card; the server
 *  re-derives the key from the live config and placement and
 *  refuses any mismatch, so the override covers exactly that
 *  position on exactly that placed compute-capability set. No
 *  flags change. Applies to starts from now on. */
export async function setBackendOffloadOverride(id: string, layers: number, placedCcs: string[]): Promise<CallResult<OffloadOverrideResult>> {
  if (!isTauri()) return { data: { backend_id: id, layers, moe_layers: null, placed_ccs: placedCcs, has_override: true, overridden: true, persisted: false }, live: false };
  try {
    const data = await invoke<OffloadOverrideResult>("backend_set_offload_override", { id, layers, placedCcs });
    return { data, live: true };
  } catch (e) {
    return { data: { backend_id: id, layers, moe_layers: null, placed_ccs: placedCcs, has_override: false, overridden: false, persisted: false }, live: true, error: errOf(e) };
  }
}

/** backend_clear_offload_override — drop a backend's standing
 *  guardrail override. The saved position then evaluates as it did
 *  before the override: a blocked position is blocked again.
 *  Idempotent: clearing with no record standing succeeds. */
export async function clearBackendOffloadOverride(id: string): Promise<CallResult<OffloadOverrideResult>> {
  if (!isTauri()) return { data: { backend_id: id, layers: null, moe_layers: null, placed_ccs: null, has_override: false, overridden: false, cleared: false, persisted: false }, live: false };
  try {
    const data = await invoke<OffloadOverrideResult>("backend_clear_offload_override", { id });
    return { data, live: true };
  } catch (e) {
    return { data: { backend_id: id, layers: null, moe_layers: null, placed_ccs: null, has_override: false, overridden: false, cleared: false, persisted: false }, live: true, error: errOf(e) };
  }
}

/** backend_set_mtp — set a backend's MTP speculative decoding
 *  draft length (tracker #53). `nMax` of 1..4 enables
 *  `--spec-type draft-mtp` with that `--spec-draft-n-max`;
 *  null turns MTP off. Only the MTP tokens in the backend's raw
 *  flags change — `--parallel` is never rewritten; the payload's
 *  `parallel` field lets the form surface a slot conflict
 *  instead. Persisted immediately; applies on the next start. */
export async function setBackendMtp(id: string, nMax: number | null): Promise<CallResult<{ backend_id: string; n_max: number | null; raw_flags: string | null; changed: boolean }>> {
  if (!isTauri()) return { data: { backend_id: id, n_max: nMax, raw_flags: null, changed: false }, live: false };
  try {
    const data = await invoke<{ backend_id: string; n_max: number | null; raw_flags: string | null; changed: boolean }>("backend_set_mtp", { id, nMax });
    return { data, live: true };
  } catch (e) {
    return { data: { backend_id: id, n_max: nMax, raw_flags: null, changed: false }, live: true, error: errOf(e) };
  }
}

/** backend_set_moe_cache — set a backend's MoE expert cache
 *  (`--moe-cache-mib`). A MiB value writes the canonical token;
 *  null turns the cache off; `auto: true` (with `mib` null) asks
 *  the server to resolve the size from priced free VRAM and
 *  reports it as `resolved_mib` (the saved `mib` then equals
 *  it). Only the cache token in the backend's raw flags changes.
 *  A nonzero size requires a saved offload above 0 — the cache
 *  holds the offloaded experts. The size is priced into the
 *  backend's memory requirement, so a start that cannot fit it
 *  is refused with the numbers. Persisted immediately; applies
 *  on the next start. */
export async function setBackendMoeCache(id: string, mib: number | null, auto?: boolean): Promise<CallResult<{ backend_id: string; mib: number | null; auto: boolean; resolved_mib: number | null; raw_flags: string | null; changed: boolean }>> {
  if (!isTauri()) return { data: { backend_id: id, mib, auto: !!auto, resolved_mib: null, raw_flags: null, changed: false }, live: false };
  try {
    const args: Record<string, unknown> = { id, mib };
    if (auto) args.auto = true;
    const data = await invoke<{ backend_id: string; mib: number | null; auto: boolean; resolved_mib: number | null; raw_flags: string | null; changed: boolean }>("backend_set_moe_cache", args);
    return { data, live: true };
  } catch (e) {
    return { data: { backend_id: id, mib, auto: !!auto, resolved_mib: null, raw_flags: null, changed: false }, live: true, error: errOf(e) };
  }
}

/** backend_set_parallel — set a backend's parallel slot count
 *  (tracker #61; the flags form's Parallel slots field). `slots`
 *  of 1..1024 writes the canonical `--parallel N`; null removes
 *  the token, returning the backend to llama.cpp's default
 *  (auto). Only the parallel tokens in the backend's raw flags
 *  change. Persisted immediately; applies on the next start. */
export async function setBackendParallel(id: string, slots: number | null): Promise<CallResult<{ backend_id: string; slots: number | null; raw_flags: string | null; changed: boolean }>> {
  if (!isTauri()) return { data: { backend_id: id, slots, raw_flags: null, changed: false }, live: false };
  try {
    const data = await invoke<{ backend_id: string; slots: number | null; raw_flags: string | null; changed: boolean }>("backend_set_parallel", { id, slots });
    return { data, live: true };
  } catch (e) {
    return { data: { backend_id: id, slots, raw_flags: null, changed: false }, live: true, error: errOf(e) };
  }
}

/** backend_set_cache — set a backend's KV cache types and
 *  flash-attention mode (the flags form's cache controls). All
 *  three values are set together and write the canonical
 *  `--cache-type-k <t> --cache-type-v <t> --flash-attn <m>`
 *  tokens; `sel` null clears the tokens, returning the backend
 *  to llama.cpp's defaults with no cache tokens. Only the cache
 *  tokens in the backend's raw flags change. A quantized V cache
 *  with flash attention off is refused server-side (llama.cpp
 *  fails context creation for it). Persisted immediately;
 *  applies on the next start. */
export async function setBackendCache(id: string, sel: CacheSelectionLike | null): Promise<CallResult<{ backend_id: string; selection: CacheSelectionLike | null; raw_flags: string | null; changed: boolean }>> {
  if (!isTauri()) return { data: { backend_id: id, selection: sel, raw_flags: null, changed: false }, live: false };
  try {
    const args: Record<string, unknown> = { id };
    if (sel) {
      args.cacheK = sel.k;
      args.cacheV = sel.v;
      args.flashAttn = sel.flash_attn;
    }
    const data = await invoke<{ backend_id: string; selection: CacheSelectionLike | null; raw_flags: string | null; changed: boolean }>("backend_set_cache", args);
    return { data, live: true };
  } catch (e) {
    return { data: { backend_id: id, selection: sel, raw_flags: null, changed: false }, live: true, error: errOf(e) };
  }
}

/** backend_cache_preview — price one cache selection for a
 *  backend (the flags form's predicted footprint and
 *  predicted-best ladder). Pure preview: nothing is written.
 *  Omitted arguments price the backend's saved values. */
export async function backendCachePreview(id: string, opts?: { nCtx?: number; slots?: number; sel?: CacheSelectionLike; moeCacheMib?: number }): Promise<CallResult<CachePreview>> {
  const empty: CachePreview = {
    selection: { k: "f16", v: "f16", flash_attn: "auto" },
    v_quantized_fa_off: false, weights_mib: 0, sys_weights_mib: 0,
    kv_mib: null, rs_mib: null, moe_cache_mib: 0, total_mib: 0, basis: "estimate",
    targets: null, headroom_reserve_percent: 5, fits_current: null,
    ladder: [], best: null,
  };
  if (!isTauri()) return { data: empty, live: false };
  try {
    const args: Record<string, unknown> = { id };
    if (opts?.nCtx !== undefined) args.nCtx = opts.nCtx;
    if (opts?.slots !== undefined) args.slots = opts.slots;
    if (opts?.moeCacheMib !== undefined) args.moeCacheMib = opts.moeCacheMib;
    if (opts?.sel) {
      args.cacheK = opts.sel.k;
      args.cacheV = opts.sel.v;
      args.flashAttn = opts.sel.flash_attn;
    }
    const data = await invoke<CachePreview>("backend_cache_preview", args);
    return { data, live: true };
  } catch (e) {
    return { data: empty, live: true, error: errOf(e) };
  }
}

/** One validation warning about a raw-flags string
 *  (backend_set_raw_flags payload). Warnings never block a save:
 *  they report unknown tokens, duplicates, and malformed values
 *  for flags the app understands. */
export interface RawFlagWarning {
  token: string;
  message: string;
}

/** backend_set_raw_flags — save a backend's raw flags edited as
 *  text (tracker #60, the advanced flags editor). Only available
 *  in advanced flags mode; the server refuses in structured mode,
 *  where the token-owning controls own the string. A null value
 *  clears the flags. Validation warnings ride the result and
 *  never block the save. Persisted immediately; applies on the
 *  next start. */
export async function setBackendRawFlags(id: string, rawFlags: string | null): Promise<CallResult<{ backend_id: string; raw_flags: string | null; changed: boolean; warnings: RawFlagWarning[] }>> {
  if (!isTauri()) return { data: { backend_id: id, raw_flags: rawFlags, changed: false, warnings: [] }, live: false };
  try {
    const data = await invoke<{ backend_id: string; raw_flags: string | null; changed: boolean; warnings: RawFlagWarning[] }>("backend_set_raw_flags", { id, rawFlags });
    return { data, live: true };
  } catch (e) {
    return { data: { backend_id: id, raw_flags: null, changed: false, warnings: [] }, live: true, error: errOf(e) };
  }
}

/** backend_set_flags_mode — turn a backend's advanced flags mode
 *  on or off (tracker #60). The flip writes only the mode: the
 *  backend's flags do not change by a single token in either
 *  direction, and the result carries the (unchanged) saved string
 *  the editor opens on. */
export async function setBackendFlagsMode(id: string, advanced: boolean): Promise<CallResult<{ backend_id: string; flags_advanced: boolean; raw_flags: string | null; changed: boolean }>> {
  if (!isTauri()) return { data: { backend_id: id, flags_advanced: advanced, raw_flags: null, changed: false }, live: false };
  try {
    const data = await invoke<{ backend_id: string; flags_advanced: boolean; raw_flags: string | null; changed: boolean }>("backend_set_flags_mode", { id, advanced });
    return { data, live: true };
  } catch (e) {
    return { data: { backend_id: id, flags_advanced: advanced, raw_flags: null, changed: false }, live: true, error: errOf(e) };
  }
}

/** backend_add — create a backend for a registered model and route it
 *  through a gateway group (an existing one, or a new one when
 *  createGroup is true). The gateway routes it after its next restart.
 *  The optional launch parameters (tracker #49) are the add form's
 *  fields: each one omitted keeps the creation default (32K context,
 *  512 batch, telemetry-assigned split, catalog default offload);
 *  offloadOverride records the operator's explicit override when a
 *  chosen offload position is guardrail-blocked, and gpuIndices is
 *  an explicit GPU selection replacing the telemetry assignment.
 *  parallel (tracker #61) is a chosen `--parallel` slot count
 *  written into the created backend's raw flags; omitted writes
 *  no token (llama.cpp's default). */
export async function addBackend(input: {
  id: string;
  modelId: string;
  port: number;
  group: string;
  createGroup: boolean;
  enabled: boolean;
  nCtx?: number;
  nBatch?: number;
  offloadLayers?: number;
  offloadOverride?: boolean;
  gpuIndices?: number[];
  parallel?: number;
  /** A chosen cache selection, written into the created
   *  backend's raw flags; omitted writes no cache tokens. */
  cacheK?: CacheTypeName;
  cacheV?: CacheTypeName;
  flashAttn?: FlashAttnName;
}): Promise<CallResult<{ backend_id: string; group: string }>> {
  if (!isTauri()) return { data: { backend_id: input.id, group: input.group }, live: false };
  try {
    const args: Record<string, unknown> = {
      id: input.id,
      modelId: input.modelId,
      port: input.port,
      group: input.group,
      createGroup: input.createGroup,
      enabled: input.enabled,
    };
    if (input.nCtx !== undefined) args.nCtx = input.nCtx;
    if (input.nBatch !== undefined) args.nBatch = input.nBatch;
    if (input.offloadLayers !== undefined) args.offloadLayers = input.offloadLayers;
    if (input.offloadOverride) args.offloadOverride = true;
    if (input.gpuIndices !== undefined) args.gpuIndices = input.gpuIndices;
    if (input.parallel !== undefined) args.parallel = input.parallel;
    if (input.cacheK !== undefined) args.cacheK = input.cacheK;
    if (input.cacheV !== undefined) args.cacheV = input.cacheV;
    if (input.flashAttn !== undefined) args.flashAttn = input.flashAttn;
    const data = await invoke<{ backend_id: string; group: string }>("backend_add", args);
    return { data, live: true };
  } catch (e) {
    return { data: { backend_id: input.id, group: input.group }, live: true, error: errOf(e) };
  }
}

/** The add form's preview payload (backend_add_preview): what
 *  creating a backend for a model would carry at the creation
 *  defaults — the offload slider state in the same shape a backend
 *  row carries (null when the model is not offload-capable), and
 *  the placement creation would use. */
export interface BackendAddPreview {
  /** The previewed model (echoed by the op; absent in the browser mock). */
  model_id?: string;
  offload: BackendOffload | null;
  /** The cache preview for the backend creation would build
   *  (predicted footprint + predicted-best ladder), or null when
   *  the model's layout cannot be resolved. */
  cache: CachePreview | null;
  tensor_split: number[];
  split_mode: string;
  /** The creation requirement (the backend card's gpu_ram_mib /
   *  sys_ram_mib figures for the backend creation would build, at
   *  the form's context/slot draft) plus the per-GPU split shares
   *  and residency; null when the model's layout cannot be
   *  resolved. Absent from older payloads. */
  memory?: BackendAddMemory | null;
}

/** backend_add_preview's `memory` object. per_gpu lists only the
 *  GPUs the previewed placement uses (empty without telemetry). */
export interface BackendAddMemory {
  gpu_ram_mib: number;
  sys_ram_mib: number;
  kv_ram_mib: number | null;
  rs_ram_mib: number | null;
  moe_cache_mib: number;
  basis: string | null;
  per_gpu: { gpu: number; share: number; required_mib: number; total_mib: number }[];
}

/** backend_add_preview — the add form's model-dependent parameter
 *  state: the offload control's range/bands/guardrail for the model
 *  and the placement a creation would assign. gpuIndices previews
 *  an explicit GPU selection instead of the telemetry assignment. */
export async function backendAddPreview(modelId: string, gpuIndices?: number[], nCtx?: number, parallel?: number): Promise<CallResult<BackendAddPreview>> {
  if (!isTauri()) return { data: { offload: null, cache: null, tensor_split: [], split_mode: "none" }, live: false };
  try {
    const args: Record<string, unknown> = { modelId };
    if (gpuIndices !== undefined) args.gpuIndices = gpuIndices;
    if (nCtx !== undefined) args.nCtx = nCtx;
    if (parallel !== undefined) args.parallel = parallel;
    const data = await invoke<BackendAddPreview>("backend_add_preview", args);
    return { data, live: true };
  } catch (e) {
    return { data: { offload: null, cache: null, tensor_split: [], split_mode: "none" }, live: true, error: errOf(e) };
  }
}

/** backend_delete — remove a backend and its group memberships.
 *  Refused while the backend is running. */
export async function deleteBackend(id: string): Promise<CallResult<{ backend_id: string; deleted: boolean }>> {
  if (!isTauri()) return { data: { backend_id: id, deleted: true }, live: false };
  try {
    const data = await invoke<{ backend_id: string; deleted: boolean }>("backend_delete", { id });
    return { data, live: true };
  } catch (e) {
    return { data: { backend_id: id, deleted: false }, live: true, error: errOf(e) };
  }
}

/** backend_get_launch_command — the exact argv the supervisor will run. */
export async function getBackendLaunchCommand(id: string): Promise<CallResult<{ id: string; argv: string[] }>> {
  if (!isTauri()) return { data: { id, argv: [] }, live: false };
  try {
    const data = await invoke<{ id: string; argv: string[] }>("backend_get_launch_command", { id });
    return { data, live: true };
  } catch (e) {
    return { data: { id, argv: [] }, live: true, error: errOf(e) };
  }
}

/** One backend's configuration as stored in AppConfig (backend_get_config). */
export interface BackendConfigData {
  id: string;
  enabled: boolean;
  model_file: string;
  port: number;
  flags: Record<string, unknown>;
  raw_flags: string | null;
  /** Advanced flags mode (tracker #60); absent on older payloads. */
  flags_advanced?: boolean;
  restart_policy: Record<string, unknown>;
  overrides_global: boolean;
}

/** backend_get_config — a backend's stored configuration. */
export async function getBackendConfig(id: string): Promise<CallResult<BackendConfigData | null>> {
  if (!isTauri()) return { data: null, live: false, error: "backend_get_config is not available in sample mode" };
  try {
    const data = await invoke<BackendConfigData>("backend_get_config", { id });
    return { data, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

export type BackendApplyMode = "RestartNow" | "OnNextRestart";

export interface BackendApplyResult {
  backend_id: string;
  apply_mode: string;
  restart_task_id: string | null;
  persisted: boolean;
}

/** backend_apply_config — merge a patch into a backend's configuration.
 *  The Tauri boundary maps the camelCase `applyMode` argument onto the
 *  Rust `apply_mode` parameter, as with `stepId` elsewhere. */
export async function applyBackendConfig(
  id: string,
  patch: Record<string, unknown>,
  applyMode: BackendApplyMode,
): Promise<CallResult<BackendApplyResult>> {
  const fallback: BackendApplyResult = { backend_id: id, apply_mode: applyMode, restart_task_id: null, persisted: false };
  if (!isTauri()) return { data: fallback, live: false, error: "backend_apply_config is not available in sample mode" };
  try {
    const data = await invoke<BackendApplyResult>("backend_apply_config", { id, patch, applyMode });
    return { data, live: true };
  } catch (e) {
    return { data: fallback, live: true, error: errOf(e) };
  }
}

/** Result of test_backend (manager-supervisor's TestResult). A
 *  non-2xx HTTP status is reported here, not thrown: content is then
 *  usually null. */
export interface BackendTestResult {
  status: number;
  content: string | null;
  prompt_tokens: number | null;
  completion_tokens: number | null;
  latency_ms: number;
}

/** test_backend — one prompt against a running backend's
 *  /v1/chat/completions; returns the first choice's content plus usage. */
export async function testBackend(id: string, prompt: string): Promise<CallResult<BackendTestResult>> {
  const fallback: BackendTestResult = { status: 0, content: null, prompt_tokens: null, completion_tokens: null, latency_ms: 0 };
  if (!isTauri()) return { data: fallback, live: false, error: "test_backend is not available in sample mode" };
  try {
    const data = await invoke<BackendTestResult>("test_backend", { id, prompt });
    return { data, live: true };
  } catch (e) {
    return { data: fallback, live: true, error: errOf(e) };
  }
}
