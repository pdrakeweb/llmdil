/* Shared TypeScript types for the Local LLM Service Manager UI.
   These are UI-side models. Wire contracts (the exact JSON the Tauri
   commands send and receive) live in api.ts — BackendRow, ModelRow, the
   wizard payloads, etc. Some types here are mapped from those wire shapes
   in api.ts; fields the backend does not supply are optional and are left
   undefined by the mapping rather than fabricated. */

export type Status = "ok" | "degraded" | "starting" | "down" | "idle";

export interface StatusResponse {
  status: string;
  version: string;
}

export interface BackendInfo {
  id: string;
  name: string;
  port: number;
  status: Status;
  model: string;
  /** True when the running process was adopted by this session (it was
   *  started by a previous launch and survived the app closing). */
  adopted?: boolean;
  /* The fields below have no backend source in the list_backends payload;
     they are only populated by the sample data. Live code must treat them
     as possibly absent and never render them as measurements when unset. */
  health?: number; // 0-100
  quant?: string;
  throughput_toks?: number; // tok/s, 1m avg
  vram_held_gb?: number;
  requests_1h?: number;
  errors_1h?: number;
  spark?: number[];
}

export interface GpuInfo {
  id: string;
  name: string;
  status: Status;
  util_pct: number;
  vram_used_gb: number;
  vram_total_gb: number;
  temp_c: number;
  power_w: number | null;
  /* temp_peak_c and power_limit_w ARE carried by the live payload
     (nullable there; undefined here when the payload carries null), as
     are fan_pct / mem_util_pct (nullable per GPU — e.g. fanless P100s
     report no fan) and the per-sample source kind. The payload's
     per-process VRAM attribution is mapped into `processes` (empty when
     the backend reports none). pci/driver/sparklines and `models` are
     not carried by the payload and are only populated by the sample
     data; live code must treat them as absent. */
  temp_peak_c?: number;
  power_limit_w?: number;
  fan_pct?: number;
  mem_util_pct?: number;
  /** Telemetry source kind for this sample, display-ready
   *  (e.g. "nvidia-smi", "NVML"). */
  source?: string;
  pci?: string;
  driver?: string;
  util_spark?: number[];
  temp_spark?: number[];
  power_spark?: number[];
  /** Per-process VRAM attribution. `vram_mib` is null when the
   *  figure was not measured (a WDDM GPU whose Windows counter data
   *  was unavailable) — unknown, not zero: consumers must not render
   *  it as 0 MiB, "context only", or measured residency. */
  processes?: { process: string; pid: number; vram_mib: number | null; backend: string }[];
  models?: { model: string; layers: string }[];
}

export interface PlacementSegment {
  gpu: string; // gpu label (telemetry GPU name live; gpu id in sample data)
  pct: number; // 0-100
  /** Position of this segment's GPU in the GPU list / split order,
   *  when the derivation knows it (live rows). Used to match segments
   *  to GPU nodes and to pick palette colors by GPU position. */
  gpuIndex?: number;
}

export interface PlacementRow {
  model: string;
  segments: PlacementSegment[];
  /** The backend that owns this placement (its model_file produced
   *  the row), when known. Sample rows may carry it too. */
  backendId?: string;
  /** The owning backend's runtime status, joined from list_backends
   *  at model assembly time; views may also resolve it live from the
   *  model's backend list via backendId. */
  backendStatus?: Status;
  /** Whether the owning backend is enabled in the configuration.
   *  A disabled backend's placement is eventual, not actual — the
   *  dashboard grays its row out (Pete, 2026-10-16: placements are
   *  per backend, and disabled backends still show where they
   *  WOULD place). */
  backendEnabled?: boolean;
}

export interface MapEvent {
  time: string;
  level: string; // "ok" | "warn" | "error" | "idle" | Status
  text: string;
}

export interface DashboardModel {
  gateway: BackendInfo | null; // null when the gateway probe errored (state unknown)
  backends: BackendInfo[];
  gpus: GpuInfo[];
  placement: PlacementRow[];
  throughputSeries: { name: string; color: string; values: number[] }[];
  /** Gateway probe result (gateway_status.up); feeds the shared
   *  system verdict. False when the probe failed or reported down. */
  gatewayUp: boolean;
  /** The one global LiteLLM routing strategy resolved from the
   *  gateway config, or null when no config/groups are available. */
  gatewayStrategy: string | null;
  /** Set when the gateway_status probe call itself failed (gateway
   *  state unknown). A probe failure does NOT fail the dashboard poll:
   *  the gateway card is absent and this message is shown as its own
   *  banner, while backends/telemetry data stays fresh. */
  gatewayError?: string;
  events: MapEvent[];
  stale: boolean;
  updatedAt: number;
}

export interface WizardStepState {
  id: string;
  name: string;
  detail: string;
  state: "done" | "running" | "queued" | "failed" | "skipped" | "manual";
  duration?: string;
  note?: string;
  helpUrl?: string;
  message?: string;
  logTail?: string[];
  remediation?: string;
  /** PowerShell text for running this step manually (failed steps
   * only), rendered with a copy button. Filled in by the wizard view
   * from setup_get_commands. */
  manualCommands?: string;
  /** Manual-command block the failed step's own outcome carried
   * (start-gateway: the exact start commands for this install,
   * secrets named but never valued). Preferred over
   * `manualCommands` when present — it is specific to the failure,
   * where the planned commands are generic. */
  manualCommand?: string;
}
