import { mockModelStorageStatus } from "../mock";
import { errOf, invoke, isTauri } from "./core";
import type { CallResult } from "./core";

/* ---------------- Tiered model storage ----------------
   docs/tiered-model-storage.md §10. A move target is a model id or a
   logical path (`models\X.gguf`) — unregistered files use the path. */

/** A tier's wire name: hot, or a cold tier — cold for the first,
 *  cold:N for a later one. */
export type StorageTierName = string;

/** One configured cold tier. */
export interface ColdTierConfig {
  path: string;
  label: string;
}

/** The config's `model_storage` section (§4). */
export interface ModelStorageConfig {
  cold_tiers: ColdTierConfig[];
  /** Cap on tierable GGUF bytes in the hot tier (GiB); null = none. */
  hot_budget_gib: number | null;
  /** Free space kept on the hot volume (GiB). */
  hot_min_free_gib: number;
  promote_after_starts: number;
  promote_window_days: number;
  /** null = never demote for idleness. */
  demote_after_idle_days: number | null;
  keep_hot: string[];
  download_tier: "hot" | "cold";
  /** 0 = unthrottled. */
  copy_rate_mib_s: number;
  auto: boolean;
}

/** A file's in-flight move: running (fraction 0..1, task id) or
 *  queued behind another move. */
export interface StorageMoving {
  kind: "promote" | "demote";
  to?: StorageTierName;
  fraction?: number;
  paused?: boolean;
  task_id?: string;
  queued?: boolean;
}

export interface StorageTierFile {
  file: string;
  model_id: string | null;
  bytes: number;
}

export interface StorageTierStatus {
  tier: StorageTierName;
  path: string;
  label: string;
  available: boolean;
  /** Volume figures; null when unknown (an unavailable cold tier). */
  free_bytes: number | null;
  total_bytes: number | null;
  /** Tierable GGUF bytes in this tier. */
  model_bytes: number;
  files: StorageTierFile[];
}

export interface StorageFileStatus {
  file: string;
  model_id: string | null;
  registered: boolean;
  /** Where the file resolves; null = not tierable. */
  tier: StorageTierName | "missing" | null;
  present_in: StorageTierName[];
  bytes: number;
  /** A verified cold archive copy exists. */
  archived: boolean;
  /** Why the file must stay hot, e.g. "used by enabled backend coder". */
  pinned: string | null;
  moving: StorageMoving | null;
  usage?: { last_started: string | null; first_seen: string | null; recent_starts: number };
}

export interface StoragePlannedMove {
  file: string;
  kind: "promote" | "demote";
  reason: string;
}

export interface StorageMoveRecord {
  id: string;
  logical: string;
  kind: "promote" | "demote";
  from: StorageTierName;
  to: StorageTierName;
  reason: string;
  started_at: string;
  finished_at: string | null;
  outcome: "running" | "done" | "failed" | "cancelled" | "interrupted";
  error: string | null;
  bytes: number;
  instant: boolean;
}

/** A directory holding recorded cold copies that is no longer a
 *  configured tier — usually a config saved by a build that does not
 *  know `model_storage`. The models there look missing until it is
 *  added back. */
export interface StorageLostTier {
  path: string;
  files: number;
  bytes: number;
}

export interface ModelStorageStatus {
  config: ModelStorageConfig;
  tiers: StorageTierStatus[];
  files: StorageFileStatus[];
  planned: StoragePlannedMove[];
  skipped: StoragePlannedMove[];
  moves: StorageMoveRecord[];
  /** Absent from builds before the lost-tier warning. */
  lost_tiers?: StorageLostTier[];
}

/** list_models' per-model `storage` block (null when the model's
 *  path is not tierable). */
export interface ModelStorageRow {
  tier: StorageTierName | "missing";
  path: string;
  present_in: StorageTierName[];
  archived: boolean;
  cold_unavailable: boolean;
  pinned: string | null;
  keep_hot: boolean;
  moving: StorageMoving | null;
  last_started: string | null;
}

/** Result of a promote/demote request: started (task id) or queued. */
export interface StorageMoveStart {
  file: string;
  kind: "promote" | "demote";
  task_id?: string;
  queued?: boolean;
}

/** True for a cold tier's wire name ("cold", "cold:1"). */
export function isColdTier(tier: string | null | undefined): boolean {
  return typeof tier === "string" && tier.startsWith("cold");
}

/** model_storage_status — tiers, every tierable file, moves. */
export async function getModelStorageStatus(): Promise<CallResult<ModelStorageStatus | null>> {
  if (!isTauri()) return { data: mockModelStorageStatus(), live: false };
  try {
    const data = await invoke<ModelStorageStatus>("model_storage_status");
    return { data, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

/** model_storage_set — replace the config's model_storage section
 *  (validated: cold roots absolute, on another volume, and a removed
 *  tier must not still hold models). */
export async function modelStorageSet(storage: ModelStorageConfig): Promise<CallResult<ModelStorageConfig>> {
  if (!isTauri()) return { data: storage, live: false, error: "model_storage_set is not available in sample mode" };
  try {
    const r = await invoke<{ model_storage: ModelStorageConfig }>("model_storage_set", { storage });
    return { data: r.model_storage, live: true };
  } catch (e) {
    return { data: storage, live: true, error: errOf(e) };
  }
}

async function moveOp(cmd: "model_storage_promote" | "model_storage_demote", target: string): Promise<CallResult<StorageMoveStart>> {
  const kind = cmd === "model_storage_promote" ? "promote" : "demote";
  const fallback: StorageMoveStart = { file: target, kind };
  if (!isTauri()) return { data: fallback, live: false, error: `${cmd} is not available in sample mode` };
  try {
    const data = await invoke<StorageMoveStart>(cmd, { target });
    return { data, live: true };
  } catch (e) {
    return { data: fallback, live: true, error: errOf(e) };
  }
}

/** model_storage_promote — copy a cold file to the hot tier (the cold
 *  copy stays as an archive). Started or queued. */
export function modelStoragePromote(target: string): Promise<CallResult<StorageMoveStart>> {
  return moveOp("model_storage_promote", target);
}

/** model_storage_demote — move a hot file to the cold tier (instant
 *  when its archive is intact). Refused for pinned files. */
export function modelStorageDemote(target: string): Promise<CallResult<StorageMoveStart>> {
  return moveOp("model_storage_demote", target);
}

/** model_storage_cancel — cancel the running move for `target`, or
 *  drop it from the queue. */
export async function modelStorageCancel(target: string): Promise<CallResult<{ file: string; cancelled: string | null }>> {
  if (!isTauri()) return { data: { file: target, cancelled: null }, live: false, error: "model_storage_cancel is not available in sample mode" };
  try {
    const data = await invoke<{ file: string; cancelled: string }>("model_storage_cancel", { target });
    return { data, live: true };
  } catch (e) {
    return { data: { file: target, cancelled: null }, live: true, error: errOf(e) };
  }
}

/** model_set_keep_hot — add or remove an operator keep-hot pin. */
export async function modelSetKeepHot(target: string, keep: boolean): Promise<CallResult<{ file: string; keep_hot: boolean }>> {
  if (!isTauri()) return { data: { file: target, keep_hot: !keep }, live: false, error: "model_set_keep_hot is not available in sample mode" };
  try {
    const data = await invoke<{ file: string; keep_hot: boolean }>("model_set_keep_hot", { target, keep });
    return { data, live: true };
  } catch (e) {
    return { data: { file: target, keep_hot: !keep }, live: true, error: errOf(e) };
  }
}
