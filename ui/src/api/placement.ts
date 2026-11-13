import type { FitStateName } from "./backends";
import { errOf, invoke, isTauri } from "./core";
import type { CallResult } from "./core";

/** placement_propose result (manager-models' PlacementProjection plus
 *  the snapshot metadata the command adds): per-GPU rows in split order.
 *  With a backend in scope the per-GPU figures are its FULL
 *  requirement (weights + KV + state + expert cache) and
 *  `weights_only` is false; without one they are weights only. */
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
  /** True when the projection covers model weights only (no
   *  backend in scope); false when `per_gpu_mib` is the backend's
   *  full requirement. */
  weights_only?: boolean;
  /** Weights resident in system RAM under the serving backend's
   *  expert offload, MiB (0/absent when no backend was in scope);
   *  the per-GPU figures then cover GPU-resident weights only. */
  sys_mib?: number;
  /** The weight total the projection divided: the full model size,
   *  or its GPU-resident share under expert offload. */
  gpu_weights_mib?: number;
  /** Full-requirement projections: the KV cache term, MiB. */
  kv_mib?: number;
  /** Full-requirement projections: the state cache term, MiB. */
  rs_mib?: number | null;
  /** Full-requirement projections: the expert-cache total, MiB
   *  (its per-GPU shares are inside `per_gpu_mib`). */
  moe_cache_mib?: number;
  /** Total VRAM per GPU, MiB, in split order. */
  gpu_total_mib?: number[];
  /** Per-GPU three-state fit classification of the required
   *  figure against total VRAM, in split order. */
  fit_states?: FitStateName[];
  /** Per-GPU paged amount, MiB (WDDM-paged GPUs only). */
  paged_mib?: number[];
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
