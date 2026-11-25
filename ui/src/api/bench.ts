import { errOf, invoke, isTauri } from "./core";
import type { CallResult } from "./core";

/* ---------------- Bench (docs: llmdil-bench-design §5.2, §7) ----------------
   Wire shapes mirror manager-bench's serde (snake_case) types.
   Argument keys are the snake_case keys the registry generates. */

export type BenchPromptOrigin = "shipped" | "user" | "generated";

export interface BenchPromptEntry {
  id: string;
  name: string;
  purpose: string;
  body: string;
  sha256: string;
  origin: BenchPromptOrigin;
  version: number;
  updated_ms: number;
}

/** One arm variant: every field optional; absent = held at the
 *  backend's configured value. Enum strings: cache "f16"|"q8_0"|"q4_0";
 *  flash "auto"|"on"|"off"; spec "none"|"ngram-mod"|"draft-simple"|"combined";
 *  draft_device "target_gpu"|"cpu". */
export interface BenchArmVariant {
  cache_k?: string | null;
  cache_v?: string | null;
  flash_attn?: string | null;
  depth?: number | null;
  n_ctx?: number | null;
  spec?: string | null;
  ngram_n_min?: number | null;
  draft_model_id?: string | null;
  draft_n_max?: number | null;
  draft_device?: string | null;
  offload_expr?: string | null;
}

/** A resolved arm's full spec as stored on its record: the variant
 *  fields plus any server-resolved extras. */
export interface BenchArmSpec extends BenchArmVariant {
  [key: string]: unknown;
}

export interface BenchSweepRequest {
  backend_id: string;
  prompt_id: string;
  temperature: number;
  correctness_control: boolean;
  stop_backend_ids: string[];
  preset: "kv" | "spec" | "combined" | null;
  dimensions: BenchArmVariant[][] | null;
  draft_model_id: string | null;
  baseline_arm_index: number;
  /** Second confirmation pass at temperature 0.6 (Run tab checkbox). */
  confirm_at_06?: boolean;
}

export interface BenchDrafterRow {
  kind: string;
  gen_tokens: number;
  acc_tokens: number;
}

export interface BenchRunRow {
  warmup: boolean;
  prompt_tps: number | null;
  gen_tps: number | null;
  ttft_ms: number | null;
  prompt_tokens: number | null;
  generated_tokens: number | null;
  acceptance_rate: number | null;
  accepted: number | null;
  generated: number | null;
  mean_accepted_len: number | null;
  drafters: BenchDrafterRow[];
}

export interface BenchArmSummary {
  median_prompt_tps: number | null;
  median_gen_tps: number | null;
  median_ttft_ms: number | null;
  mean_acceptance_rate: number | null;
  mean_accepted_len: number | null;
}

export type BenchArmStatus = "pending" | "running" | "measured" | "pruned" | "preflight_failed" | "failed" | "cancelled";

export interface BenchArmRecord {
  index: number;
  label: string;
  spec: BenchArmSpec;
  raw_flags: string | null;
  comparability_key: string;
  status: BenchArmStatus;
  detail: string | null;
  runs: BenchRunRow[];
  summary: BenchArmSummary | null;
  peak_vram_mib: number | null;
  started_ms: number | null;
  ended_ms: number | null;
}

export interface BenchSweepProgress {
  arm_index: number;
  arm_count: number;
  phase: string;
  run: number;
  elapsed_ms: number;
}

export type BenchSweepStatus = "running" | "complete" | "partial" | "failed";

export interface BenchSweepRecord {
  id: string;
  status: BenchSweepStatus;
  started_ms: number;
  ended_ms: number | null;
  app_version: string;
  llama_build: string;
  machine_gpus: string[];
  backend_id: string;
  backend_name: string;
  model: { path: string; size_bytes: number; sha256: string };
  prompt_id: string;
  prompt_name: string;
  prompt_sha256: string;
  prompt_tokens: number | null;
  temperature: number;
  correctness_control: boolean;
  interlock: { running_before: string[]; stopped_confirmed: string[] };
  baseline_arm_index: number;
  projected_minutes: number | null;
  partial: boolean;
  arms: BenchArmRecord[];
  progress?: BenchSweepProgress | null;
}

/** bench_prompts_list — the prompt library, bodies included. */
export async function benchPromptsList(): Promise<CallResult<{ prompts: BenchPromptEntry[] }>> {
  if (!isTauri()) return { data: { prompts: [] }, live: false };
  try {
    const data = await invoke<{ prompts: BenchPromptEntry[] }>("bench_prompts_list");
    return { data: { prompts: Array.isArray(data.prompts) ? data.prompts : [] }, live: true };
  } catch (e) {
    return { data: { prompts: [] }, live: true, error: errOf(e) };
  }
}

/** bench_prompt_save — create/update one entry; the server recomputes
 *  sha256/updated_ms and returns the full library. */
export async function benchPromptSave(entry: BenchPromptEntry): Promise<CallResult<{ prompts: BenchPromptEntry[] }>> {
  if (!isTauri()) return { data: { prompts: [] }, live: false };
  try {
    const data = await invoke<{ prompts: BenchPromptEntry[] }>("bench_prompt_save", { entry });
    return { data: { prompts: Array.isArray(data.prompts) ? data.prompts : [] }, live: true };
  } catch (e) {
    return { data: { prompts: [] }, live: true, error: errOf(e) };
  }
}

/** bench_prompt_delete — delete one entry; returns the full library. */
export async function benchPromptDelete(id: string): Promise<CallResult<{ prompts: BenchPromptEntry[] }>> {
  if (!isTauri()) return { data: { prompts: [] }, live: false };
  try {
    const data = await invoke<{ prompts: BenchPromptEntry[] }>("bench_prompt_delete", { id });
    return { data: { prompts: Array.isArray(data.prompts) ? data.prompts : [] }, live: true };
  } catch (e) {
    return { data: { prompts: [] }, live: true, error: errOf(e) };
  }
}

/** bench_prompt_generate — draft a prompt with a running backend
 *  (§4.3). Long-running; bench_prompt_generate_cancel fires its
 *  scoped cancellation handle. The draft is returned unsaved. */
export async function benchPromptGenerate(purpose: string, description: string, size: "short" | "standard", backendId?: string): Promise<CallResult<{ text: string; backend_id: string; model: string }>> {
  if (!isTauri()) return { data: { text: "", backend_id: "", model: "" }, live: false };
  try {
    const data = await invoke<{ text: string; backend_id: string; model: string }>("bench_prompt_generate", { purpose, description, size, backendId: backendId ?? null });
    return { data, live: true };
  } catch (e) {
    return { data: { text: "", backend_id: "", model: "" }, live: true, error: errOf(e) };
  }
}

/** bench_prompt_generate_cancel — cancel an in-flight draft generation. */
export async function benchPromptGenerateCancel(): Promise<CallResult<Record<string, never>>> {
  if (!isTauri()) return { data: {}, live: false };
  try {
    const data = await invoke<Record<string, never>>("bench_prompt_generate_cancel");
    return { data: data ?? {}, live: true };
  } catch (e) {
    return { data: {}, live: true, error: errOf(e) };
  }
}

/** bench_run_start — validate, expand, and run a sweep. The call may
 *  stay open for the whole sweep (progress comes from
 *  bench_results_list meanwhile), so a caller polls from the moment it
 *  calls; `sweep_id` is absent when the payload does not name one. An
 *  interlock failure rejects with a message listing the conflicting
 *  running backends; the caller parses it for the stop-and-run
 *  confirmation (nothing is stopped without that explicit action). */
export async function benchRunStart(request: BenchSweepRequest): Promise<CallResult<{ sweep_id?: string }>> {
  if (!isTauri()) return { data: {}, live: false };
  try {
    const data = await invoke<{ sweep_id?: string } | null>("bench_run_start", { request });
    return { data: data ?? {}, live: true };
  } catch (e) {
    return { data: {}, live: true, error: errOf(e) };
  }
}

/** bench_run_cancel — real cancellation via the scoped handle (D13). */
export async function benchRunCancel(sweepId: string): Promise<CallResult<Record<string, never>>> {
  if (!isTauri()) return { data: {}, live: false };
  try {
    const data = await invoke<Record<string, never>>("bench_run_cancel", { sweepId });
    return { data: data ?? {}, live: true };
  } catch (e) {
    return { data: {}, live: true, error: errOf(e) };
  }
}

/** bench_results_list — every stored sweep, including the in-progress
 *  one with per-arm status and progress. */
export async function benchResultsList(): Promise<CallResult<{ sweeps: BenchSweepRecord[] }>> {
  if (!isTauri()) return { data: { sweeps: [] }, live: false };
  try {
    const data = await invoke<{ sweeps: BenchSweepRecord[] }>("bench_results_list");
    return { data: { sweeps: Array.isArray(data.sweeps) ? data.sweeps : [] }, live: true };
  } catch (e) {
    return { data: { sweeps: [] }, live: true, error: errOf(e) };
  }
}

/** bench_result_delete — delete one sweep record; returns the list. */
export async function benchResultDelete(sweepId: string): Promise<CallResult<{ sweeps: BenchSweepRecord[] }>> {
  if (!isTauri()) return { data: { sweeps: [] }, live: false };
  try {
    const data = await invoke<{ sweeps: BenchSweepRecord[] }>("bench_result_delete", { sweepId });
    return { data: { sweeps: Array.isArray(data.sweeps) ? data.sweeps : [] }, live: true };
  } catch (e) {
    return { data: { sweeps: [] }, live: true, error: errOf(e) };
  }
}
