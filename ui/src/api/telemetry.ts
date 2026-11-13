import { fromWire, toStatus } from "../lifecycle-state";
import { mockBackends, mockDashboard, mockGpuHistory, mockGpus } from "../mock";
import type { BackendInfo, DashboardModel, GpuInfo, StatusResponse } from "../types";
import type { BackendRow } from "./backends";
import { errOf, invoke, isTauri } from "./core";
import type { CallResult } from "./core";
import { getGatewayConfig, getGatewayStatus } from "./gateway";
import type { GatewayStatusData } from "./gateway";
import { getAppConfig } from "./services";

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

/** A list_backends row -> the UI-side BackendInfo. The lifecycle state
 *  is the row's unified `lifecycle` (falling back to `state` on older
 *  payloads); `status` is derived from it with toStatus(). Exported so
 *  views holding raw rows (the service store) map them the same way. */
export function backendInfoFromRow(b: BackendRow): BackendInfo {
  const lifecycle = fromWire(b.lifecycle ?? b.state);
  return {
    id: b.id,
    name: b.id,
    port: b.port,
    model: b.model_file,
    status: toStatus(lifecycle),
    lifecycle,
    transition: b.transition ?? null,
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
  };
}

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
      const data: BackendInfo[] = rows.map(backendInfoFromRow);
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
  /** Why power_w is null, classified by the telemetry source:
   *  "driver_not_available" (driver answered N/A) or "field_missing"
   *  (no power element in the source output). Null/absent when power
   *  was measured, and absent on older payloads. */
  power_absence?: "driver_not_available" | "field_missing" | null;
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
        power_absence: s.power_absence ?? undefined,
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

/** One recorded GPU history point (get_gpu_history). throughput_toks
 *  and power_w are null when the value was not measured at that tick
 *  — never coerced to 0 (unknown is not zero). */
export interface GpuHistoryPoint {
  ts_ms: number;
  temp_c: number;
  util_pct: number;
  throughput_toks: number | null;
  power_w: number | null;
}

/** One GPU's history series: decimated points plus their nominal
 *  spacing (the chart breaks the line where the real gap exceeds
 *  3x interval_ms, and at null values). */
export interface GpuHistoryGpu {
  index: number;
  name: string;
  interval_ms: number;
  points: GpuHistoryPoint[];
}

export interface GpuHistoryData {
  range: string;
  gpus: GpuHistoryGpu[];
}

/** get_gpu_history — recorded per-GPU history for the GPUs page
 *  charts (temperature, utilization, throughput, power) over the
 *  "1h" / "24h" / "7d" ranges. */
export async function getGpuHistory(range: string): Promise<CallResult<GpuHistoryData>> {
  if (isTauri()) {
    try {
      const r = await invoke<{ range: string; gpus: GpuHistoryGpu[] }>("get_gpu_history", { range });
      return { data: { range: r.range, gpus: Array.isArray(r.gpus) ? r.gpus : [] }, live: true };
    } catch (e) {
      return { data: { range, gpus: [] }, live: true, error: errOf(e) };
    }
  }
  return { data: mockGpuHistory(range), live: false };
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

/** Assemble the dashboard model from the backend commands. Live
 *  placement is derived from the configured backends' tensor splits
 *  (see placementFromConfig) — empty when no split is configured, and
 *  the view renders an explicit empty state for that card. The gateway
 *  card comes from the gateway_status probe (up state + port), never
 *  from a list_backends row: no backend row represents the gateway.
 *  The gateway card's status is its lifecycle state (gateway_status
 *  .state), so a restart reads "restarting", never "down".
 *  `prefetched` lets a caller holding fresh list_backends /
 *  gateway_status results (the service store) skip those two calls. */
export async function getDashboard(prefetched?: {
  backends?: CallResult<BackendRow[]>;
  gateway?: CallResult<GatewayStatusData>;
}): Promise<CallResult<DashboardModel>> {
  const backendsCall: Promise<CallResult<BackendInfo[]>> = prefetched?.backends
    ? Promise.resolve({ ...prefetched.backends, data: prefetched.backends.data.map(backendInfoFromRow) })
    : listBackends();
  const gatewayCall = prefetched?.gateway ? Promise.resolve(prefetched.gateway) : getGatewayStatus();
  const [s, b, g, gw] = await Promise.all([getStatus(), backendsCall, getGpuTelemetry(), gatewayCall]);
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
  // Throughput sums only the backends whose live rate is actually
  // measured: a backend whose rate is unknown (absent) contributes
  // nothing — it is not a measured 0 — and the gateway total is
  // absent when no backend's rate is measured.
  const tputRows = b.data.filter((x) => x.throughput_toks !== undefined);
  const throughputSum: number | undefined = tputRows.length
    ? tputRows.reduce((a, x) => a + (x.throughput_toks ?? 0), 0)
    : undefined;
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
        status: toStatus(gw.data.state),
        lifecycle: gw.data.state,
        transition: gw.data.transition,
        model: "",
        requests_1h: sumStat((x) => x.requests_1h),
        errors_1h: sumStat((x) => x.errors_1h),
        throughput_toks: throughputSum,
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
    gatewayStrategy: gwCfg.data ? gwCfg.data.effective_strategy : null,
    gatewayState: gw.error ? null : gw.data.state,
    gatewayError: gw.error,
    openrouterSpend: gw.error ? null : gw.data.openrouter_spend ?? null,
    stale: g.data.stale, updatedAt: Date.now(),
  };
  return errors.length ? { data, live: true, error: errors.join("; ") } : { data, live: true };
}
