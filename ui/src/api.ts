/* Tauri command boundary.
   - Under Tauri: calls window.__TAURI__.core.invoke and returns live data.
   - Plain browser: falls back to built-in mock data and reports live=false,
     so the UI renders sample data with a "disconnected" banner instead of failing.
   - Every invoke is wrapped in try/catch. On failure the result carries
     live=true (we ARE under Tauri) plus error=<message> and degraded data —
     callers must render a degraded banner, never present the fallback as
     healthy live data.
*/

import { trim1 } from "./charts";
import { mockBackends, mockDashboard, mockGpus, mockOptimizeState, mockWizardSteps } from "./mock";
import type { BackendInfo, DashboardModel, GpuInfo, Status, StatusResponse, WizardStepState } from "./types";

export {}; // module marker

declare global {
  interface Window {
    __TAURI__?: {
      core: { invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> };
    };
  }
}

export function isTauri(): boolean {
  return typeof window.__TAURI__?.core.invoke === "function";
}

async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const fn = window.__TAURI__?.core.invoke;
  if (!fn) throw new Error("not under Tauri");
  return fn<T>(cmd, args);
}

export interface CallResult<T> {
  data: T;
  live: boolean;
  /** Set when the backend call failed; data is degraded (empty/placeholder). */
  error?: string;
}

function errOf(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** get_status — always available shape; mock when disconnected. */
export async function getStatus(): Promise<CallResult<StatusResponse>> {
  if (isTauri()) {
    try {
      const data = await invoke<StatusResponse>("get_status");
      return { data, live: true };
    } catch (e) {
      return { data: { status: "unreachable", version: "?" }, live: true, error: errOf(e) };
    }
  }
  return { data: { status: "ok", version: "0.1.0-sample" }, live: false };
}

/** Supervisor process state -> dashboard Status vocabulary. */
const BACKEND_STATUS: Record<string, Status> = {
  running: "ok",
  starting: "starting",
  stopping: "degraded",
  failed: "down",
  stopped: "idle",
};

/** list_backends — returns { backends: BackendRow[] } (configured backends
 *  merged with live supervisor state). Each row is mapped to the UI-side
 *  BackendInfo: name = id, model = model_file, status from the supervisor
 *  state. Rows also carry trailing-hour gateway statistics
 *  (requests_1h / errors_1h / throughput_toks) when the metrics poller
 *  has a window — those come from LiteLLM's Prometheus counters via
 *  the Rust stats store; absent fields render an honest placeholder,
 *  never a fabricated measurement. Rows carry `configured`: false
 *  marks a supervisor-only orphan (a process still live under an id
 *  that left the config); the Backends view badges those "not in
 *  config" and offers Stop only. */
export async function listBackends(): Promise<CallResult<BackendInfo[]>> {
  if (isTauri()) {
    try {
      const r = await invoke<{ backends: BackendRow[] }>("list_backends");
      const rows = Array.isArray(r.backends) ? r.backends : [];
      const data: BackendInfo[] = rows.map((b) => ({
        id: b.id,
        name: b.id,
        port: b.port,
        model: b.model_file,
        status: BACKEND_STATUS[b.state] ?? "idle",
        adopted: b.adopted,
        enabled: b.enabled,
        configured: b.configured,
        service_managed: b.service_managed,
        requests_1h: typeof b.requests_1h === "number" ? b.requests_1h : undefined,
        errors_1h: typeof b.errors_1h === "number" ? b.errors_1h : undefined,
        throughput_toks: typeof b.throughput_toks === "number" ? b.throughput_toks : undefined,
        health: typeof b.health === "number" ? b.health : undefined,
        spark: Array.isArray(b.spark) ? b.spark : undefined,
        reqSpark: Array.isArray(b.req_spark) ? b.req_spark : undefined,
      }));
      return { data, live: true };
    } catch (e) {
      return { data: [], live: true, error: errOf(e) };
    }
  }
  return { data: mockBackends(), live: false };
}

/** One GPU sample as sent by get_gpu_telemetry (manager-telemetry). */
interface GpuSample {
  index: number;
  name: string;
  ts: string;
  utilization_pct: number;
  vram_used_mib: number;
  vram_total_mib: number;
  temp_c: number;
  power_w: number | null;
  stale: boolean;
  /** True when the GPU was absent from the latest polls after being
   *  reported before (e.g. fallen off the bus): the store retains it
   *  flagged down with last-known readings. Absent on older payloads. */
  down?: boolean;
  source: string;
  mem_util_pct: number | null;
  fan_pct: number | null;
  power_limit_w: number | null;
  temp_peak_c: number | null;
  /** History sparkline arrays from the telemetry store, attached
   *  by get_gpu_telemetry when at least two samples exist. */
  util_spark?: number[];
  temp_spark?: number[];
  power_spark?: number[];
}

/** One per-process VRAM attribution entry as sent by get_gpu_telemetry.
 *  The Rust ProcessSample names the VRAM field `vram_mib`; `used_mib` is
 *  accepted as a fallback in case the field is renamed on the Rust side.
 *  `vram_mib` is null when the figure was not measured (WDDM GPU,
 *  counter data unavailable) — unknown, never to be coerced to 0. */
interface GpuProcess {
  gpu_index: number;
  pid: number;
  name?: string;
  vram_mib?: number | null;
  used_mib?: number;
  backend_id: string | null;
}

/** TelemetrySourceKind serializes lowercase ("nvml" | "nvidiasmi");
 *  render it the way operators name the tool. */
function sourceLabel(raw: string): string {
  if (raw === "nvidiasmi") return "nvidia-smi";
  if (raw === "nvml") return "NVML";
  return raw;
}

export interface GpuTelemetryData {
  gpus: GpuInfo[];
  stale: boolean;
  /** Snapshot timestamp (epoch ms) from the payload — the time the
   *  samples were polled, not the time the command answered; null in
   *  sample mode or before the first poll. */
  atMs: number | null;
  /** True once the telemetry poller has completed at least one poll.
   *  False with an empty GPU list means "not polled yet", not "no
   *  GPUs" — the view renders a waiting state for it. */
  hasPolled: boolean;
  /** Display-ready telemetry source ("nvidia-smi" / "NVML"), from the
   *  first sample; null when no sample carries one. */
  source: string | null;
  /** Last poll error from the telemetry source, when it is failing;
   *  null when polls are succeeding (or in sample mode). */
  sourceError: string | null;
}

/** get_gpu_telemetry — returns { samples, processes, at_ms,
 *  source_error }. Each sample is mapped to the UI-side GpuInfo:
 *  id = String(index), VRAM converted MiB -> GiB, status degraded when
 *  the sample is stale, the nullable temp_peak_c / power_limit_w /
 *  fan_pct / mem_util_pct become optional fields (undefined when the
 *  payload carries null), and the payload's per-process attribution is
 *  attached to its GPU (backend "—" when unattributed). The payload has
 *  no pci/driver data, so those GpuInfo fields stay undefined on the
 *  live path; sparkline arrays ride the samples from the telemetry
 *  store's history. Overall stale is true when any sample is stale. */
export async function getGpuTelemetry(): Promise<CallResult<GpuTelemetryData>> {
  if (isTauri()) {
    try {
      const r = await invoke<{ samples: GpuSample[]; processes?: GpuProcess[]; at_ms: number | null; has_polled?: boolean; source_error?: string | null }>("get_gpu_telemetry");
      const samples = Array.isArray(r.samples) ? r.samples : [];
      const processes = Array.isArray(r.processes) ? r.processes : [];
      const gpus: GpuInfo[] = samples.map((s) => ({
        id: String(s.index),
        name: s.name,
        util_pct: s.utilization_pct,
        vram_used_gb: s.vram_used_mib / 1024,
        vram_total_gb: s.vram_total_mib / 1024,
        temp_c: s.temp_c,
        temp_peak_c: s.temp_peak_c ?? undefined,
        power_w: s.power_w,
        util_spark: Array.isArray(s.util_spark) && s.util_spark.length >= 2 ? s.util_spark : undefined,
        temp_spark: Array.isArray(s.temp_spark) && s.temp_spark.length >= 2 ? s.temp_spark : undefined,
        power_spark: Array.isArray(s.power_spark) && s.power_spark.length >= 2 ? s.power_spark : undefined,
        power_limit_w: s.power_limit_w ?? undefined,
        fan_pct: s.fan_pct ?? undefined,
        mem_util_pct: s.mem_util_pct ?? undefined,
        source: s.source ? sourceLabel(s.source) : undefined,
        processes: processes
          .filter((p) => p.gpu_index === s.index)
          .map((p) => ({
            process: p.name ? p.name : "—",
            pid: p.pid,
            // Unmeasured stays null end to end: coercing it to 0
            // here is how a resident model on a WDDM GPU used to be
            // rendered as "context only".
            vram_mib: typeof p.vram_mib === "number" ? p.vram_mib
              : typeof p.used_mib === "number" ? p.used_mib : null,
            backend: p.backend_id ?? "—",
          })),
        status: s.down ? "down" : s.stale ? "degraded" : "ok",
      }));
      const stale = samples.length > 0 && samples.every((s) => s.stale)
        ? true
        : samples.some((s) => s.stale);
      return {
        data: {
          gpus,
          stale,
          atMs: typeof r.at_ms === "number" ? r.at_ms : null,
          hasPolled: typeof r.has_polled === "boolean" ? r.has_polled : samples.length > 0,
          source: samples.length && samples[0].source ? sourceLabel(samples[0].source) : null,
          sourceError: r.source_error ?? null,
        },
        live: true,
      };
    } catch (e) {
      return { data: { gpus: [], stale: true, atMs: null, hasPolled: false, source: null, sourceError: null }, live: true, error: errOf(e) };
    }
  }
  return { data: { gpus: mockGpus(), stale: false, atMs: null, hasPolled: true, source: null, sourceError: null }, live: false };
}

/** Placement rows derived from the app config: one row per configured
 *  backend — placements belong to backends, not models (Pete,
 *  2026-10-16). Segments carry the backend's tensor-split fractions
 *  UNROUNDED (`frac`), labeled with the telemetry GPU names in index
 *  order and carrying their GPU index; zero-fraction entries are
 *  dropped (the GPU holds none of the model), but a sub-1% share is
 *  kept — the view renders it as a minimum-width sliver with its true
 *  value in the tooltip. A backend with no split gets a single 100%
 *  segment on the first GPU — an unsplit launch loads there — and a
 *  split with no positive entry likewise falls back to that
 *  single-GPU row, so every backend's placement is listed; such rows
 *  are flagged `defaulted` so the view can ghost them instead of
 *  presenting a default as an operator's configuration. Disabled
 *  backends are included and flagged (`backendEnabled: false`):
 *  their placement is eventual, and the dashboard grays the row out.
 *  Model = file stem of the backend's model_file. Each row also
 *  carries its owning backend's id and (when the caller supplies the
 *  live backend list) its runtime status, so views can key
 *  presentation to the backend's measured state instead of asserting
 *  one from configuration alone. */
function placementFromConfig(
  cfg: Record<string, unknown> | null,
  gpus: GpuInfo[],
  backends?: BackendInfo[],
): DashboardModel["placement"] {
  const backs = Array.isArray(cfg?.backends) ? cfg.backends : [];
  const rows: DashboardModel["placement"] = [];
  for (const b of backs) {
    if (!b || typeof b !== "object" || Array.isArray(b)) continue;
    const rec = b as Record<string, unknown>;
    const flags = rec.flags && typeof rec.flags === "object" && !Array.isArray(rec.flags)
      ? (rec.flags as Record<string, unknown>) : null;
    const split = Array.isArray(flags?.tensor_split) ? flags.tensor_split : null;
    let segments = (split ?? [])
      .map((f, i) => {
        const frac = typeof f === "number" && Number.isFinite(f) ? f : 0;
        return {
          gpu: gpus[i]?.name ?? `GPU ${i}`,
          gpuIndex: i,
          frac,
          pct: Math.round(frac * 100),
        };
      })
      .filter((s) => s.frac > 0);
    let defaulted = false;
    if (!segments.length) {
      segments = [{ gpu: gpus[0]?.name ?? "GPU 0", gpuIndex: 0, frac: 1, pct: 100 }];
      defaulted = true;
    }
    const modelFile = typeof rec.model_file === "string" ? rec.model_file : "";
    const base = modelFile.split(/[\\/]/).pop() || modelFile || (typeof rec.id === "string" ? rec.id : "");
    const model = base.replace(/\.gguf$/i, "");
    const backendId = typeof rec.id === "string" ? rec.id : undefined;
    rows.push({
      model,
      segments,
      backendId,
      backendStatus: backendId ? backends?.find((x) => x.id === backendId)?.status : undefined,
      backendEnabled: rec.enabled !== false,
      defaulted,
    });
  }
  return rows;
}

export interface SystemVerdict {
  /** Dot class vocabulary shared by the dashboard pill and the map pill. */
  dot: "ok" | "warn" | "err" | "idle";
  text: string;
  ok: boolean;
}

/** The one system-verdict rule, shared by the Dashboard header pill and
 *  the Service Map pill: "All systems normal" iff at least one backend
 *  is configured, every backend is running, telemetry is not stale, no
 *  load error occurred, and the gateway probe reports up. Every other
 *  state gets its own wording — all-stopped is idle, not "normal" and
 *  not an error; a partial count reads "N / M backends running". */
export function systemVerdict(input: {
  total: number;
  running: number;
  stale: boolean;
  error: string | null;
  gatewayUp: boolean;
}): SystemVerdict {
  const { total, running, stale, error, gatewayUp } = input;
  if (error) return { dot: "err", text: "Backend error", ok: false };
  if (total === 0) return { dot: "idle", text: "No backends configured", ok: false };
  if (running === total) {
    if (stale) return { dot: "warn", text: "Telemetry stale", ok: false };
    if (!gatewayUp) return { dot: "err", text: "Gateway down", ok: false };
    return { dot: "ok", text: "All systems normal", ok: true };
  }
  // A down gateway is part of the picture in the partial and
  // all-stopped states too — the pill must not read as if backends
  // were the only problem. (The all-running + gateway-down state has
  // its own "Gateway down" verdict above.)
  const gwSuffix = gatewayUp ? "" : " \u00b7 gateway down";
  if (running === 0) return { dot: "idle", text: `All backends stopped${gwSuffix}`, ok: false };
  return { dot: "warn", text: `${running} / ${total} backends running${gwSuffix}`, ok: false };
}

/** Assemble the dashboard model from the backend commands. Live
 *  placement is derived from the configured backends' tensor splits
 *  (see placementFromConfig) — empty when no split is configured, and
 *  the view renders an explicit empty state for that card. The gateway
 *  card comes from the gateway_status probe (up state + port), never
 *  from a list_backends row: no backend row represents the gateway. */
export async function getDashboard(): Promise<CallResult<DashboardModel>> {
  const [s, b, g, gw] = await Promise.all([getStatus(), listBackends(), getGpuTelemetry(), getGatewayStatus()]);
  const live = s.live && b.live && g.live;
  if (!live) {
    return { data: { ...mockDashboard(), stale: false, updatedAt: Date.now() }, live: false };
  }
  // Error separation: a gateway-probe failure does not fail the poll.
  // The gateway card is absent (its state is unknown, not down) and
  // the failure rides on the model as gatewayError for its own banner;
  // the poll-level error is set only by status/backends/telemetry
  // failures, so fresh backend data is not frozen behind a probe error.
  const errors = [s.error, b.error, g.error].filter((e): e is string => Boolean(e));
  // Live path: map whatever the backend returned. No fabrication — when
  // the probe errors, the gateway card is absent instead of a card
  // asserting a state we did not measure.
  // Gateway totals are the sum of the per-backend windows (external
  // deployments such as the cloud fallback are not in backend rows
  // and are not counted); absent when no backend has a window yet.
  const statRows = b.data.filter((x) => x.requests_1h !== undefined);
  const sumStat = (pick: (x: BackendInfo) => number | undefined): number | undefined =>
    statRows.length ? statRows.reduce((a, x) => a + (pick(x) ?? 0), 0) : undefined;
  // Gateway sparkline: requests/min summed across the backends
  // that carry a series (the card captions it "Requests per
  // minute"); absent when no backend has one yet.
  const sparkRows = b.data.filter((x) => x.reqSpark !== undefined);
  const gatewaySpark: number[] | undefined = sparkRows.length
    ? sparkRows[0].reqSpark!.map((_, i) =>
        sparkRows.reduce((a, x) => a + (x.reqSpark![i] ?? 0), 0))
    : undefined;
  const gateway: BackendInfo | null = gw.error
    ? null
    : {
        id: "gateway",
        name: "LiteLLM gateway",
        port: gw.data.port,
        status: gw.data.up ? "ok" : "down",
        model: "",
        requests_1h: sumStat((x) => x.requests_1h),
        errors_1h: sumStat((x) => x.errors_1h),
        throughput_toks: sumStat((x) => x.throughput_toks),
        spark: gatewaySpark,
      };
  // VRAM held per backend, measured from telemetry's per-process
  // attribution (the dashboard card's "VRAM HELD" figure previously
  // had no live source and always rendered absent). Unmeasured rows
  // (vram_mib null) are skipped, and a backend with no measured row
  // gets no figure at all — absent, not zero.
  const heldMib = new Map<string, number>();
  for (const gpu of g.data.gpus) {
    for (const p of gpu.processes ?? []) {
      if (p.backend !== "—" && p.vram_mib !== null) {
        heldMib.set(p.backend, (heldMib.get(p.backend) ?? 0) + p.vram_mib);
      }
    }
  }
  const backends = b.data.map((x) => {
    const mib = heldMib.get(x.id);
    return mib !== undefined ? { ...x, vram_held_gb: mib / 1024 } : x;
  });
  const cfg = await getAppConfig();
  const gwCfg = await getGatewayConfig();
  const data: DashboardModel = {
    gateway, backends, gpus: g.data.gpus, placement: placementFromConfig(cfg.data, g.data.gpus, backends),
    gatewayUp: !gw.error && gw.data.up,
    gatewayStrategy: gwCfg.data ? effectiveGatewayStrategy(gwCfg.data) : null,
    gatewayError: gw.error,
    stale: g.data.stale, updatedAt: Date.now(),
  };
  return errors.length ? { data, live: true, error: errors.join("; ") } : { data, live: true };
}

export interface WizardStepOutcome {
  state: string;
  message: string;
  log_tail: string[];
  duration_ms: number;
  /** Ready-to-run manual commands attached to a failure (the
   * start-gateway step sets this): rendered as a code block under
   * the failure message. */
  manual_command?: string | null;
}

/** The run_wizard_step result. `busy` is true when another step
 * already holds the wizard lock: a running state to report, not a
 * failure, with `outcome` null. When the backend knows which step
 * is in flight, `running_step`/`elapsed_secs` say what and for how
 * long. */
export interface WizardStepRun {
  task_id: string;
  busy: boolean;
  running_step?: string;
  elapsed_secs?: number;
  outcome: WizardStepOutcome | null;
}

/** Human elapsed for a busy report: "45s", "12 min", "1 h 5 min". */
function fmtElapsedSecs(secs: number): string {
  if (secs < 60) return `${secs}s`;
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins} min`;
  const rem = mins % 60;
  return rem ? `${Math.floor(mins / 60)} h ${rem} min` : `${Math.floor(mins / 60)} h`;
}

/** The message for a busy run_wizard_step result: names the
 * in-flight step and its elapsed time when the backend reported
 * them, else the caller's fallback text. */
export function wizardBusyMessage(data: { running_step?: string; elapsed_secs?: number }, fallback: string): string {
  if (data.running_step) {
    const elapsed = typeof data.elapsed_secs === "number" ? ` for ${fmtElapsedSecs(data.elapsed_secs)}` : "";
    return `${data.running_step} has been running${elapsed} — a setup step is still running; wait for it to finish, then try again.`;
  }
  return fallback;
}

/** run_wizard_step — runs one step to completion; the outcome is in the
 * result. `busy` is true when another step already holds the wizard
 * lock: that is a running state to report, not a failure, and
 * `outcome` is null. */
export async function runWizardStep(stepId: string): Promise<CallResult<WizardStepRun>> {
  if (isTauri()) {
    try {
      const data = await invoke<{ task_id: string; busy?: boolean; running_step?: string; elapsed_secs?: number; outcome: WizardStepOutcome | null }>("run_wizard_step", { stepId });
      return { data: { task_id: data.task_id, busy: Boolean(data.busy), running_step: data.running_step, elapsed_secs: data.elapsed_secs, outcome: data.outcome ?? null }, live: true };
    } catch (e) {
      return { data: { task_id: "", busy: false, outcome: { state: "failed", message: String(e), log_tail: [], duration_ms: 0 } }, live: true, error: errOf(e) };
    }
  }
  return { data: { task_id: "sample-task", busy: false, outcome: { state: "done", message: "sample mode", log_tail: [], duration_ms: 0 } }, live: false };
}

/** run_audit — re-run all check-type steps in audit mode. The response
 * carries the audit's per-step results: the checklist is otherwise
 * painted from the install wizard's state, which an audit never
 * changes, so the caller paints these steps to make the outcome
 * visible. */
export interface AuditResult {
  task_id: string;
  done: number;
  total: number;
  steps: WizardStepState[];
  /** True when an audit was already in flight (e.g. the launch-time
   * hydration audit): a running state to report, not a failure. The
   * running audit's results land in the checklist when it finishes. */
  already_running: boolean;
}

export async function runAudit(): Promise<CallResult<AuditResult>> {
  if (isTauri()) {
    try {
      const r = await invoke<{ task_id: string; done: number; total: number; steps: BackendStep[]; already_running?: boolean }>("run_audit");
      return { data: { task_id: r.task_id, done: r.done, total: r.total, steps: r.steps.map(mapBackendStep), already_running: Boolean(r.already_running) }, live: true };
    } catch (e) {
      return { data: { task_id: "", done: 0, total: 0, steps: [], already_running: false }, live: true, error: errOf(e) };
    }
  }
  return { data: { task_id: "sample-task", done: 0, total: 0, steps: [], already_running: false }, live: false };
}

/** One command a wizard step will run (setup_get_commands wire shape). */
export interface PlannedCommandWire {
  argv: string[];
  cwd: string | null;
  env: Record<string, string>;
  stdout_log: string | null;
  stderr_log: string | null;
}

/** setup_get_commands — the exact commands a step runs, for the
 * failed-step "run it manually" block in the wizard. */
export async function getWizardCommands(stepId: string): Promise<CallResult<PlannedCommandWire[]>> {
  if (!isTauri()) return { data: [], live: false };
  try {
    const data = await invoke<PlannedCommandWire[]>("setup_get_commands", { stepId });
    return { data: Array.isArray(data) ? data : [], live: true };
  } catch (e) {
    return { data: [], live: true, error: errOf(e) };
  }
}

interface BackendStep {
  id: string;
  name: string;
  state: WizardStepState["state"];
  message: string;
  duration_ms: number;
  log_tail: string[];
  note: string | null;
  manual_command: string | null;
  remediation: string | null;
  help_url: string | null;
}

/** setup_get_steps — live wizard step states under Tauri; sample data in the browser. */
/** Map one backend step record (setup_get_steps / run_audit shape)
 * into the UI's step state. */
function mapBackendStep(s: BackendStep): WizardStepState {
  return {
    id: s.id,
    name: s.name,
    detail: s.message,
    state: s.state,
    duration: s.duration_ms > 0 ? `${trim1(s.duration_ms / 1000)}s` : undefined,
    note: s.note ?? undefined,
    helpUrl: s.help_url ?? undefined,
    message: s.message,
    logTail: s.log_tail,
    manualCommand: s.manual_command ?? undefined,
    remediation: s.remediation ?? undefined,
  };
}

export async function getWizardSteps(): Promise<CallResult<WizardStepState[]>> {
  if (!isTauri()) return { data: mockWizardSteps(), live: false };
  try {
    const r = await invoke<{ steps: BackendStep[]; done: number; total: number }>("setup_get_steps");
    return { data: r.steps.map(mapBackendStep), live: true };
  } catch (e) {
    return { data: [], live: true, error: errOf(e) };
  }
}

/** setup_status — whether the setup wizard has been completed. */
export async function getSetupStatus(): Promise<CallResult<{ setup_completed: boolean }>> {
  if (!isTauri()) return { data: { setup_completed: true }, live: false };
  try {
    const data = await invoke<{ setup_completed: boolean }>("setup_status");
    return { data, live: true };
  } catch (e) { return { data: { setup_completed: true }, live: false, error: String(e) }; }
}

/** setup_mark_complete — mark the setup wizard complete (persisted).
 *  The command now rejects on save failure (a proper Err, like every
 *  other command); the rejection is normalized here into
 *  { ok: false, persisted: false } + error so callers keep reading the
 *  outcome from the result instead of an unhandled rejection. */
export async function markSetupComplete(): Promise<CallResult<{ ok: boolean; persisted: boolean }>> {
  if (!isTauri()) return { data: { ok: true, persisted: false }, live: false };
  try {
    const data = await invoke<{ ok: boolean; persisted: boolean }>("setup_mark_complete");
    return { data, live: true };
  } catch (e) { return { data: { ok: false, persisted: false }, live: true, error: errOf(e) }; }
}

/** setup_step_help_url — help URL a wizard step offers (e.g. driver download page). */
export async function getStepHelpUrl(stepId: string): Promise<CallResult<{ url: string | null; step_id: string }>> {
  if (!isTauri()) return { data: { url: null, step_id: stepId }, live: false };
  try {
    const data = await invoke<{ url: string | null; step_id: string }>("setup_step_help_url", { stepId });
    return { data, live: true };
  } catch (e) { return { data: { url: null, step_id: stepId }, live: false, error: String(e) }; }
}

/** open_url — open an external URL in the system browser (http/https only). */
export async function openUrl(url: string): Promise<CallResult<{ ok: boolean }>> {
  if (!isTauri()) { window.open(url, "_blank", "noopener"); return { data: { ok: true }, live: false }; }
  try {
    const data = await invoke<{ ok: boolean }>("open_url", { url });
    return { data, live: true };
  } catch (e) { return { data: { ok: false }, live: false, error: String(e) }; }
}

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
  /** Supervisor process state: stopped | starting | running | stopping | failed. */
  state: string;
  /** True when the row's state was merged from the SCM (service mode). */
  service_managed?: boolean;
  pid: number | null;
  /** True when this session adopted the process (started by a previous
   *  launch; the app tracks it by pid rather than as its own child). */
  adopted: boolean;
  /** Supervisor crash-backoff attempt count for this backend. It counts
   *  crash-restart backoff attempts, not starts: nothing consumes it yet,
   *  so it reads 0 until a restart monitor exists (v0.1.2 has none). */
  restarts: number;
  uptime_secs: number | null;
  /** Trailing-hour gateway statistics from LiteLLM's Prometheus
   *  counters (present only once the metrics poller has a window;
   *  absent renders as "—", not zero). */
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
  /** Gateway group this backend belongs to, or null when ungrouped. */
  group?: string | null;
  /** Memory requirement under the backend's current configuration
   *  (configured rows whose model resolves): GPU-resident weights,
   *  system-RAM weights (expert offload), and the KV cache at the
   *  configured context when the model's geometry is known. */
  gpu_ram_mib?: number;
  sys_ram_mib?: number;
  kv_ram_mib?: number | null;
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
  /** Parallel slots state (configured rows — the flags form's
   *  Parallel slots field renders for every backend, so this is
   *  not capability-gated); absent on supervisor-only orphan
   *  rows. Decoded from the backend's saved raw flags. */
  parallel?: BackendParallel;
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

async function backendAction(
  cmd: "start_backend" | "stop_backend" | "restart_backend",
  id: string,
): Promise<CallResult<{ task_id: string; backend_id: string }>> {
  if (!isTauri()) return { data: { task_id: "sample-task", backend_id: id }, live: false };
  try {
    const args: Record<string, unknown> = cmd === "start_backend" ? { id } : { id, graceful: true };
    const data = await invoke<{ task_id: string; backend_id: string }>(cmd, args);
    return { data, live: true };
  } catch (e) {
    return { data: { task_id: "", backend_id: id }, live: true, error: errOf(e) };
  }
}

export function startBackend(id: string) { return backendAction("start_backend", id); }
export function stopBackend(id: string) { return backendAction("stop_backend", id); }
export function restartBackend(id: string) { return backendAction("restart_backend", id); }

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
 *  `allowOverride` is true it is sent as `allow_override`, recording
 *  an explicit override for a guardrail-blocked position; the
 *  argument is omitted entirely when falsy, preserving the previous
 *  invoke shape for other callers. */
export async function setBackendOffload(id: string, layers: number, allowOverride?: boolean): Promise<CallResult<{ backend_id: string; layers: number; raw_flags: string | null; changed: boolean }>> {
  if (!isTauri()) return { data: { backend_id: id, layers, raw_flags: null, changed: false }, live: false };
  try {
    const args: Record<string, unknown> = { id, layers };
    if (allowOverride) args.allow_override = true;
    const data = await invoke<{ backend_id: string; layers: number; raw_flags: string | null; changed: boolean }>("backend_set_offload", args);
    return { data, live: true };
  } catch (e) {
    return { data: { backend_id: id, layers, raw_flags: null, changed: false }, live: true, error: errOf(e) };
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
  offload: BackendOffload | null;
  tensor_split: number[];
  split_mode: string;
}

/** backend_add_preview — the add form's model-dependent parameter
 *  state: the offload control's range/bands/guardrail for the model
 *  and the placement a creation would assign. gpuIndices previews
 *  an explicit GPU selection instead of the telemetry assignment. */
export async function backendAddPreview(modelId: string, gpuIndices?: number[]): Promise<CallResult<BackendAddPreview>> {
  if (!isTauri()) return { data: { offload: null, tensor_split: [], split_mode: "none" }, live: false };
  try {
    const args: Record<string, unknown> = { modelId };
    if (gpuIndices !== undefined) args.gpuIndices = gpuIndices;
    const data = await invoke<BackendAddPreview>("backend_add_preview", args);
    return { data, live: true };
  } catch (e) {
    return { data: { offload: null, tensor_split: [], split_mode: "none" }, live: true, error: errOf(e) };
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

/* ---------------- Models ---------------- */

export interface ModelRow {
  id: string;
  /** Short display name for compact controls (the curated catalog
   *  name when the model has one; the id otherwise). */
  name: string;
  gguf_path: string;
  quant: string;
  params_b: number;
  sha256: string;
  verified_at: string | null;
  /** Whether the setup wizard's download step includes this model. */
  enabled: boolean;
  on_disk: boolean;
  size_bytes: number | null;
  /** True while a download for this model is in flight (backend-tracked,
   *  so it is accurate on first paint, not just after a local click). */
  downloading: boolean;
  /** True after a download's transfer has finished while the backend
   *  watcher verifies the file against the pinned SHA-256. The file is
   *  not a plain complete state until this clears. */
  verifying: boolean;
  /** True while an in-flight download is paused (worker parked, partial
   *  file kept for resume). */
  paused: boolean;
  /** Size of the partial (.part) file when an incomplete download exists,
   *  else null. The final path only ever holds complete files, so a
   *  nonzero partial_bytes with on_disk=false means "partial download". */
  partial_bytes: number | null;
  /** Last terminal download failure for this model in this session
   *  (transport error, or a hash mismatch — which also removes the
   *  downloaded file), else null. */
  download_error: string | null;
  /** Curated catalog size for this model when known. An ESTIMATE, used
   *  only for display ("~X GB (estimate)"), never for verification. */
  catalog_size_bytes: number | null;
  /** The model's rated maximum context length in tokens, from the
   *  curated catalog (null for models outside it). The backend flags
   *  editor annotates context presets beyond this value. */
  ctx_max?: number | null;
  fit_check_available: boolean;
}

/** list_models — configured models with on-disk presence and size. */
export async function getModels(): Promise<CallResult<ModelRow[]>> {
  if (!isTauri()) return { data: [], live: false };
  try {
    const r = await invoke<{ models: (ModelRow & { verifying?: boolean })[] }>("list_models");
    const rows = Array.isArray(r.models) ? r.models : [];
    return { data: rows.map((m) => ({ ...m, verifying: Boolean(m.verifying) })), live: true };
  } catch (e) {
    return { data: [], live: true, error: errOf(e) };
  }
}

/** download_model — spawn a model download and return immediately with
 *  the destination path; the transfer runs in a background worker that
 *  resumes from any partial (.part) file. When the transfer completes, a
 *  backend watcher verifies the file against the pinned SHA-256 (when a
 *  real pin exists) and records verified_at; a mismatch removes the file
 *  and surfaces as the row's download_error. */
export async function downloadModel(id: string): Promise<CallResult<{ model_id: string; dest: string }>> {
  if (!isTauri()) return { data: { model_id: id, dest: "" }, live: false };
  try {
    const data = await invoke<{ model_id: string; dest: string }>("download_model", { id });
    return { data, live: true };
  } catch (e) {
    return { data: { model_id: id, dest: "" }, live: true, error: errOf(e) };
  }
}

/** cancel_download — abort an in-flight download (the partial file is
 *  discarded). Errors with "no download in flight" when the download has
 *  already finished. */
export async function cancelDownload(id: string): Promise<CallResult<{ model_id: string; cancelled: boolean }>> {
  if (!isTauri()) return { data: { model_id: id, cancelled: true }, live: false };
  try {
    const data = await invoke<{ model_id: string; cancelled: boolean }>("cancel_download", { id });
    return { data, live: true };
  } catch (e) {
    return { data: { model_id: id, cancelled: false }, live: true, error: errOf(e) };
  }
}

/** model_pause_download — pause an in-flight download; the worker parks
 *  and the partial (.part) file is kept for resume. */
export async function pauseModelDownload(id: string): Promise<CallResult<{ model_id: string; paused: boolean }>> {
  if (!isTauri()) return { data: { model_id: id, paused: false }, live: false, error: "model_pause_download is not available in sample mode" };
  try {
    const data = await invoke<{ model_id: string; paused: boolean }>("model_pause_download", { id });
    return { data, live: true };
  } catch (e) {
    return { data: { model_id: id, paused: false }, live: true, error: errOf(e) };
  }
}

/** model_resume_download — resume a paused download from its partial file. */
export async function resumeModelDownload(id: string): Promise<CallResult<{ model_id: string; resumed: boolean }>> {
  if (!isTauri()) return { data: { model_id: id, resumed: false }, live: false, error: "model_resume_download is not available in sample mode" };
  try {
    const data = await invoke<{ model_id: string; resumed: boolean }>("model_resume_download", { id });
    return { data, live: true };
  } catch (e) {
    return { data: { model_id: id, resumed: false }, live: true, error: errOf(e) };
  }
}

/** model_verify_checksum — SHA-256 the file on disk against the pinned hash. */
export async function verifyModel(id: string): Promise<CallResult<{ id: string; ok: boolean }>> {
  if (!isTauri()) return { data: { id, ok: false }, live: false };
  try {
    const data = await invoke<{ id: string; ok: boolean }>("model_verify_checksum", { id });
    return { data, live: true };
  } catch (e) {
    return { data: { id, ok: false }, live: true, error: errOf(e) };
  }
}

/** model_set_short_name — set the model's operator-chosen short
 *  display name (persisted override; wins over the catalog name in
 *  every short-name display). The model id never changes. */
export async function modelSetShortName(id: string, name: string): Promise<CallResult<{ id: string; name: string }>> {
  if (!isTauri()) return { data: { id, name }, live: false };
  try {
    const data = await invoke<{ id: string; name: string }>("model_set_short_name", { id, name });
    return { data, live: true };
  } catch (e) {
    return { data: { id, name }, live: true, error: errOf(e) };
  }
}

/** model_set_enabled — include/exclude a model from wizard downloads.
 *  The model's file and configuration are kept either way. */
export async function modelSetEnabled(id: string, enabled: boolean): Promise<CallResult<{ model_id: string; enabled: boolean }>> {
  if (!isTauri()) return { data: { model_id: id, enabled }, live: false };
  try {
    const data = await invoke<{ model_id: string; enabled: boolean }>("model_set_enabled", { id, enabled });
    return { data, live: true };
  } catch (e) {
    return { data: { model_id: id, enabled: !enabled }, live: true, error: errOf(e) };
  }
}

/** model_add — register a new model by id and GGUF source URL. File
 *  name, quant, and paramsB are optional (the file name defaults to
 *  the URL's last path segment). paramsB is the total parameter count
 *  in billions: the command resolves it from the curated catalog
 *  when omitted and refuses the add when no source yields a count.
 *  The model starts enabled with no pinned hash. */
export async function modelAdd(id: string, sourceUrl: string, fileName?: string, quant?: string, paramsB?: number): Promise<CallResult<{ model_id: string }>> {
  if (!isTauri()) return { data: { model_id: id }, live: false };
  try {
    const data = await invoke<{ model_id: string }>("model_add", {
      id,
      sourceUrl,
      fileName: fileName ?? null,
      quant: quant ?? null,
      paramsB: paramsB ?? null,
    });
    return { data, live: true };
  } catch (e) {
    return { data: { model_id: id }, live: true, error: errOf(e) };
  }
}

/** model_delete — remove a model's file (and any partial .part file) and
 *  its config entry; the removal is persisted. Refused while a download
 *  is in flight for the model or while any backend references it. */
export async function deleteModel(id: string): Promise<CallResult<{ model_id: string; deleted: boolean; bytes_freed: number }>> {
  if (!isTauri()) return { data: { model_id: id, deleted: true, bytes_freed: 0 }, live: false };
  try {
    const data = await invoke<{ model_id: string; deleted: boolean; bytes_freed: number }>("model_delete", { id });
    return { data, live: true };
  } catch (e) {
    return { data: { model_id: id, deleted: false, bytes_freed: 0 }, live: true, error: errOf(e) };
  }
}

/* ---------------- Logs ---------------- */

export interface LogRow {
  ts: string;
  stream: string;
  line: string;
  /** Process id this line was captured from (its scope). */
  source: string;
  /** True when the line comes from a previous run's retained ring. */
  archived: boolean;
}

/** One entry of the payload's scope list: a queryable log scope.
 *  `previous` marks a scope that also has a previous run's retained
 *  ring, labeled "<id> (incl. previous run)" in the picker. */
export interface LogScopeInfo {
  id: string;
  previous: boolean;
}

export interface LogPage {
  lines: LogRow[];
  /** Lines matching scope + filters before paging was applied. */
  total_matching: number;
  /** Alias of total_matching, returned by current backends. */
  total: number;
  /** True when paging cut the result on either side of the page. */
  truncated: boolean;
  scopes: LogScopeInfo[];
}

export interface GetLogsOptions {
  limit?: number;
  /** Heuristic level filter: error | warn | info | debug. */
  level?: string;
  /** Case-insensitive substring filter over line text. */
  search?: string;
  /** Case-insensitive substring filter over line text and source. */
  query?: string;
  /** Epoch-ms lower bound (inclusive) on record timestamps. */
  sinceMs?: number;
  /** Epoch-ms upper bound (inclusive) on record timestamps. */
  untilMs?: number;
  /** Timestamp sort direction; default "desc" (newest first). */
  order?: "asc" | "desc";
  /** Filtered, ordered records to skip before the page (paging). */
  offset?: number;
}

/** get_logs — captured supervisor logs. `scope` is a process id or
 *  "all"; archived (previous-run) scope ids from the payload's scopes
 *  list are accepted as scopes too. `level` is an optional error/warn/
 *  info/debug heuristic filter; `search` and `query` are optional
 *  case-insensitive substrings (line text only / line text + source);
 *  `sinceMs`/`untilMs` bound record timestamps; results sort by
 *  timestamp per `order` (default "desc", newest first) and page via
 *  `offset` + `limit`. The payload is { lines, total_matching, total,
 *  truncated, scopes }; a bare-array reply (older backend) is
 *  normalized into the same shape. */
export async function getLogs(scope: string, opts: GetLogsOptions = {}): Promise<CallResult<LogPage>> {
  const empty: LogPage = { lines: [], total_matching: 0, total: 0, truncated: false, scopes: [] };
  if (!isTauri()) return { data: empty, live: false };
  try {
    const args: Record<string, unknown> = { scope, limit: opts.limit ?? 200 };
    if (opts.level) args.level = opts.level;
    if (opts.search) args.search = opts.search;
    if (opts.query) args.query = opts.query;
    if (opts.sinceMs !== undefined) args.sinceMs = opts.sinceMs;
    if (opts.untilMs !== undefined) args.untilMs = opts.untilMs;
    if (opts.order) args.order = opts.order;
    if (opts.offset !== undefined) args.offset = opts.offset;
    const raw = await invoke<unknown>("get_logs", args);
    if (Array.isArray(raw)) {
      // Pre-envelope backend: rows carry no source/archived markers.
      const lines = (raw as { ts: string; stream: string; line: string }[]).map((r) => ({
        ts: r.ts, stream: r.stream, line: r.line,
        source: scope === "all" ? "" : scope, archived: false,
      }));
      return { data: { lines, total_matching: lines.length, total: lines.length, truncated: false, scopes: [] }, live: true };
    }
    const page = (raw ?? {}) as Partial<LogPage>;
    const lines = (Array.isArray(page.lines) ? page.lines : []).map((r) => ({
      ts: r.ts, stream: r.stream, line: r.line,
      source: typeof r.source === "string" ? r.source : "",
      archived: Boolean(r.archived),
    }));
    const totalMatching = typeof page.total_matching === "number" ? page.total_matching : lines.length;
    return {
      data: {
        lines,
        total_matching: totalMatching,
        total: typeof page.total === "number" ? page.total : totalMatching,
        truncated: Boolean(page.truncated),
        scopes: Array.isArray(page.scopes) ? page.scopes : [],
      },
      live: true,
    };
  } catch (e) {
    return { data: empty, live: true, error: errOf(e) };
  }
}

/** diagnostics_export_bundle — write a diagnostics zip (config snapshot,
 *  per-scope recent logs, telemetry snapshot, versions) under the data
 *  dir; returns the bundle path. The logs inside are raw and unredacted —
 *  the view warns before sharing. */
export async function exportDiagnostics(): Promise<CallResult<{ path: string }>> {
  if (!isTauri()) return { data: { path: "" }, live: false, error: "diagnostics_export_bundle is not available in sample mode" };
  try {
    const data = await invoke<{ path: string }>("diagnostics_export_bundle");
    return { data, live: true };
  } catch (e) {
    return { data: { path: "" }, live: true, error: errOf(e) };
  }
}

/** open_folder — open a folder (or a file's containing folder) in
 *  Windows Explorer. Backs the "Open folder" button shown after a
 *  diagnostics export. */
export async function openFolder(path: string): Promise<CallResult<{ ok: boolean }>> {
  if (!isTauri()) return { data: { ok: false }, live: false, error: "open_folder is not available in sample mode" };
  try {
    const data = await invoke<{ ok: boolean }>("open_folder", { path });
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false }, live: true, error: errOf(e) };
  }
}

/* ---------------- Gateway ---------------- */

export interface GatewayGroup {
  name: string;
  members: string[];
  strategy: string;
  fallbacks: string[];
  /** Derived by the backend (manager-gateway), never stored: the
   *  group is enabled iff at least one member backend is enabled,
   *  and only enabled groups reach the generated gateway config. */
  enabled: boolean;
  /** How many member backends are enabled — the basis of `enabled`. */
  enabled_members: number;
}

/** A typed routing rule's behavior (manager-config's
 *  RoutingRuleKind, routing-rules design §6.2). Only the
 *  context-overflow kind exists in v1. Absent/null on legacy
 *  fragment-only rules, which are inert and render read-only. */
export interface GatewayRuleKind {
  type: "context-overflow";
  target_group: string;
  /** Advisory threshold as a fraction of the serving group's
   *  hard window; ignored when optimal_tokens is set. */
  optimal_fraction: number;
  /** Absolute advisory threshold in tokens, or null for the
   *  fraction default (half the serving group's window). */
  optimal_tokens: number | null;
  /** Output tokens assumed when a request sets no max_tokens. */
  output_reserve_tokens: number;
  advisory: boolean;
}

export interface GatewayRule {
  id: string;
  description: string;
  source: string;
  /** Serde default true on payloads that predate the field. */
  enabled: boolean;
  /** The typed behavior, or null for a legacy fragment-only rule. */
  kind: GatewayRuleKind | null;
  /** The LiteLLM config fragment this rule injects, carried by the
   *  get_gateway_config payload; shown in the rule's Fragment
   *  expandable. Absent on payloads that predate the field. */
  litellm_fragment?: string;
  /** The fragment's untouched raw value from the payload, carried
   *  so a save through update_gateway_config writes back exactly
   *  what was stored (the display form above stringifies objects).
   *  Undefined when the payload carried no fragment. */
  fragment_raw?: unknown;
}

export interface GatewayGroup {
  name: string;
  members: string[];
  strategy: string;
  fallbacks: string[];
  /** Derived by the backend (manager-gateway), never stored: the
   *  group is enabled iff at least one member backend is enabled,
   *  and only enabled groups reach the generated gateway config. */
  enabled: boolean;
  /** How many member backends are enabled — the basis of `enabled`. */
  enabled_members: number;
}

/** The OpenRouter cloud fallback tier (manager-config's
 *  CloudTierConfig). The key itself is never in the config — only the
 *  Credential Manager reference name. */
export interface GatewayOpenRouter {
  enabled: boolean;
  cred_ref: string | null;
  daily_cap_usd: number;
  model_allowlist: string[];
}

export interface GatewayConfigData {
  port: number;
  groups: GatewayGroup[];
  routing_rules: GatewayRule[];
  request_log_len: number;
  openrouter: GatewayOpenRouter | null;
}

/** get_gateway_config — LiteLLM gateway configuration from AppConfig.
 *  The command serializes the whole GatewayConfig; the openrouter tier
 *  is mapped out explicitly so the view can render it (it was silently
 *  dropped by the earlier typing). */
export async function getGatewayConfig(): Promise<CallResult<GatewayConfigData | null>> {
  if (!isTauri()) return { data: null, live: false };
  try {
    const raw = await invoke<Record<string, unknown>>("get_gateway_config");
    const or = raw.openrouter as Partial<GatewayOpenRouter> | null | undefined;
    const data: GatewayConfigData = {
      port: typeof raw.port === "number" ? raw.port : 0,
      groups: Array.isArray(raw.groups) ? (raw.groups as Array<Partial<GatewayGroup>>).map((g) => ({
        name: typeof g.name === "string" ? g.name : "",
        members: Array.isArray(g.members) ? g.members : [],
        strategy: typeof g.strategy === "string" ? g.strategy : "",
        fallbacks: Array.isArray(g.fallbacks) ? g.fallbacks : [],
        enabled: typeof g.enabled === "boolean" ? g.enabled : true,
        enabled_members: typeof g.enabled_members === "number"
          ? g.enabled_members
          : (Array.isArray(g.members) ? g.members.length : 0),
      })) : [],
      routing_rules: Array.isArray(raw.routing_rules) ? (raw.routing_rules as Array<Partial<GatewayRule> & Record<string, unknown>>).map((r) => ({
        id: typeof r.id === "string" ? r.id : "",
        description: typeof r.description === "string" ? r.description : "",
        source: typeof r.source === "string" ? r.source : "",
        enabled: typeof r.enabled === "boolean" ? r.enabled : true,
        kind: r.kind && typeof r.kind === "object" && (r.kind as GatewayRuleKind).type === "context-overflow"
          ? {
              type: "context-overflow" as const,
              target_group: typeof (r.kind as GatewayRuleKind).target_group === "string" ? (r.kind as GatewayRuleKind).target_group : "",
              optimal_fraction: typeof (r.kind as GatewayRuleKind).optimal_fraction === "number" ? (r.kind as GatewayRuleKind).optimal_fraction : 0.5,
              optimal_tokens: typeof (r.kind as GatewayRuleKind).optimal_tokens === "number" ? (r.kind as GatewayRuleKind).optimal_tokens : null,
              output_reserve_tokens: typeof (r.kind as GatewayRuleKind).output_reserve_tokens === "number" ? (r.kind as GatewayRuleKind).output_reserve_tokens : 1024,
              advisory: (r.kind as GatewayRuleKind).advisory !== false,
            }
          : null,
        // The fragment is an arbitrary JSON value in the stored
        // config; objects render as their JSON text.
        litellm_fragment: typeof r.litellm_fragment === "string"
          ? r.litellm_fragment
          : r.litellm_fragment != null
            ? JSON.stringify(r.litellm_fragment)
            : undefined,
        fragment_raw: r.litellm_fragment === undefined ? undefined : r.litellm_fragment,
      })) : [],
      request_log_len: typeof raw.request_log_len === "number" ? raw.request_log_len : 0,
      openrouter: or && typeof or === "object"
        ? {
            enabled: Boolean(or.enabled),
            cred_ref: typeof or.cred_ref === "string" ? or.cred_ref : null,
            daily_cap_usd: typeof or.daily_cap_usd === "number" ? or.daily_cap_usd : 0,
            model_allowlist: Array.isArray(or.model_allowlist) ? or.model_allowlist : [],
          }
        : null,
    };
    return { data, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

/** The one global routing strategy LiteLLM will apply, resolved with
 *  the same rule the config generator uses (manager-gateway's
 *  resolve_global_strategy) over the groups the generator includes
 *  — enabled groups only: the first multi-member group's strategy,
 *  else the planner group's, else the first group's. Null when no
 *  enabled group exists. */
export function effectiveGatewayStrategy(cfg: GatewayConfigData): string | null {
  const included = cfg.groups.filter((g) => g.enabled);
  if (!included.length) return null;
  const multi = included.find((g) => g.members.length > 1);
  if (multi) return multi.strategy;
  const planner = included.find((g) => g.name === "planner");
  if (planner) return planner.strategy;
  return included[0].strategy;
}

export interface GatewayStatusData {
  port: number;
  /** Enabled groups only: a group counts when at least one of its
   *  member backends is enabled — the groups the gateway actually
   *  serves, not the configured total (tracker #11). */
  groups: number;
  routing_rules: number;
  /** Live probe result: true when the gateway answered on its port. */
  up: boolean;
  /** Routing-test traces recorded this session (dry-run decisions from
   *  gateway_test_routing). Live requests are not logged. */
  routing_log_len: number;
  /** Whether the OpenRouter credential reference resolves to a
   *  credential present in Windows Credential Manager: true = present,
   *  false = reference set but not found, null = no reference is
   *  configured (or the backend did not report presence). A presence
   *  check only — the credential value never leaves the backend. */
  openrouter_cred_present: boolean | null;
  /** The configured bind address (0.0.0.0 = all interfaces,
   *  127.0.0.1 = this computer only). */
  bind_host: string;
  /** The workstation's primary LAN address when bound beyond
   *  loopback, for the client endpoint shown to other machines. */
  lan_ip: string | null;
  /** True when saved configuration changes are waiting on a gateway
   *  restart (an activation plan is pending). */
  activation_pending: boolean;
  /** How many journaled changes the pending plan carries. */
  activation_changes: number;
  /** The LiteLLM version recorded by the last gateway spawn (MCP
   *  design §4.3); null when no spawn has recorded one — rendered
   *  as "not measured", never as a guess. */
  litellm_version: string | null;
}

/** gateway_status — probe state (up) + port + group/rule counts + the
 *  recorded routing-test trace count. */
/** gateway_master_key — the LiteLLM master key: the API key local
 *  clients present and the admin UI password (username admin). The
 *  backend reads it from Windows Credential Manager (generating and
 *  storing on first use), so the returned value is the one in force. */
export async function getGatewayMasterKey(): Promise<CallResult<{ master_key: string }>> {
  if (!isTauri()) return { data: { master_key: "" }, live: false, error: "gateway_master_key is not available in sample mode" };
  try {
    const data = await invoke<{ master_key: string }>("gateway_master_key");
    return { data, live: true };
  } catch (e) {
    return { data: { master_key: "" }, live: true, error: errOf(e) };
  }
}

export async function getGatewayStatus(): Promise<CallResult<GatewayStatusData>> {
  const fallback: GatewayStatusData = { port: 4000, groups: 0, routing_rules: 0, up: false, routing_log_len: 0, openrouter_cred_present: null, bind_host: "0.0.0.0", lan_ip: null, activation_pending: false, activation_changes: 0, litellm_version: null };
  if (!isTauri()) return { data: fallback, live: false };
  try {
    const raw = await invoke<Partial<GatewayStatusData>>("gateway_status");
    return {
      data: {
        port: typeof raw.port === "number" ? raw.port : 0,
        groups: typeof raw.groups === "number" ? raw.groups : 0,
        routing_rules: typeof raw.routing_rules === "number" ? raw.routing_rules : 0,
        up: Boolean(raw.up),
        routing_log_len: typeof raw.routing_log_len === "number" ? raw.routing_log_len : 0,
        openrouter_cred_present: typeof raw.openrouter_cred_present === "boolean" ? raw.openrouter_cred_present : null,
        bind_host: typeof raw.bind_host === "string" ? raw.bind_host : "0.0.0.0",
        lan_ip: typeof raw.lan_ip === "string" ? raw.lan_ip : null,
        activation_pending: Boolean(raw.activation_pending),
        activation_changes: typeof raw.activation_changes === "number" ? raw.activation_changes : 0,
        litellm_version: typeof raw.litellm_version === "string" ? raw.litellm_version : null,
      },
      live: true,
    };
  } catch (e) {
    return { data: { ...fallback, port: 0 }, live: true, error: errOf(e) };
  }
}

/** gateway_set_bind_host — set the gateway bind address. Applies at
 *  the next gateway start (restart_required is always true); the
 *  start-gateway step reconciles the Windows firewall rule then. */
export async function setGatewayBindHost(host: string): Promise<CallResult<{ bind_host: string; restart_required: boolean }>> {
  if (!isTauri()) return { data: { bind_host: host, restart_required: true }, live: false };
  try {
    const data = await invoke<{ bind_host: string; restart_required: boolean }>("gateway_set_bind_host", { host });
    return { data, live: true };
  } catch (e) {
    return { data: { bind_host: host, restart_required: true }, live: true, error: errOf(e) };
  }
}

/** gateway_stop — stop the running gateway process (the counterpart
 *  of the wizard's start-gateway step). `stopped` reports whether a
 *  gateway process was actually terminated; false with ok=true means
 *  it was not running. */
export async function gatewayStop(): Promise<CallResult<{ ok: boolean; stopped: boolean }>> {
  if (!isTauri()) return { data: { ok: false, stopped: false }, live: false, error: "gateway_stop is not available in sample mode" };
  try {
    const data = await invoke<{ ok: boolean; stopped: boolean }>("gateway_stop");
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false, stopped: false }, live: true, error: errOf(e) };
  }
}

/* ---------------- MCP servers (persisted registry) ---------------- */

export type McpTransportKind = "stdio" | "http";
export type McpExposureKind = "gateway" | "local" | "disabled";

/** One mcp_list entry: a persisted registry server (the
 *  McpServerConfig fields) plus live status — supervised-process
 *  `running` (Local servers only) and credential presence over the
 *  accepted Credential Manager targets (presence only; values are
 *  never read). `credential_targets` from the Phase 4 brief does not
 *  exist in the payload: the found target rides in
 *  `credential_target_found`, and per-env-key presence in
 *  `credential_presence`. */
export interface McpServerRow {
  id: string;
  display_name: string;
  /** Same value as display_name (back-compat name field). */
  name: string;
  transport: McpTransportKind;
  exposure: McpExposureKind;
  /** Derived runtime mode: remote | local | disabled. */
  mode: string;
  command: string | null;
  args: string[];
  url: string | null;
  env_keys: string[];
  cred_ref: string | null;
  /** Registration / API-key page for the credential, when known. */
  signup_url: string | null;
  /** What credential to store (kind + where it comes from),
   *  resolved by the backend (stored value, else the seed's for
   *  the same id). Labels the card's credential field. */
  credential_hint: string | null;
  allowed_tools: string[] | null;
  metered: boolean;
  rpm_limit: number | null;
  running: boolean;
  credential_present: boolean;
  /** "present" | "missing" — the §9 card wording. */
  credential_status: string;
  /** The accepted Credential Manager target that was found, when
   *  one was (legacy servers accept either the unified wizard
   *  target or the legacy llm-manager/mcp/<id> target). */
  credential_target_found: string | null;
  /** Per-env-key presence as [env key, present] pairs. */
  credential_presence: [string, boolean][];
}

/** An MCP server definition as mcp_save_server accepts it: exactly
 *  the persisted McpServerConfig serde shape (transport and
 *  exposure are the lowercase tags; command/args serve stdio, url
 *  serves http). */
export interface McpServerInput {
  id: string;
  display_name: string;
  transport: McpTransportKind;
  exposure: McpExposureKind;
  command: string | null;
  args: string[];
  url: string | null;
  env_keys: string[];
  cred_ref: string | null;
  signup_url: string | null;
  credential_hint: string | null;
  allowed_tools: string[] | null;
  metered: boolean;
  rpm_limit: number | null;
}

/** Result of mcp_test_connection. Two payload shapes exist, keyed
 *  on the server's exposure:
 *  - Gateway: { id, ok, registered, tools, detail } — plus, for the
 *    curated servers after a passing test, real_call_ok /
 *    real_call_detail (brave_search's one real, metered call) or
 *    real_call_skipped (web_fetch never makes a real call).
 *  - Local / Disabled: { id, ok, detail } — a supervised-process
 *    liveness check (Local) or an unconditional not-ok (Disabled);
 *    the Gateway-only fields stay absent. */
export interface McpTestResult {
  id: string;
  ok: boolean;
  detail: string;
  registered?: boolean;
  tools?: string[];
  real_call_ok?: boolean;
  real_call_detail?: string;
  real_call_skipped?: boolean;
}

/** Sample-mode registry mirroring the four seeded servers, so the
 *  MCP view renders under the browser smoke test. */
function sampleMcpServers(): McpServerRow[] {
  const base = {
    name: "",
    mode: "disabled",
    command: null as string | null,
    args: [] as string[],
    url: null as string | null,
    env_keys: [] as string[],
    cred_ref: null as string | null,
    signup_url: null as string | null,
    credential_hint: null as string | null,
    allowed_tools: null as string[] | null,
    metered: false,
    rpm_limit: null as number | null,
    running: false,
    credential_present: false,
    credential_status: "missing",
    credential_target_found: null as string | null,
    credential_presence: [] as [string, boolean][],
  };
  return [
    {
      ...base, id: "github", display_name: "GitHub", name: "GitHub",
      transport: "stdio", exposure: "local", mode: "local",
      command: "npx", args: ["-y", "@modelcontextprotocol/server-github"],
      env_keys: ["GITHUB_PERSONAL_ACCESS_TOKEN"],
      cred_ref: "local-llm-service-manager/github-mcp",
      credential_hint: "GitHub personal access token — created under GitHub Settings, Developer settings, Personal access tokens",
      running: true, credential_present: true, credential_status: "present",
      credential_target_found: "local-llm-service-manager/github-mcp",
      credential_presence: [["GITHUB_PERSONAL_ACCESS_TOKEN", true]],
    },
    {
      ...base, id: "gdrive", display_name: "Google Drive", name: "Google Drive",
      transport: "stdio", exposure: "local", mode: "local",
      command: "npx", args: ["-y", "@modelcontextprotocol/server-gdrive"],
      env_keys: ["GOOGLE_APPLICATION_CREDENTIALS"],
      cred_ref: "local-llm-service-manager/gdrive-mcp",
      credential_hint: "Path to the Google OAuth credentials JSON file — downloaded from Google Cloud Console, APIs & Services, Credentials; store the file path, not the file contents",
      credential_present: true, credential_status: "present",
      credential_target_found: "local-llm-service-manager/gdrive-mcp",
      credential_presence: [["GOOGLE_APPLICATION_CREDENTIALS", true]],
    },
    {
      ...base, id: "brave_search", display_name: "Brave Search", name: "Brave Search",
      transport: "stdio", exposure: "gateway", mode: "remote",
      command: "npx",
      args: ["-y", "@brave/brave-search-mcp-server@2.1.4", "--transport", "stdio"],
      env_keys: ["BRAVE_API_KEY"],
      cred_ref: "local-llm-service-manager/brave-search-mcp",
      credential_hint: "Brave Search API key — the subscription key from the Brave Search API page",
      // The live entry resolves the Brave Search API page here;
      // the literal URL stays out of the sample fixture because
      // the smoke test bans external URLs in the bundle.
      signup_url: null,
      allowed_tools: ["brave_web_search", "brave_llm_context"],
      metered: true, rpm_limit: 20,
      credential_presence: [["BRAVE_API_KEY", false]],
    },
    {
      ...base, id: "web_fetch", display_name: "Web Fetch", name: "Web Fetch",
      transport: "stdio", exposure: "disabled", mode: "disabled",
      command: "uvx", args: ["mcp-server-fetch@2026.10.10"],
      allowed_tools: ["fetch"],
    },
  ];
}

/** mcp_list — the persisted MCP registry with live status. */
export async function getMcpServers(): Promise<CallResult<McpServerRow[]>> {
  if (!isTauri()) return { data: sampleMcpServers(), live: false };
  try {
    const data = await invoke<McpServerRow[]>("mcp_list");
    return { data: Array.isArray(data) ? data : [], live: true };
  } catch (e) {
    return { data: [], live: true, error: errOf(e) };
  }
}

/** mcp_test_connection — test one server (see McpTestResult for the
 *  per-exposure payload shapes). For brave_search a passing test
 *  performs one real, metered tool call — this wrapper is only ever
 *  invoked from the view's explicit Test action, never on load or a
 *  timer (design §8). */
export async function testMcpServer(id: string): Promise<CallResult<McpTestResult>> {
  if (!isTauri()) return { data: { id, ok: false, detail: "sample mode — tests run in the installed app" }, live: false };
  try {
    const data = await invoke<McpTestResult>("mcp_test_connection", { id });
    return { data, live: true };
  } catch (e) {
    return { data: { id, ok: false, detail: "" }, live: true, error: errOf(e) };
  }
}

/** mcp_save_server — create or update (upsert by id) a registry
 *  server. The command validates the whole config on save (§3.2);
 *  a violation comes back as `error` and nothing is stored. */
export async function saveMcpServer(server: McpServerInput): Promise<CallResult<{ id: string; persisted: boolean }>> {
  if (!isTauri()) return { data: { id: server.id, persisted: false }, live: false };
  try {
    const data = await invoke<{ id: string; persisted: boolean }>("mcp_save_server", { server });
    return { data, live: true };
  } catch (e) {
    return { data: { id: server.id, persisted: false }, live: true, error: errOf(e) };
  }
}

/** mcp_delete_server — remove a registry server (a running Local
 *  server is stopped first). */
export async function deleteMcpServer(id: string): Promise<CallResult<{ id: string; deleted: boolean; persisted: boolean }>> {
  if (!isTauri()) return { data: { id, deleted: true, persisted: false }, live: false };
  try {
    const data = await invoke<{ id: string; deleted: boolean; persisted: boolean }>("mcp_delete_server", { id });
    return { data, live: true };
  } catch (e) {
    return { data: { id, deleted: false, persisted: false }, live: true, error: errOf(e) };
  }
}

/** mcp_set_exposure — set a server's exposure (gateway | local |
 *  disabled), persisted through the validated save path. */
export async function setMcpExposure(id: string, exposure: McpExposureKind): Promise<CallResult<{ id: string; exposure: string; persisted: boolean }>> {
  if (!isTauri()) return { data: { id, exposure, persisted: false }, live: false };
  try {
    const data = await invoke<{ id: string; exposure: string; persisted: boolean }>("mcp_set_exposure", { id, exposure });
    return { data, live: true };
  } catch (e) {
    return { data: { id, exposure, persisted: false }, live: true, error: errOf(e) };
  }
}

/** mcp_store_credential — store a server's credential in Windows
 *  Credential Manager under its declared cred_ref target. The value
 *  is passed to the command only; it is never returned or rendered. */
export async function storeMcpCredential(id: string, value: string): Promise<CallResult<{ id: string; stored: boolean }>> {
  if (!isTauri()) return { data: { id, stored: false }, live: false };
  try {
    const data = await invoke<{ id: string; stored: boolean }>("mcp_store_credential", { id, value });
    return { data, live: true };
  } catch (e) {
    return { data: { id, stored: false }, live: true, error: errOf(e) };
  }
}

/** One mcp_start_preflight entry: a Gateway-exposed MCP server a
 *  gateway start would omit for want of a credential — the store
 *  it via mcp_store_credential before the start and the start's
 *  arming pass includes it in that same start. */
export interface McpStartPreflightServer {
  id: string;
  display_name: string;
  cred_ref: string;
  env_keys: string[];
  signup_url: string | null;
  credential_hint: string | null;
  metered: boolean;
}

/** mcp_start_preflight — the enabled-but-unarmed Gateway MCP
 *  servers, for the start-gateway credential prompt
 *  (ui/src/mcp-prompt.ts). Empty when every Gateway server's
 *  credential is present. */
export async function mcpStartPreflight(): Promise<CallResult<{ servers: McpStartPreflightServer[] }>> {
  if (!isTauri()) return { data: { servers: [] }, live: false };
  try {
    const data = await invoke<{ servers: McpStartPreflightServer[] }>("mcp_start_preflight");
    return { data: { servers: Array.isArray(data?.servers) ? data.servers : [] }, live: true };
  } catch (e) {
    return { data: { servers: [] }, live: true, error: errOf(e) };
  }
}

/** backend_test_tool_call — probe a backend for structured
 *  tool_calls (MCP design §7.2); the outcome persists on the
 *  backend and renders on its card as the Tool calls line. */
export async function testBackendToolCall(id: string): Promise<CallResult<BackendToolCallTestResult>> {
  const fallback: BackendToolCallTestResult = { outcome: "failed", finish_reason: null, chat_template_tool_use: false, chat_template_caps: null, detail: "", latency_ms: 0 };
  if (!isTauri()) return { data: fallback, live: false, error: "backend_test_tool_call is not available in sample mode" };
  try {
    const data = await invoke<BackendToolCallTestResult>("backend_test_tool_call", { id });
    return { data, live: true };
  } catch (e) {
    return { data: fallback, live: true, error: errOf(e) };
  }
}

/** One saved change inside a pending activation plan. */
export interface ActivationChange {
  ts: string;
  kind: string;
  summary: string;
  /** What activates the change: "gateway" (a gateway restart) or
   *  "backend" (the target backends' next (re)start). */
  scope?: string;
  /** Backend ids a backend-scope change applies to. */
  targets?: string[];
  /** How the change goes live: "gateway_restart" or "restart"
   *  (a running process must restart), or "next_start" (nothing
   *  running holds the old state). */
  activation?: string;
}

/** gateway_activation_plan — every saved configuration change that
 *  is not live yet, across scopes: gateway changes wait on a
 *  gateway restart, backend changes (flags, tensor split, model
 *  assignment) wait on their backend's (re)start. The plan is inert
 *  until the operator applies it, defers it, or cancels the
 *  changes. */
export interface ActivationPlanData {
  pending: boolean;
  gateway_up: boolean;
  /** A gateway-side change is pending (journal entry or drift). */
  gateway_pending?: boolean;
  /** Live drift against the RUNNING gateway: the files it was
   *  started with differ from the saved configuration. Always
   *  false while the gateway is down — a stale file then is not
   *  pending work, since the next start regenerates it. */
  drift: boolean;
  /** The raw generated-vs-on-disk file comparison, in any
   *  gateway state. Informational while the gateway is down (the
   *  next start regenerates the files); when drift is pending
   *  work it is also itemized in `changes` as a `gateway_drift`
   *  item, so a pending plan never counts zero changes. */
  files_differ?: boolean;
  /** Pending while the gateway is down: the changes take effect at
   *  the next start rather than needing a restart. */
  applies_at_next_start: boolean;
  /** Something Apply can do right now: restart the gateway (or
   *  start it) or restart a running backend. */
  actionable?: boolean;
  /** Running backends a plan application restarts. */
  backend_restart_ids?: string[];
  changes: ActivationChange[];
  /** A pre-change config snapshot exists, so cancel can restore it. */
  snapshot_available: boolean;
}

const EMPTY_PLAN: ActivationPlanData = {
  pending: false, gateway_up: false, drift: false, files_differ: false,
  applies_at_next_start: false, changes: [], snapshot_available: false,
};

export async function getActivationPlan(): Promise<CallResult<ActivationPlanData>> {
  if (!isTauri()) return { data: EMPTY_PLAN, live: false };
  try {
    const data = await invoke<ActivationPlanData>("gateway_activation_plan");
    return { data, live: true };
  } catch (e) {
    return { data: EMPTY_PLAN, live: true, error: errOf(e) };
  }
}

/** gateway_activation_cancel — restore the pre-change snapshot and
 *  clear the plan. Refused (error) when the config has since changed
 *  through another path. */
export async function activationCancelChanges(): Promise<CallResult<{ cancelled: boolean; reason?: string }>> {
  if (!isTauri()) return { data: { cancelled: false }, live: false };
  try {
    const data = await invoke<{ cancelled: boolean; reason?: string }>("gateway_activation_cancel");
    return { data, live: true };
  } catch (e) {
    return { data: { cancelled: false }, live: true, error: errOf(e) };
  }
}

/** gateway_activation_complete — clear the plan after a successful
 *  gateway (re)start made the saved changes live. */
export async function activationComplete(): Promise<CallResult<{ cleared: boolean }>> {
  if (!isTauri()) return { data: { cleared: false }, live: false };
  try {
    const data = await invoke<{ cleared: boolean }>("gateway_activation_complete");
    return { data, live: true };
  } catch (e) {
    return { data: { cleared: false }, live: true, error: errOf(e) };
  }
}

/** One plan change inside a recorded activation execution, with
 *  the outcome the apply recorded for it. */
export interface ActivationExecutionItem {
  /** When the change was saved (the journal entry's timestamp). */
  ts: string;
  kind: string;
  summary: string;
  scope: string;
  targets: string[];
  activation: string;
  /** "applied", "skipped", or "failed". */
  outcome: string;
  /** Skip reason or failure error text; absent for applied items. */
  detail?: string;
}

/** One recorded activation plan execution (a whole apply). Status:
 *  "completed", "completed_with_failures", or "failed" (aborted
 *  before every step was attempted). */
export interface ActivationExecution {
  id: string;
  started_at: string;
  finished_at: string;
  status: string;
  /** The surface that initiated the apply ("desktop"). */
  source: string;
  /** Apply mode: "quiesce", "restart", or "start". */
  mode?: string;
  drift: boolean;
  gateway_up: boolean;
  gateway_pending: boolean;
  items: ActivationExecutionItem[];
}

/** The list form of a recorded execution: identity, times, status,
 *  and per-outcome item counts (the full items ride the detail). */
export interface ActivationExecutionSummary {
  id: string;
  started_at: string;
  finished_at: string;
  status: string;
  source: string;
  mode?: string;
  drift: boolean;
  gateway_up: boolean;
  gateway_pending: boolean;
  items: number;
  applied: number;
  skipped: number;
  failed: number;
}

/** gateway_activation_record_execution — report one finished
 *  activation plan apply for the durable execution history. The
 *  apply funnel calls this when an apply ends (including an aborted
 *  one) and ignores the result: recording is best-effort and must
 *  never affect the activation it describes. The record argument is
 *  the execution without its server-assigned id. */
export async function recordActivationExecution(
  record: Omit<ActivationExecution, "id">,
): Promise<CallResult<{ recorded: boolean; id?: string | null }>> {
  if (!isTauri()) return { data: { recorded: false }, live: false };
  try {
    const data = await invoke<{ recorded: boolean; id?: string | null }>("gateway_activation_record_execution", { record });
    return { data, live: true };
  } catch (e) {
    return { data: { recorded: false }, live: true, error: errOf(e) };
  }
}

/** gateway_activation_history — the recorded activation plan
 *  executions, newest first. With an execution id, that execution's
 *  full record (or null when it has aged out of the retained
 *  history). */
export async function getActivationHistory(): Promise<CallResult<{ executions: ActivationExecutionSummary[] }>> {
  if (!isTauri()) return { data: { executions: [] }, live: false };
  try {
    const data = await invoke<{ executions: ActivationExecutionSummary[] }>("gateway_activation_history");
    return { data, live: true };
  } catch (e) {
    return { data: { executions: [] }, live: true, error: errOf(e) };
  }
}

export async function getActivationExecution(id: string): Promise<CallResult<{ execution: ActivationExecution | null }>> {
  if (!isTauri()) return { data: { execution: null }, live: false };
  try {
    const data = await invoke<{ execution: ActivationExecution | null }>("gateway_activation_history", { executionId: id });
    return { data, live: true };
  } catch (e) {
    return { data: { execution: null }, live: true, error: errOf(e) };
  }
}

/** gateway_quiesce_wait — wait for in-flight gateway requests to
 *  drain to zero (bounded by timeoutSecs); reports the final count
 *  when the wait times out. */
export async function quiesceWait(timeoutSecs: number): Promise<CallResult<{ drained: boolean; in_flight: number; timed_out: boolean }>> {
  if (!isTauri()) return { data: { drained: true, in_flight: 0, timed_out: false }, live: false };
  try {
    const data = await invoke<{ drained: boolean; in_flight: number; timed_out: boolean }>("gateway_quiesce_wait", { timeoutSecs });
    return { data, live: true };
  } catch (e) {
    return { data: { drained: false, in_flight: 0, timed_out: false }, live: true, error: errOf(e) };
  }
}

/** update_gateway_config — merge a JSON patch into the gateway
 *  configuration; `persisted` reports whether the change was saved to
 *  the configuration file (an in-memory-only apply reverts on restart). */
export async function updateGatewayConfig(patch: Record<string, unknown>): Promise<CallResult<{ ok: boolean; persisted: boolean }>> {
  if (!isTauri()) return { data: { ok: false, persisted: false }, live: false, error: "update_gateway_config is not available in sample mode" };
  try {
    const data = await invoke<{ ok: boolean; persisted: boolean }>("update_gateway_config", { patch });
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false, persisted: false }, live: true, error: errOf(e) };
  }
}

/** gateway_preview_yaml — the generated LiteLLM config.yaml, plus
 *  the generated llmdil_routing.py callback when a typed routing
 *  rule is enabled (null otherwise). */
export async function getGatewayYaml(): Promise<CallResult<{ yaml: string; routing_py: string | null }>> {
  if (!isTauri()) return { data: { yaml: "", routing_py: null }, live: false };
  try {
    const data = await invoke<{ yaml: string; routing_py?: string | null }>("gateway_preview_yaml");
    return { data: { yaml: data.yaml, routing_py: data.routing_py ?? null }, live: true };
  } catch (e) {
    return { data: { yaml: "", routing_py: null }, live: true, error: errOf(e) };
  }
}

/* ---------------- Settings ---------------- */

/** get_config — the full AppConfig JSON (data_dir, telemetry, mxc, updates, ...). */
/* ---------------- Windows service supervision ---------------- */

/** One managed service's row in the services_list payload. */
export interface ServiceRow {
  /** A backend id, or "gateway". */
  target: string;
  service_name: string;
  registered: boolean;
  /** SCM state (snake_case), null when not registered. */
  state: string | null;
  pid: number | null;
}

/** services_list payload: mode, resolved account, shim presence, rows. */
export interface ServicesInfo {
  mode: string;
  account: string | null;
  host_path: string;
  host_present: boolean;
  services: ServiceRow[];
}

export async function servicesList(): Promise<CallResult<ServicesInfo>> {
  if (!isTauri()) {
    return { data: { mode: "process", account: null, host_path: "", host_present: false, services: [] }, live: false };
  }
  try {
    const data = await invoke<ServicesInfo>("services_list");
    return { data, live: true };
  } catch (e) {
    return { data: { mode: "process", account: null, host_path: "", host_present: false, services: [] }, live: true, error: errOf(e) };
  }
}

/** Persist the supervision mode through the generic config patch. */
export async function setSupervisionMode(mode: string): Promise<CallResult<{ ok: boolean; persisted?: boolean }>> {
  if (!isTauri()) return { data: { ok: false }, live: false, error: "update_config is not available in sample mode" };
  try {
    const data = await invoke<{ ok: boolean; persisted?: boolean }>("update_config", { patch: { supervision: { mode } } });
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false }, live: true, error: errOf(e) };
  }
}

/** Register one service (backend id or "gateway") via the elevated
 *  helper. The account password is handed over through a transient
 *  credential the helper deletes; it is never persisted by the app. */
export async function servicesRegister(target: string, password: string): Promise<CallResult<{ ok: boolean; service_name: string; account: string }>> {
  if (!isTauri()) return { data: { ok: false, service_name: "", account: "" }, live: false, error: "services_register is not available in sample mode" };
  try {
    const data = await invoke<{ ok: boolean; service_name: string; account: string }>("services_register", { target, password });
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false, service_name: "", account: "" }, live: true, error: errOf(e) };
  }
}

export async function servicesUnregister(target: string): Promise<CallResult<{ ok: boolean; service_name: string }>> {
  if (!isTauri()) return { data: { ok: false, service_name: "" }, live: false, error: "services_unregister is not available in sample mode" };
  try {
    const data = await invoke<{ ok: boolean; service_name: string }>("services_unregister", { target });
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false, service_name: "" }, live: true, error: errOf(e) };
  }
}

export async function getAppConfig(): Promise<CallResult<Record<string, unknown> | null>> {
  if (!isTauri()) return { data: null, live: false };
  try {
    const data = await invoke<Record<string, unknown>>("get_config");
    return { data, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

/** One filesystem rule of an MXC policy: access is read | write |
 *  readwrite | deny (manager-mxc's FsAccess, lowercase). */
export interface MxcFsRule { path: string; access: string }
/** One network rule: decision is allow | prompt | deny. */
export interface MxcNetRule { host: string; ports: number[]; decision: string }
export interface MxcCredentialRef { name: string; target: string }

/** The MXC sandbox policy document (manager-mxc's Policy). */
export interface MxcPolicy {
  version: number;
  filesystem: MxcFsRule[];
  network: MxcNetRule[];
  credentials: MxcCredentialRef[];
  default_deny: boolean;
  ui_access: boolean;
}

/** get_mxc_policy payload: the policy, where it came from ("file" when
 *  a policy file was read, "default" when the built-in default policy
 *  is in effect because no file exists), and the current mode. */
export interface MxcPolicyData {
  policy: MxcPolicy | null;
  source: string;
  mode: string;
}

/** get_mxc_policy — the active MXC sandbox policy with its provenance. */
export async function getMxcPolicy(): Promise<CallResult<MxcPolicyData | null>> {
  if (!isTauri()) return { data: null, live: false };
  try {
    const raw = await invoke<{ policy?: MxcPolicy; source?: string; mode?: string }>("get_mxc_policy");
    return {
      data: {
        policy: raw.policy ?? null,
        source: typeof raw.source === "string" ? raw.source : "",
        mode: typeof raw.mode === "string" ? raw.mode : "",
      },
      live: true,
    };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

/** One observed decision in a Learning-mode activity report. `kind`
 *  serializes as the AccessKind variant name (FileRead | FileWrite |
 *  Network) when recorded. */
export interface MxcActivityEntry {
  timestamp: string;
  /** allow | prompt | deny */
  decision: string;
  description: string;
  kind: string | null;
  path_or_host: string | null;
  port: number | null;
  count: number;
}

export interface MxcActivityData {
  entries: MxcActivityEntry[];
  /** The report file path, or "none" when no report exists yet. */
  source: string;
}

/** mxc_get_activity_report — recent MXC sandbox decisions (Learning
 *  mode observations). An absent report is an empty entry list with
 *  source "none", not an error. */
export async function getMxcActivity(): Promise<CallResult<MxcActivityData | null>> {
  if (!isTauri()) return { data: null, live: false };
  try {
    const raw = await invoke<{ entries?: Partial<MxcActivityEntry>[]; source?: string }>("mxc_get_activity_report");
    const entries: MxcActivityEntry[] = (Array.isArray(raw.entries) ? raw.entries : []).map((e) => ({
      timestamp: typeof e.timestamp === "string" ? e.timestamp : "",
      decision: typeof e.decision === "string" ? e.decision : "",
      description: typeof e.description === "string" ? e.description : "",
      kind: typeof e.kind === "string" ? e.kind : null,
      path_or_host: typeof e.path_or_host === "string" ? e.path_or_host : null,
      port: typeof e.port === "number" ? e.port : null,
      count: typeof e.count === "number" ? e.count : 1,
    }));
    return { data: { entries, source: typeof raw.source === "string" ? raw.source : "" }, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

/* ---------------- Telemetry actions ---------------- */

/** telemetry_set_interval — set the GPU poll interval (ms). Persists
 *  immediately, but the running poller's cadence is fixed at startup —
 *  the new interval applies on restart. */
export async function setTelemetryInterval(ms: number): Promise<CallResult<{ ok: boolean }>> {
  if (!isTauri()) return { data: { ok: true }, live: false };
  try {
    const data = await invoke<{ ok: boolean }>("telemetry_set_interval", { ms });
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false }, live: true, error: errOf(e) };
  }
}

/** telemetry_reset_peaks — reset recorded peak GPU values. */
export async function resetTelemetryPeaks(): Promise<CallResult<{ ok: boolean }>> {
  if (!isTauri()) return { data: { ok: true }, live: false };
  try {
    const data = await invoke<{ ok: boolean }>("telemetry_reset_peaks");
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false }, live: true, error: errOf(e) };
  }
}

/** telemetry_save_snapshot — persist a telemetry snapshot to the data
 *  dir; returns the path it was written to. */
export async function saveTelemetrySnapshot(): Promise<CallResult<{ path: string }>> {
  if (!isTauri()) return { data: { path: "" }, live: false };
  try {
    const data = await invoke<{ path: string }>("telemetry_save_snapshot");
    return { data, live: true };
  } catch (e) {
    return { data: { path: "" }, live: true, error: errOf(e) };
  }
}

/** mxc_set_mode — set MXC enforcement mode: learning | enforced |
 *  disabled. `persisted` reports whether the change was saved to the
 *  configuration file; false means it applies to this session only.
 *  Sample mode returns an explicit not-available error (like the other
 *  action wrappers) — never a fake ok:true the view would announce as
 *  a mode change that did not happen. */
export async function setMxcMode(mode: string): Promise<CallResult<{ ok: boolean; mode: string; persisted?: boolean }>> {
  if (!isTauri()) return { data: { ok: false, mode }, live: false, error: "mxc_set_mode is not available in sample mode" };
  try {
    const data = await invoke<{ ok: boolean; mode: string; persisted?: boolean }>("mxc_set_mode", { mode });
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false, mode }, live: true, error: errOf(e) };
  }
}

/** Notification preferences (Settings → Notifications). Mirrors
 *  `NotificationConfig` in manager-config: `enabled` is the master
 *  switch; each category switch is ANDed with it app-side. */
export interface NotificationSettings {
  enabled: boolean;
  operations: boolean;
  failures: boolean;
  activation: boolean;
}

export async function setNotificationConfig(config: NotificationSettings): Promise<CallResult<{ ok: boolean; persisted?: boolean }>> {
  if (!isTauri()) return { data: { ok: false }, live: false, error: "notifications_set_config is not available in sample mode" };
  try {
    const data = await invoke<{ ok: boolean; persisted?: boolean }>("notifications_set_config", { config });
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false }, live: true, error: errOf(e) };
  }
}

/** Fire one toast immediately, bypassing the category switches —
 *  the Settings card's delivery check. */
export async function sendTestNotification(): Promise<CallResult<{ ok: boolean }>> {
  if (!isTauri()) return { data: { ok: false }, live: false, error: "notifications_send_test is not available in sample mode" };
  try {
    const data = await invoke<{ ok: boolean }>("notifications_send_test");
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false }, live: true, error: errOf(e) };
  }
}

/** One MXC self-test probe outcome (manager-mxc's SelfTestCase minus
 *  the request detail, which the view does not render). */
export interface MxcSelfTestCase {
  name: string;
  description: string;
  /** benign | malicious */
  kind: string;
  expected: string;
  actual: string;
  passed: boolean;
}

export interface MxcSelfTestData {
  cases: MxcSelfTestCase[];
  summary: { total: number; passed: number; failed: number };
}

/** mxc_run_self_test — evaluate the fixed probe set against the
 *  current policy. Pure policy evaluation: nothing is executed and no
 *  network traffic is sent. */
export async function mxcRunSelfTest(): Promise<CallResult<MxcSelfTestData>> {
  const empty: MxcSelfTestData = { cases: [], summary: { total: 0, passed: 0, failed: 0 } };
  if (!isTauri()) return { data: empty, live: false, error: "mxc_run_self_test is not available in sample mode" };
  try {
    const data = await invoke<MxcSelfTestData>("mxc_run_self_test");
    return { data, live: true };
  } catch (e) {
    return { data: empty, live: true, error: errOf(e) };
  }
}

/* ---------------- Placement ---------------- */

export interface ModelAssignResult {
  task_id: string;
  /** True only when the assignment was saved to the configuration file. */
  persisted: boolean;
}

/** model_assign — assign a model to a backend: sets the backend's
 *  model_file to the model's gguf_path, applies the tensor split, and
 *  persists. This is the one command that aligns a backend with a
 *  model; the Placement flow calls it when the selected backend does
 *  not already resolve to the selected model. The split is validated
 *  against the discovered GPU count by the command. */
export async function modelAssign(backendId: string, modelId: string, split: number[]): Promise<CallResult<ModelAssignResult>> {
  const fallback: ModelAssignResult = { task_id: "", persisted: false };
  if (!isTauri()) return { data: fallback, live: false, error: "model_assign is not available in sample mode" };
  try {
    const data = await invoke<ModelAssignResult>("model_assign", { id: modelId, backendId, split });
    return { data, live: true };
  } catch (e) {
    return { data: fallback, live: true, error: errOf(e) };
  }
}

/** placement_propose result (manager-models' PlacementProjection plus
 *  the snapshot metadata the command adds): per-GPU rows in split order.
 *  The fit math is weights-only — see the GPUs view's labels. */
export interface PlacementProjectionData {
  model_id: string;
  /** The evaluated per-GPU fractions (sums to 1.0). */
  split: number[];
  /** Projected weight allocation per GPU, MiB, in split order. */
  per_gpu_mib: number[];
  /** Free VRAM per GPU at snapshot time, MiB, in split order — from
   *  the server's snapshot, the same basis the fit verdict used. */
  free_mib?: number[];
  /** The serving backend's current weight allocation per GPU, MiB,
   *  in split order — credited toward the proposed allocation
   *  because applying a split restarts the backend and frees these
   *  weights first. Absent/zero when no backend was in scope or the
   *  backend is not running. */
  current_mib?: number[];
  /** Additional allocation each GPU must find in free VRAM, MiB:
   *  per_gpu_mib minus current_mib, floored at 0. */
  additional_mib?: number[];
  /** Per-GPU fit: additional allocation <= free VRAM at snapshot
   *  time (the backend's current allocation on that GPU credited). */
  per_gpu_fits: boolean[];
  /** True when every GPU's allocation fits. */
  fits: boolean;
  /** Age of the telemetry snapshot the projection used, ms. */
  snapshot_age_ms?: number;
  /** GPU index for each split entry, in split order. */
  gpu_indices?: number[];
  /** Indices of GPUs currently down (absent from telemetry polls);
   *  the projection treats them as having no free VRAM. */
  down_indices?: number[];
  /** Always true: the projection covers model weights only. */
  weights_only?: boolean;
  /** Weights resident in system RAM under the serving backend's
   *  expert offload, MiB (0/absent when no backend was in scope);
   *  the per-GPU figures then cover GPU-resident weights only. */
  sys_mib?: number;
  /** The weight total the projection divided: the full model size,
   *  or its GPU-resident share under expert offload. */
  gpu_weights_mib?: number;
}

/** placement_propose — project a tensor split for a catalog model onto
 *  the latest telemetry snapshot's free VRAM. When `backendId` names
 *  the serving backend, the projection is scoped to it: with expert
 *  offload the split divides the GPU-resident weights only and the
 *  result carries the system-RAM share. */
export async function placementPropose(modelId: string, split: number[], backendId?: string): Promise<CallResult<PlacementProjectionData | null>> {
  if (!isTauri()) return { data: null, live: false, error: "placement_propose is not available in sample mode" };
  try {
    const data = await invoke<PlacementProjectionData>("placement_propose", { modelId, split, backendId: backendId ?? null });
    return { data, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

export interface PlacementApplyResult {
  task_id?: string;
  /** Backends the split was written to. */
  backends?: string[];
  /** True only when the split was saved to the configuration file. */
  persisted: boolean;
}

/** placement_apply — validate a tensor split and write it to the
 *  backends serving the model, or to the single given backend when
 *  `backendId` is passed (the placement form's per-backend scope);
 *  `persisted` reports whether the change was saved (it takes effect
 *  on the backends' next restart). */
export async function placementApply(modelId: string, split: number[], backendId?: string): Promise<CallResult<PlacementApplyResult>> {
  const fallback: PlacementApplyResult = { persisted: false };
  if (!isTauri()) return { data: fallback, live: false, error: "placement_apply is not available in sample mode" };
  try {
    const data = await invoke<PlacementApplyResult>("placement_apply", { modelId, split, backendId: backendId ?? null });
    return { data, live: true };
  } catch (e) {
    return { data: fallback, live: true, error: errOf(e) };
  }
}

/* ---------------- Optimize (docs/optimize-design.md) ---------------- */

export type OptimizeDecision = "undecided" | "accepted" | "denied";

export interface OptimizeExpected {
  gpu_ram_mib: [number, number] | null;
  sys_ram_mib: [number, number] | null;
  note: string | null;
}

export interface OptimizeValidation {
  status: "valid" | "warning" | "invalid";
  reason: string | null;
  expected: OptimizeExpected;
}

export interface OptimizePlanItem {
  id: number;
  fingerprint: string;
  kind: string;
  target: string | null;
  change: Record<string, unknown>;
  advice_text: string | null;
  rationale: string;
  expected_effect: string;
  confidence: "high" | "medium" | "low";
  decision: OptimizeDecision;
  previously_denied: boolean;
  /** Experiment this item was chained to provision (register /
   *  download its candidate model), when it is one; absent on
   *  plans written before experiment provisioning. */
  provision_for_experiment?: number | null;
  apply_outcome: "applied" | "failed" | "not_attempted" | null;
  apply_error: string | null;
}

/** One experiment proposal's structured flags (Phase 5e,
 *  addendum A.3). Every field optional: absent is the temp
 *  backend's default, never an inherited value. */
export interface OptimizeExperimentFlags {
  n_ctx?: number | null;
  offload_layers?: number | null;
  tensor_split?: number[] | null;
}

export interface OptimizeExperimentPredicted {
  predicted_per_second?: number | null;
  fit?: string | null;
}

/** One benchmark prompt's stored result: median rates plus
 *  every per-run sample. Mirrors manager-optimize's
 *  BenchResult. */
export interface OptimizeBenchResult {
  prompt_per_second: number | null;
  predicted_per_second: number | null;
  ttft_ms: number | null;
  runs: { prompt_per_second: number | null; predicted_per_second: number | null; ttft_ms: number | null; latency_ms: number }[];
}

export interface OptimizeEvalRunRecord {
  backend_id: string;
  suite_version: number;
  run_at: string;
  results: unknown[];
  scores: { overall: OptimizeEvalScore; per_category: Record<string, OptimizeEvalScore> };
}

export interface OptimizeScoreDelta {
  baseline: OptimizeEvalScore;
  candidate: OptimizeEvalScore;
  delta_passed: number;
  delta_fraction: number;
}

export interface OptimizeEvalComparison {
  overall: OptimizeScoreDelta;
  per_category: Record<string, OptimizeScoreDelta>;
  screen_at_or_above_baseline: boolean;
  any_category_zero_that_baseline_passes: boolean;
  zero_categories_that_baseline_passes: string[];
}

/** One experiment run's recorded outcome (Phase 5e): survives
 *  cleanup and plan replacement via the experiment journal. */
export interface OptimizeExperimentResult {
  experiment_id: number;
  candidate_model_id: string;
  batch_id: string;
  status: "completed" | "failed" | "timeout" | "cancelled";
  detail: string;
  measured: { short: OptimizeBenchResult; long: OptimizeBenchResult } | null;
  eval: OptimizeEvalRunRecord | null;
  comparison: OptimizeEvalComparison | null;
  preflight: string;
  started_at: string;
  finished_at: string;
}

/** One experiment estimate pass's figures (tracker #56): the
 *  abbreviated benchmark's results, recorded on the proposal.
 *  An estimate is screening evidence — it is not a run result
 *  and the figures are always rendered as "Estimate". */
export interface OptimizeExperimentEstimate {
  short: OptimizeBenchResult;
  long: OptimizeBenchResult;
  batch_id: string;
  temp_backend_id: string;
  preflight: string;
  detail: string;
  estimated_at: string;
}

/** One experiment proposal (Phase 5e, addendum A.3): a sibling
 *  of the plan items, not one of them. `requires_stopping`
 *  proposals start denied; undecided never runs. */
export interface OptimizeExperiment {
  id: number;
  fingerprint: string;
  candidate_model_id: string;
  flags: OptimizeExperimentFlags;
  serves: string;
  hypothesis: string;
  predicted: OptimizeExperimentPredicted;
  requires_stopping: string[];
  run_eval: boolean;
  decision: OptimizeDecision;
  previously_denied: boolean;
  /** Plan items that register and download this experiment's
   *  candidate model, when the candidate is not registered and
   *  the plan provisions it; absent otherwise (and on plans
   *  written before experiment provisioning). */
  provisioning?: { model_id: string; item_ids: number[] } | null;
  result: OptimizeExperimentResult | null;
  /** The latest estimate pass's figures (tracker #56), or
   *  null/absent when no estimate pass has run. */
  estimate?: OptimizeExperimentEstimate | null;
}

/** A plan item excluded at plan build because a reference in it
 *  could not be grounded (tracker #51): a backend.create whose
 *  model is neither registered, nor in the catalog, nor among
 *  the consultation's discovered candidates. Recorded on the
 *  plan and stated in its summary; never offered as actionable. */
export interface OptimizeExcludedItem {
  kind: string;
  target?: string | null;
  reason: string;
}

export interface OptimizePlan {
  summary: string;
  /** Operator directive that scoped this consultation (addendum
   *  A.2), stored verbatim on the plan; "" when none. */
  directive: string;
  items: OptimizePlanItem[];
  /** Items excluded at plan build for ungroundable references
   *  (tracker #51); absent on plans written before the field,
   *  hence optional in the type. */
  excluded_items?: OptimizeExcludedItem[];
  /** Experiment proposals (Phase 5e); absent on plans written
   *  before 5e, hence optional in the type. */
  experiments?: OptimizeExperiment[];
  experiments_dropped_invalid?: number;
  experiments_truncated?: number;
  /** Light validations aligned with `experiments` (same shape
   *  as the item validations). */
  experiment_validations?: OptimizeValidation[];
  consulted_backend: string;
  created_at: string;
  config_hash: string;
  dropped_invalid: number;
  truncated: number;
  dropped_fragments: string[];
  raw_response: string;
  context: unknown;
  validations: OptimizeValidation[];
  config_stale: boolean;
  sample: boolean;
}

export interface OptimizeMeasured {
  prompt_per_second: number | null;
  predicted_per_second: number | null;
  ttft_ms: number | null;
  age_secs: number;
}

export interface OptimizeBackendInfo {
  id: string;
  purpose: string;
  running: boolean;
  enabled: boolean;
  model_id: string | null;
  short_name: string | null;
  /** Current-fingerprint measurement summary (Phase 5a), or
   *  null when this backend's configuration has not been
   *  measured. */
  measured: OptimizeMeasured | null;
  /** Current-fingerprint ESTIMATE summary (tracker #56): the
   *  abbreviated benchmark's figures, or null. Always
   *  rendered as "Estimate", never as a measured figure. */
  estimated: OptimizeMeasured | null;
}

/** One discovered Hugging Face candidate (Phase 5c): one quant
 *  file of one repo, with HF-reported facts, the app's fit
 *  verdict ("fits" | "fits_with_split" | "does_not_fit" |
 *  "unchecked"), and its retrieval provenance. Mirrors
 *  manager-optimize's CandidateFact. */
export interface OptimizeCandidate {
  repo_id: string;
  file: string;
  architecture: string;
  params_b: number;
  ctx_max: number;
  size_bytes: number;
  downloads: number;
  fit: string;
  retrieved_at: string;
}

/** The stored discovery set: the candidates plus the directive
 *  they were gathered for (a consultation only receives them
 *  when its directive matches). */
export interface OptimizeCandidates {
  directive: string;
  retrieved_at: string;
  items: OptimizeCandidate[];
}

/** Passed/total for one screening-suite score line (overall
 *  or one category). Mirrors manager-optimize's Score. */
export interface OptimizeEvalScore {
  passed: number;
  total: number;
}

/** The stored screening-suite baseline (Phase 5d, addendum
 *  A.4): the incumbent backend's suite run, keyed by its
 *  config fingerprint at run time. `stale` means the stored
 *  fingerprint no longer matches the backend's current one
 *  (or the backend is gone, or the suite version moved) —
 *  a stale baseline is shown as history, never as current.
 *  `per_category` keys are the EvalCategory strings
 *  (instruction_following, factual_recall, arithmetic_reasoning,
 *  code_adjacent, summarization, tool_call_formatting,
 *  long_context_retrieval). Experiment results (Phase 5e)
 *  carry the same Score shape, so deltas render without
 *  reshaping. */
export interface OptimizeEvalBaseline {
  backend_id: string;
  config_fingerprint: string;
  suite_version: number;
  run_at: string;
  overall: OptimizeEvalScore;
  per_category: Record<string, OptimizeEvalScore>;
  stale: boolean;
  /** The §A.4 caveat, verbatim — stated wherever scores appear. */
  caveat: string;
}

export interface OptimizeState {
  purpose: string;
  /** The stored plan's directive (addendum A.2): the consult
   *  field's default text for the next consultation; "" when
   *  no plan carries one. */
  last_directive: string;
  purpose_is_seed: boolean;
  purpose_seed: string;
  groups: { name: string; purpose: string }[];
  backends: OptimizeBackendInfo[];
  context_summary: {
    gpus: number;
    backends: number;
    groups: number;
    models: number;
    measured_blocks: string[];
    traffic_window_label: string | null;
  };
  context: unknown;
  default_backend: string | null;
  better_backend: { id: string; short_name: string | null } | null;
  plan: OptimizePlan | null;
  /** The stored discovery pre-pass set (Phase 5c), or null when
   *  no discovery has run. */
  candidates: OptimizeCandidates | null;
  /** The stored screening-suite baseline (Phase 5d), or null
   *  when no baseline has been run. */
  eval_baseline: OptimizeEvalBaseline | null;
  consulting: boolean;
  /** A GPU-bound Optimize job (measurement, later an
   *  experiment batch) is currently running. */
  measuring: boolean;
}

/** optimize_get_state — the page's full state: purposes, context
 *  summary + document, and the stored plan with live validation. */
export async function getOptimizeState(): Promise<CallResult<OptimizeState | null>> {
  if (!isTauri()) return { data: mockOptimizeState(), live: false };
  try {
    const data = await invoke<OptimizeState>("optimize_get_state");
    return { data, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

async function optimizeStateCmd<T>(cmd: string, args?: Record<string, unknown>): Promise<CallResult<OptimizeState | null>> {
  if (!isTauri()) return { data: mockOptimizeState(), live: false };
  try {
    const data = await invoke<OptimizeState>(cmd, args);
    return { data, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

/** optimize_save_purpose — persist purpose text (deployment scope,
 *  or one group's / backend's). */
export function optimizeSavePurpose(scope: "deployment" | "group" | "backend", text: string, target?: string) {
  return optimizeStateCmd("optimize_save_purpose", { scope, target: target ?? null, text });
}

/** optimize_consult — run a consultation (or a local sample plan)
 *  and return the refreshed state with the new plan. Long-running:
 *  up to the 900 s task budget for a live consultation. `directive`
 *  is the optional operator directive scoping this consultation
 *  (addendum A.2); it is recorded verbatim on the produced plan. */
export function optimizeConsult(backendId?: string, sample = false, directive?: string, candidateRepos?: string[]) {
  return optimizeStateCmd("optimize_consult", { backendId: backendId ?? null, sample, directive: directive ?? null, candidateRepos: candidateRepos ?? null });
}

/** optimize_discover — run the Hugging Face discovery pre-pass
 *  (Phase 5c) for a directive: HF search seeded from the
 *  directive, per-repo records, and app-computed fit verdicts.
 *  Stores the result server-side and returns the refreshed
 *  state with the candidate set. */
export function optimizeDiscover(directive: string) {
  return optimizeStateCmd("optimize_discover", { directive });
}

/** optimize_cancel_consult — fire the scoped cancellation handle. */
export async function optimizeCancelConsult(): Promise<CallResult<{ cancelled: boolean }>> {
  if (!isTauri()) return { data: { cancelled: false }, live: false };
  try {
    const data = await invoke<{ cancelled: boolean }>("optimize_cancel_consult");
    return { data, live: true };
  } catch (e) {
    return { data: { cancelled: false }, live: true, error: errOf(e) };
  }
}

/** optimize_measure_backend — run the standardized benchmark
 *  (Phase 5a) against one running backend and return the
 *  refreshed state with its measurement summary. Long-running:
 *  two prompts × (warm-up + 3 runs), 600 s budget per run. */
export function optimizeMeasureBackend(backendId: string) {
  return optimizeStateCmd("optimize_measure_backend", { backendId });
}

/** optimize_estimate_backend — run the abbreviated benchmark
 *  (tracker #56) against one running backend and return the
 *  refreshed state with its estimate summary: the same two
 *  standardized prompts, one run each, no warm-up, reduced
 *  generation budgets. Stored as an estimate under its own
 *  basis; never replaces a measurement. Runs under the shared
 *  GPU-job slot, so optimizeCancelJob cancels it. */
export function optimizeEstimateBackend(backendId: string) {
  return optimizeStateCmd("optimize_estimate_backend", { backendId });
}

/** optimize_run_baseline — run the screening suite against a
 *  running backend and store the result as the baseline
 *  (Phase 5d). Long-running: inside the suite's 10-minute
 *  time-box. Runs under the shared GPU-job slot, so
 *  optimizeCancelJob cancels it. */
export function optimizeRunBaseline(backendId: string) {
  return optimizeStateCmd("optimize_run_baseline", { backendId });
}

/** optimize_cancel_job — fire the shared GPU-job cancellation
 *  handle (measurement / experiment jobs, not consultations). */
export async function optimizeCancelJob(): Promise<CallResult<{ cancelled: boolean }>> {
  if (!isTauri()) return { data: { cancelled: false }, live: false };
  try {
    const data = await invoke<{ cancelled: boolean }>("optimize_cancel_job");
    return { data, live: true };
  } catch (e) {
    return { data: { cancelled: false }, live: true, error: errOf(e) };
  }
}

/** optimize_set_item_state — set one plan item's tri-state decision. */
export function optimizeSetItemState(itemId: number, decision: OptimizeDecision) {
  return optimizeStateCmd("optimize_set_item_state", { itemId, decision });
}

/** optimize_set_experiment_state — set one experiment
 *  proposal's tri-state decision (Phase 5e). */
export function optimizeSetExperimentState(experimentId: number, decision: OptimizeDecision) {
  return optimizeStateCmd("optimize_set_experiment_state", { experimentId, decision });
}

/** optimize_run_experiments — run the plan's accepted
 *  experiments as one batch (Phase 5e): per-experiment
 *  preflight, temp backend outside every gateway group,
 *  standardized benchmark, screening eval when the proposal
 *  asks, guaranteed cleanup. Long-running: budgets are
 *  30 min per experiment, 60 min per batch, enforced
 *  server-side. Runs under the shared GPU-job slot, so
 *  optimizeCancelJob cancels it. */
export function optimizeRunExperiments() {
  return optimizeStateCmd("optimize_run_experiments");
}

/** optimize_estimate_experiments — run the plan's accepted
 *  experiments as one estimate batch (tracker #56): the same
 *  lifecycle and budgets as a full batch, but each experiment
 *  runs only the abbreviated benchmark against its temp
 *  backend and records the figures on the proposal as an
 *  estimate, so a batch can be screened before full runs are
 *  committed. Runs under the shared GPU-job slot, so
 *  optimizeCancelJob cancels it. */
export function optimizeEstimateExperiments() {
  return optimizeStateCmd("optimize_estimate_experiments");
}

/** optimize_set_item_outcome — record an apply outcome on one item
 *  (the Phase 4 orchestrator reports progress through this). */
export function optimizeSetItemOutcome(itemId: number, outcome: "applied" | "failed" | "not_attempted", error?: string) {
  return optimizeStateCmd("optimize_set_item_outcome", { itemId, outcome, error: error ?? null });
}

/** optimize_discard_plan — drop the stored plan and its decisions. */
export function optimizeDiscardPlan() {
  return optimizeStateCmd("optimize_discard_plan");
}

/** Short chip label per plan kind (mirrors manager-optimize's
 *  PlanKind::short_label). */
export function optimizeKindLabel(kind: string): string {
  switch (kind) {
    case "backend.create": return "New backend";
    case "backend.delete": return "Delete";
    case "backend.set_context": return "Context";
    case "backend.set_flags": return "Flags";
    case "backend.set_offload": return "Offload";
    case "backend.set_placement": return "Placement";
    case "backend.set_model": return "Model";
    case "backend.set_enabled": return "Enabled";
    case "group.set_members":
    case "group.set_strategy":
    case "group.set_fallbacks": return "Routing";
    case "group.create": return "New group";
    case "group.delete": return "Delete";
    case "model.add": return "New model";
    case "model.download": return "Download model";
    default: return "Advice";
  }
}

export function optimizeIsDestructive(kind: string): boolean {
  return kind === "backend.delete" || kind === "group.delete";
}

/** One-line summary of an item's target + change, from the
 *  normalized change payload (design §1.3 row anatomy). Rendered
 *  from data only — never model prose. */
export function optimizeChangeSummary(item: OptimizePlanItem, shortName: (modelId: string) => string): string {
  const c = item.change ?? {};
  const t = item.target ?? "";
  const num = (v: unknown): string => typeof v === "number" ? v.toLocaleString("en-US") : "?";
  const split = Array.isArray(c.tensor_split)
    ? (c.tensor_split as number[]).map((f) => `${Math.round(f * 100)}`).join(" / ") + " %"
    : "";
  switch (item.kind) {
    case "backend.create": {
      const parts = [`Model: ${shortName(String(c.model_id ?? ""))}`, `Port: ${num(c.port)}`];
      if (typeof c.group === "string") parts.push(`Group: ${c.group}`);
      if (typeof c.n_ctx === "number") parts.push(`Context: ${num(c.n_ctx)}`);
      if (typeof c.offload_layers === "number") parts.push(`Offload: ${num(c.offload_layers)} layers`);
      if (split) parts.push(`Split: ${split}`);
      return parts.join(" · ");
    }
    case "backend.delete": return `Backend: ${t}`;
    case "backend.set_context": return `Context: ${num(c.n_ctx)} tokens`;
    case "backend.set_flags": {
      const parts: string[] = [];
      if (typeof c.n_ctx === "number") parts.push(`Context: ${num(c.n_ctx)}`);
      if (typeof c.n_batch === "number") parts.push(`Batch: ${num(c.n_batch)}`);
      return parts.join(" · ");
    }
    case "backend.set_offload": return `Offload: ${num(c.offload_layers)} layers to system RAM`;
    case "backend.set_placement": return `Split: ${split}${typeof c.split_mode === "string" ? ` · Mode: ${c.split_mode}` : ""}`;
    case "backend.set_model": return `Model: ${shortName(String(c.model_id ?? ""))}`;
    case "backend.set_enabled": return c.enabled === true ? "Enable backend" : "Disable backend";
    case "group.set_members": return `Members: ${Array.isArray(c.members) ? (c.members as string[]).join(", ") : ""}`;
    case "group.set_strategy": return `Strategy: ${String(c.strategy ?? "")}`;
    case "group.set_fallbacks": return `Fallbacks: ${Array.isArray(c.fallbacks) && c.fallbacks.length ? (c.fallbacks as string[]).join(", ") : "none"}`;
    case "group.create": return `Members: ${Array.isArray(c.members) ? (c.members as string[]).join(", ") : ""} · Strategy: ${String(c.strategy ?? "simple-shuffle")}`;
    case "group.delete": return `Group: ${t}`;
    case "model.add": return `Source: ${String(c.source_url ?? "")}`;
    case "model.download": return "Downloads the GGUF file";
    default: return "";
  }
}

// ---------------------------------------------------------------------------
// Admin API (Remote Access settings): desktop-local commands served
// by the shell (src-tauri/src/main.rs), not registry operations.
// ---------------------------------------------------------------------------

export interface AdminApiStatus {
  enabled: boolean;
  running: boolean;
  port: number;
  bind: string;
  base_url: string | null;
  cert_sha256: string | null;
  firewall: string;
  note: string | null;
  user_count: number;
  admin_count: number;
}

export interface AdminApiUser {
  id: string;
  username: string;
  role: string;
  disabled: boolean;
  must_change_password: boolean;
  totp_enabled: boolean;
  created_ms: number;
}

const emptyStatus: AdminApiStatus = {
  enabled: false, running: false, port: 47821, bind: "0.0.0.0",
  base_url: null, cert_sha256: null, firewall: "unsupported",
  note: null, user_count: 0, admin_count: 0,
};

export async function adminApiGetStatus(): Promise<CallResult<AdminApiStatus>> {
  if (!isTauri()) return { data: emptyStatus, live: false };
  try {
    const data = await invoke<AdminApiStatus>("admin_api_get_status");
    return { data, live: true };
  } catch (e) {
    return { data: emptyStatus, live: true, error: errOf(e) };
  }
}

export async function adminApiSetEnabled(enabled: boolean): Promise<CallResult<AdminApiStatus>> {
  if (!isTauri()) return { data: emptyStatus, live: false, error: "admin_api_set_enabled is not available in sample mode" };
  try {
    const data = await invoke<AdminApiStatus>("admin_api_set_enabled", { enabled });
    return { data, live: true };
  } catch (e) {
    return { data: emptyStatus, live: true, error: errOf(e) };
  }
}

export async function adminApiListUsers(): Promise<CallResult<AdminApiUser[]>> {
  if (!isTauri()) return { data: [], live: false };
  try {
    const data = await invoke<{ users: AdminApiUser[] }>("admin_api_list_users");
    return { data: data.users, live: true };
  } catch (e) {
    return { data: [], live: true, error: errOf(e) };
  }
}

export async function adminApiCreateUser(username: string, password: string, role: string): Promise<CallResult<AdminApiUser | null>> {
  if (!isTauri()) return { data: null, live: false, error: "admin_api_create_user is not available in sample mode" };
  try {
    const data = await invoke<AdminApiUser>("admin_api_create_user", { username, password, role });
    return { data, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

export async function adminApiUpdateUser(id: string, role?: string, disabled?: boolean): Promise<CallResult<AdminApiUser | null>> {
  if (!isTauri()) return { data: null, live: false, error: "admin_api_update_user is not available in sample mode" };
  try {
    const data = await invoke<AdminApiUser>("admin_api_update_user", { id, role: role ?? null, disabled: disabled ?? null });
    return { data, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

export async function adminApiDeleteUser(id: string): Promise<CallResult<boolean>> {
  if (!isTauri()) return { data: false, live: false, error: "admin_api_delete_user is not available in sample mode" };
  try {
    await invoke<unknown>("admin_api_delete_user", { id });
    return { data: true, live: true };
  } catch (e) {
    return { data: false, live: true, error: errOf(e) };
  }
}

export async function adminApiResetPassword(id: string): Promise<CallResult<string | null>> {
  if (!isTauri()) return { data: null, live: false, error: "admin_api_reset_password is not available in sample mode" };
  try {
    const data = await invoke<{ temporary_password: string }>("admin_api_reset_password", { id });
    return { data: data.temporary_password, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

export interface AdminApiPairing {
  base_url: string;
  cert_sha256: string | null;
  enrollment_code?: string;
}

export async function adminApiCreateEnrollment(userId: string): Promise<CallResult<AdminApiPairing | null>> {
  if (!isTauri()) return { data: null, live: false, error: "admin_api_create_enrollment is not available in sample mode" };
  try {
    const data = await invoke<AdminApiPairing>("admin_api_create_enrollment", { userId });
    return { data, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

export interface AdminApiAuditEntry {
  ts_ms: number;
  actor: string;
  auth_type: string;
  source_ip: string;
  action: string;
  target: string | null;
  result: string;
  request_id: string;
}

export async function adminApiRegenerateCert(): Promise<CallResult<AdminApiStatus>> {
  if (!isTauri()) return { data: emptyStatus, live: false, error: "admin_api_regenerate_cert is not available in sample mode" };
  try {
    const data = await invoke<AdminApiStatus>("admin_api_regenerate_cert");
    return { data, live: true };
  } catch (e) {
    return { data: emptyStatus, live: true, error: errOf(e) };
  }
}

export async function adminApiGetAudit(limit = 20): Promise<CallResult<AdminApiAuditEntry[]>> {
  if (!isTauri()) return { data: [], live: false };
  try {
    const data = await invoke<{ entries: AdminApiAuditEntry[] }>("admin_api_get_audit", { limit });
    return { data: data.entries, live: true };
  } catch (e) {
    return { data: [], live: true, error: errOf(e) };
  }
}
