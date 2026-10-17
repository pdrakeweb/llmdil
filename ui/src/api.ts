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
import { mockBackends, mockDashboard, mockGpus, mockWizardSteps } from "./mock";
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
  /** The backend's persisted tool-call probe (MCP design §7.2),
   *  or null when it has never been probed — the card renders null
   *  as "not measured", never as a measured state. Absent on
   *  supervisor-only orphan rows, which carry no probe. */
  tool_call_probe?: ToolCallProbeData | null;
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
 *  Persisted immediately; applies on the next start. */
export async function setBackendOffload(id: string, layers: number): Promise<CallResult<{ backend_id: string; layers: number; raw_flags: string | null; changed: boolean }>> {
  if (!isTauri()) return { data: { backend_id: id, layers, raw_flags: null, changed: false }, live: false };
  try {
    const data = await invoke<{ backend_id: string; layers: number; raw_flags: string | null; changed: boolean }>("backend_set_offload", { id, layers });
    return { data, live: true };
  } catch (e) {
    return { data: { backend_id: id, layers, raw_flags: null, changed: false }, live: true, error: errOf(e) };
  }
}

/** backend_add — create a backend for a registered model and route it
 *  through a gateway group (an existing one, or a new one when
 *  createGroup is true). The gateway routes it after its next restart. */
export async function addBackend(input: {
  id: string;
  modelId: string;
  port: number;
  group: string;
  createGroup: boolean;
  enabled: boolean;
}): Promise<CallResult<{ backend_id: string; group: string }>> {
  if (!isTauri()) return { data: { backend_id: input.id, group: input.group }, live: false };
  try {
    const data = await invoke<{ backend_id: string; group: string }>("backend_add", {
      id: input.id,
      modelId: input.modelId,
      port: input.port,
      group: input.group,
      createGroup: input.createGroup,
      enabled: input.enabled,
    });
    return { data, live: true };
  } catch (e) {
    return { data: { backend_id: input.id, group: input.group }, live: true, error: errOf(e) };
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

export interface GatewayRule {
  id: string;
  description: string;
  source: string;
  /** The LiteLLM config fragment this rule injects, carried by the
   *  get_gateway_config payload; shown in the rule's Fragment
   *  expandable. Absent on payloads that predate the field. */
  litellm_fragment?: string;
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

export interface GatewayRule {
  id: string;
  description: string;
  source: string;
  /** The LiteLLM config fragment this rule injects, carried by the
   *  get_gateway_config payload; shown in the rule's Fragment
   *  expandable. Absent on payloads that predate the field. */
  litellm_fragment?: string;
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
      routing_rules: Array.isArray(raw.routing_rules) ? (raw.routing_rules as GatewayRule[]) : [],
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
  drift: boolean;
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
  pending: false, gateway_up: false, drift: false,
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

/** gateway_preview_yaml — the generated LiteLLM config.yaml. */
export async function getGatewayYaml(): Promise<CallResult<{ yaml: string }>> {
  if (!isTauri()) return { data: { yaml: "" }, live: false };
  try {
    const data = await invoke<{ yaml: string }>("gateway_preview_yaml");
    return { data, live: true };
  } catch (e) {
    return { data: { yaml: "" }, live: true, error: errOf(e) };
  }
}

/* ---------------- Settings ---------------- */

/** get_config — the full AppConfig JSON (data_dir, telemetry, mxc, updates, ...). */
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
