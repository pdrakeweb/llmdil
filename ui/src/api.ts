/* Tauri command boundary.
   - Under Tauri: calls window.__TAURI__.core.invoke and returns live data.
   - Plain browser: falls back to built-in mock data and reports live=false,
     so the UI renders sample data with a "disconnected" banner instead of failing.
   - Every invoke is wrapped in try/catch. On failure the result carries
     live=true (we ARE under Tauri) plus error=<message> and degraded data —
     callers must render a degraded banner, never present the fallback as
     healthy live data.
*/

import { mockBackends, mockDashboard, mockGpus, mockWizardSteps } from "./mock";
import type { BackendInfo, DashboardModel, GpuInfo, StatusResponse, WizardStepState } from "./types";

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
      return { data: { status: "unreachable", scaffold: true, version: "?" }, live: true, error: errOf(e) };
    }
  }
  return { data: { status: "ok", scaffold: false, version: "0.1.0-sample" }, live: false };
}

/** list_backends — Tauri stub returns { backends: [] }; render honestly. */
export async function listBackends(): Promise<CallResult<BackendInfo[]>> {
  if (isTauri()) {
    try {
      const r = await invoke<{ backends: BackendInfo[] }>("list_backends");
      return { data: Array.isArray(r.backends) ? r.backends : [], live: true };
    } catch (e) {
      return { data: [], live: true, error: errOf(e) };
    }
  }
  return { data: mockBackends(), live: false };
}

/** get_gpu_telemetry — Tauri stub returns { gpus: [], stale: false }. */
export async function getGpuTelemetry(): Promise<CallResult<{ gpus: GpuInfo[]; stale: boolean }>> {
  if (isTauri()) {
    try {
      const r = await invoke<{ gpus: GpuInfo[]; stale: boolean }>("get_gpu_telemetry");
      return {
        data: { gpus: Array.isArray(r.gpus) ? r.gpus : [], stale: Boolean(r.stale) },
        live: true,
      };
    } catch (e) {
      return { data: { gpus: [], stale: true }, live: true, error: errOf(e) };
    }
  }
  return { data: { gpus: mockGpus(), stale: false }, live: false };
}

/** Assemble the dashboard model from the three commands. */
export async function getDashboard(): Promise<CallResult<DashboardModel>> {
  const [s, b, g] = await Promise.all([getStatus(), listBackends(), getGpuTelemetry()]);
  const live = s.live && b.live && g.live;
  if (!live) {
    return { data: { ...mockDashboard(), stale: false, updatedAt: Date.now() }, live: false };
  }
  const errors = [s.error, b.error, g.error].filter((e): e is string => Boolean(e));
  // Live path: map whatever the backend returned. No fabrication — when the
  // backend reports nothing, gateway is null and the view shows an empty state.
  const all = b.data;
  const gateway = all.find((x) => x.id === "gateway" || x.port === 4000) ?? null;
  const backends = gateway ? all.filter((x) => x !== gateway) : all;
  const data: DashboardModel = {
    gateway, backends, gpus: g.data.gpus, placement: [], throughputSeries: [],
    events: [], stale: g.data.stale, updatedAt: Date.now(),
  };
  return errors.length ? { data, live: true, error: errors.join("; ") } : { data, live: true };
}

/** run_wizard_step — returns a task id; progress arrives via events (stub: accepted only). */
export async function runWizardStep(stepId: string): Promise<CallResult<{ task_id: string; step_id: string; accepted: boolean }>> {
  if (isTauri()) {
    try {
      const data = await invoke<{ task_id: string; step_id: string; accepted: boolean }>("run_wizard_step", { stepId });
      return { data, live: true };
    } catch (e) {
      return { data: { task_id: "", step_id: stepId, accepted: false }, live: true, error: errOf(e) };
    }
  }
  return { data: { task_id: "sample-task", step_id: stepId, accepted: true }, live: false };
}

/** Wizard steps — sample layout in the browser AND under Tauri until the
    backend exposes step state. Always reported as non-live sample data. */
export async function getWizardSteps(): Promise<CallResult<WizardStepState[]>> {
  return { data: mockWizardSteps(), live: false };
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
