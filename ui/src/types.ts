/* Shared TypeScript types for the Local LLM Service Manager UI.
   Shapes mirror the Tauri command contracts (src-tauri/src/main.rs). */

export type Status = "ok" | "degraded" | "starting" | "down" | "idle";

export interface StatusResponse {
  status: string;
  scaffold: boolean;
  version: string;
}

export interface BackendInfo {
  id: string;
  name: string;
  port: number;
  status: Status;
  health: number; // 0-100
  model: string;
  quant: string;
  throughput_toks: number; // tok/s, 1m avg
  vram_held_gb: number;
  requests_1h: number;
  errors_1h: number;
  spark: number[];
}

export interface GpuInfo {
  id: string;
  name: string;
  pci: string;
  driver: string;
  status: Status;
  util_pct: number;
  vram_used_gb: number;
  vram_total_gb: number;
  temp_c: number;
  temp_peak_c: number;
  power_w: number;
  power_limit_w: number;
  util_spark: number[];
  temp_spark: number[];
  power_spark: number[];
  processes: { process: string; pid: number; vram_mib: number; backend: string }[];
  models: { model: string; layers: string }[];
}

export interface PlacementSegment {
  gpu: string; // gpu id
  pct: number; // 0-100
}

export interface PlacementRow {
  model: string;
  segments: PlacementSegment[];
}

export interface MapEvent {
  time: string;
  level: string; // "ok" | "warn" | "error" | "idle" | Status
  text: string;
}

export interface DashboardModel {
  gateway: BackendInfo | null; // null when the backend reports no gateway yet
  backends: BackendInfo[];
  gpus: GpuInfo[];
  placement: PlacementRow[];
  throughputSeries: { name: string; color: string; values: number[] }[];
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
}
