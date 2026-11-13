import type { OpName } from "../generated/ops";
import { mockOptimizeState } from "../mock";
import type { CacheSelectionLike } from "./backends";
import { errOf, invoke, isTauri } from "./core";
import type { CallResult } from "./core";

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
  /** Predicted throughput for the item's RESULTING
   *  configuration (predicted-throughput design §9), in the
   *  shared measurement shape; present only on flag-changing
   *  items (context/batch, offload, model assignment) whose
   *  destination configuration has an exact measurement. */
  predicted?: PredictedThroughput;
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

/** The readable fingerprint inputs a v2 measurement was taken
 *  under (predicted-throughput design §4.3): the offload as its
 *  resolved label, the raw flags as the normalized string that
 *  was hashed, the GPU set as sorted `index:name` strings. */
export interface MeasurementInputs {
  n_ctx: number;
  n_batch: number;
  offload: string;
  tensor_split: number[];
  split_mode: string;
  raw_flags: string;
  gpus: string[];
}

/** One resolved measurement — the shape shared by the
 *  measurement_lookup exact/near results and the `predicted`
 *  field of a list_backends row (design §7). The headline
 *  figures plus the provenance the surfaces disclose: measured
 *  date, source, app version, driver and llama build (§5.5). */
export interface PredictedThroughput {
  prompt_per_second: number | null;
  predicted_per_second: number | null;
  ttft_ms: number | null;
  measured_at: string;
  /** "benchmark" | "experiment" | "live"; null on v1 entries,
   *  whose provenance was not recorded. */
  source: string | null;
  /** The entry's provenance backend id (a temp `opt-exp-<n>`
   *  id for experiment entries). */
  backend_id: string;
  origin_backend_id: string | null;
  model_id: string;
  model_file: string | null;
  app_version: string;
  driver_version: string[];
  llama_build: string | null;
  inputs: MeasurementInputs | null;
  /** Near matches only (design §5.3): the snapshot fields in
   *  which the entry differs from the looked-up configuration,
   *  named as the surfaces name them. */
  differs?: string[];
}

/** The draft a measurement_lookup request carries (design
 *  §6.2): the form's full draft state. `backend_id` for the
 *  Edit Flags form (merged over the saved backend), `model_id`
 *  for the Add Backend form. `parallel_slots` null is the
 *  field's Auto state; `raw_flags` present is the advanced
 *  editor's text and supersedes the structured fields. */
export interface MeasurementLookupRequest {
  backend_id?: string;
  model_id?: string;
  n_ctx?: number;
  n_batch?: number;
  offload_layers?: number | null;
  mtp_position?: number | null;
  parallel_slots?: number | null;
  gpu_indices?: number[] | null;
  raw_flags?: string | null;
}

export interface MeasurementLookupResult {
  fingerprint: string;
  exact: PredictedThroughput | null;
  near: PredictedThroughput | null;
}

/** measurement_lookup — resolve a flags-form draft to its
 *  exact and nearest near measurement (design §6.2). A pure
 *  read: it never writes configuration. */
export async function measurementLookup(request: MeasurementLookupRequest): Promise<CallResult<MeasurementLookupResult | null>> {
  if (!isTauri()) return { data: null, live: false };
  try {
    const data = await invoke<MeasurementLookupResult>("measurement_lookup", { request });
    return { data, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

/** measurement_delete — delete one stored measurement by its
 *  (fingerprint, measured_at) identity (design §10). */
export async function measurementDelete(fingerprint: string, measuredAt: string): Promise<CallResult<{ deleted: boolean }>> {
  if (!isTauri()) return { data: { deleted: false }, live: false };
  try {
    const data = await invoke<{ deleted: boolean }>("measurement_delete", { fingerprint, measuredAt });
    return { data, live: true };
  } catch (e) {
    return { data: { deleted: false }, live: true, error: errOf(e) };
  }
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
  /** The backend's current measurement fingerprint and its
   *  readable inputs (design §10): the history view names an
   *  entry's differing fields against this configuration. */
  current_fingerprint: string | null;
  current_inputs: MeasurementInputs | null;
}

/** One stored measurement in the Optimize history payload
 *  (design §10): the store entry as persisted (v2 provenance
 *  fields optional — absent on v1 entries), annotated with the
 *  configured backends whose prediction currently resolves to
 *  it. */
export interface OptimizeMeasurementEntry {
  fingerprint: string;
  backend_id: string;
  model_id: string;
  measured_at: string;
  app_version: string;
  short: OptimizeBenchResult;
  long: OptimizeBenchResult;
  source?: string | null;
  origin_backend_id?: string | null;
  model_file?: string | null;
  model_sha256?: string | null;
  model_size_bytes?: number | null;
  inputs?: MeasurementInputs | null;
  driver_version?: string[];
  llama_build?: string | null;
  current_for: string[];
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

/** One deep discovery pass's accounting (deep-discovery
 *  Part 1, §P1.10): every filter drop counted; `partial` with
 *  its reason when the pass stopped early on the deadline or
 *  the rate limit. HF sends no total match count, so the
 *  funnel reports fetched counts only. */
export interface OptimizeDiscoveryFunnel {
  queries_run: number;
  pool_repos: number;
  repos_fetched: number;
  repos_failed: number;
  dropped_unpopular: number;
  dropped_stale: number;
  dropped_family: number;
  dropped_fit: number;
  quants_considered: number;
  quants_collapsed: number;
  kept: number;
  partial: boolean;
  partial_reason: string | null;
  stopped_early: boolean;
}

/** The stored discovery set: the candidates plus the directive
 *  they were gathered for (a consultation only receives them
 *  when its directive matches). */
export interface OptimizeCandidates {
  directive: string;
  retrieved_at: string;
  items: OptimizeCandidate[];
  /** The pass's funnel, or null for a set stored before the
   *  deep pass existed. */
  funnel: OptimizeDiscoveryFunnel | null;
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
  /** Every stored measurement (design §10), in store order;
   *  the history card groups and sorts for display. */
  measurements: OptimizeMeasurementEntry[];
  /** The stored discovery pre-pass set (Phase 5c), or null when
   *  no discovery has run. */
  candidates: OptimizeCandidates | null;
  /** The stored screening-suite baseline (Phase 5d), or null
   *  when no baseline has been run. */
  eval_baseline: OptimizeEvalBaseline | null;
  consulting: boolean;
  /** When the running consultation started (Unix ms), recorded
   *  server-side: the only start time a page instance can trust
   *  when it did not start the run itself (a local clock starts
   *  at 0 and renders the epoch as an elapsed time). */
  consult_started_unix_ms: number | null;
  /** The last consultation's failure, recorded server-side when
   *  a run ends in one and cleared when a new run starts or one
   *  succeeds — so a failure that lands while the operator is on
   *  another view still surfaces here instead of silently
   *  leaving the previous plan looking current. */
  consult_error: string | null;
  consult_error_unix_ms: number | null;
  /** A GPU-bound Optimize job (measurement, later an
   *  experiment batch) is currently running. */
  measuring: boolean;
  /** The running Estimate/Measure job's descriptor (tracker
   *  #69), recorded server-side: any page instance renders the
   *  true job kind, backend, and elapsed from this — the shared
   *  `measuring` boolean alone cannot distinguish an Estimate
   *  from a Measure. Null when no such job is running. */
  job: OptimizeJob | null;
  /** The last Estimate/Measure job's failure, recorded
   *  server-side when a job ends in one and cleared when a new
   *  job starts or one succeeds — so a failure that lands while
   *  the operator is on another view still surfaces here. */
  job_error: string | null;
  job_error_unix_ms: number | null;
  /** Per-backend running Optimize operations (tracker #70):
   *  the mutual-exclusion record — which operation family/kind
   *  holds each backend and when it started — that consultation
   *  and benchmark start refusals name. Empty when none runs. */
  running_ops: Record<string, {
    family: "consultation" | "benchmark";
    kind: string;
    backend_id: string;
    started_unix_ms: number;
  }>;
  /** Per-backend tuning sweeps (Addendum B §B.2): the running
   *  sweep's progress record and the most recent finished
   *  sweep, either possibly null. */
  tuning: { running: TuningSweep | null; latest: TuningSweep | null };
  /** Streaming progress of the running consultation (Optimize
   *  Phase 3): text chunks received (≈ tokens), partial answer
   *  size, time to first token. Null when no consultation is
   *  streaming; absent from older builds and the sample payload. */
  consult_progress?: OptimizeConsultProgress | null;
  /** The Rust apply executor is running (Phase 3). */
  applying?: boolean;
}

export interface OptimizeConsultProgress {
  backend_id: string;
  chunks: number;
  content_bytes: number;
  reasoning_bytes: number;
  elapsed_ms: number;
  first_token_ms: number | null;
  /** At least one chunk has arrived (generation, not prompt
   *  processing). */
  streaming: boolean;
}

/** optimize_apply_plan's `apply_result` (Phase 3 executor). */
export interface OptimizeApplyResult {
  dry_run: boolean;
  destructive: boolean;
  config_changed: boolean;
  steps: { item_id: number; kind: string; target: string | null; operations: string[] }[];
  skipped: { item_id: number; reason: string }[];
  applied?: number[];
  failed?: { item_id: number; error: string } | null;
  not_attempted?: number[];
  rolled_back?: boolean;
}

/** One arm's full configuration (Addendum B §B.1), resolved:
 *  every axis coordinate the sweep varies. */
export interface TuningCoords {
  gpu_indices: number[];
  tensor_split: number[];
  offload_layers: number | null;
  moe_cache_mib: number | null;
  mtp_n_max: number | null;
  n_ctx: number;
  cache: CacheSelectionLike;
  parallel_slots: number;
}

/** One measured (or failed / excluded) arm of a tuning sweep. */
export interface TuningArm {
  index: number;
  axis: string;
  coords: TuningCoords;
  status: "completed" | "failed" | "aborted_thermal" | "unverified" | "excluded";
  detail?: string | null;
  measured?: {
    short: { prompt_per_second: number | null; predicted_per_second: number | null; ttft_ms: number | null };
    long: { prompt_per_second: number | null; predicted_per_second: number | null; ttft_ms: number | null };
  } | null;
  residency?: { verified_exclusive: unknown } | { unverified: { reason: string } } | string | null;
  free_vram_mib: number[];
  temp_start_c?: number | null;
  temp_peak_c?: number | null;
  fingerprint?: string | null;
}

/** One tuning sweep's durable record (Addendum B §B.6). */
export interface TuningSweep {
  id: string;
  backend_id: string;
  model_id: string;
  state: "running" | "completed" | "cancelled" | "failed";
  started_at: string;
  finished_at?: string | null;
  incumbent: TuningCoords;
  stop_list: string[];
  arms: TuningArm[];
  exclusions: { coords: TuningCoords; reason: string }[];
  winner_arm?: number | null;
  incumbent_stands: boolean;
  confirmation?: {
    arm_index: number;
    reproduced: boolean;
    gen_delta_pct?: number | null;
    screen_passes: boolean;
    comparison?: { screen_at_or_above_baseline: boolean } | null;
  } | null;
  truncated?: string | null;
  detail?: string | null;
  adopted: boolean;
  adopted_anyway: boolean;
}

/** The sweep preview (Addendum B §B.2/§B.3). */
export interface TuningPreview {
  backend_id: string;
  model_id: string;
  incumbent: TuningCoords;
  plan: {
    backend_id: string;
    model_id: string;
    axes: [string, string[]][];
    planned_arms: number;
    stop_list: string[];
    exclusions: { coords: TuningCoords; reason: string }[];
    max_arms: number;
    arm_timeout_secs: number;
    sweep_timeout_secs: number;
  };
}

/** One running Estimate/Measure job (tracker #69). */
export interface OptimizeJob {
  kind: "measure" | "estimate";
  backend_id: string;
  started_unix_ms: number;
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

async function optimizeStateCmd<T>(cmd: OpName, args?: Record<string, unknown>): Promise<CallResult<OptimizeState | null>> {
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

/** optimize_tuning_preview — price and plan a per-backend
 *  tuning sweep (Addendum B §B.2/§B.3): axes and value sets,
 *  arm count, stop list, budgets, planning exclusions. Pure
 *  read; nothing is stopped or started. */
export async function optimizeTuningPreview(backendId: string): Promise<CallResult<TuningPreview | null>> {
  if (!isTauri()) return { data: null, live: false };
  try {
    const data = await invoke<TuningPreview>("optimize_tuning_preview", { backendId });
    return { data, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

/** optimize_tuning_run — start a per-backend tuning sweep
 *  (Addendum B): parks the stop list, measures arms as temp
 *  backends under verified residency, ranks them, and runs
 *  the winner's confirmation arm. Long-running (up to the
 *  90-minute sweep budget, enforced server-side); runs under
 *  the shared GPU-job slot, so optimizeCancelJob cancels it.
 *  Returns the refreshed state; progress rides its `tuning`
 *  field. */
export function optimizeTuningRun(backendId: string) {
  return optimizeStateCmd("optimize_tuning_run", { backendId });
}

/** optimize_tuning_adopt — write a completed sweep's winning
 *  coordinates into the backend's saved configuration through
 *  the structured save ops (Addendum B §B.6). Nothing is
 *  auto-applied by the sweep; this is the explicit action.
 *  `adoptAnyway` is the explicit flip required when the
 *  confirmation arm did not reproduce the winner or the
 *  screening gate did not pass. */
export function optimizeTuningAdopt(sweepId: string, adoptAnyway?: boolean) {
  return optimizeStateCmd("optimize_tuning_adopt", { sweepId, adoptAnyway: adoptAnyway ?? null });
}

/** optimize_set_item_outcome — record an apply outcome on one item
 *  (the Phase 4 orchestrator reports progress through this). */
export function optimizeSetItemOutcome(itemId: number, outcome: "applied" | "failed" | "not_attempted", error?: string) {
  return optimizeStateCmd("optimize_set_item_outcome", { itemId, outcome, error: error ?? null });
}

/** optimize_apply_plan — the Rust apply executor (Phase 3):
 *  re-validates, then executes the accepted items of one phase
 *  (non-destructive, or the deletes) through the existing ops in
 *  plan order, halting on the first failure. `dryRun` reports the
 *  operations without executing. Desktop only: in sample mode the
 *  caller keeps its own orchestration (`live: false`). */
export async function optimizeApplyPlan(destructive: boolean, dryRun = false): Promise<CallResult<(OptimizeState & { apply_result: OptimizeApplyResult }) | null>> {
  if (!isTauri()) return { data: null, live: false };
  try {
    const data = await invoke<OptimizeState & { apply_result: OptimizeApplyResult }>("optimize_apply_plan", { destructive, dryRun });
    return { data, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
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
      if (typeof c.moe_cache_mib === "number") parts.push(c.moe_cache_mib === 0 ? "Expert cache: off" : `Expert cache: ${num(c.moe_cache_mib)} MiB`);
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
    case "backend.set_offload": return `Offload: ${num(c.offload_layers)} layers to system RAM${typeof c.moe_cache_mib === "number" ? (c.moe_cache_mib === 0 ? " · Expert cache: off" : ` · Expert cache: ${num(c.moe_cache_mib)} MiB`) : ""}`;
    case "backend.set_placement": return `Split: ${split}${typeof c.split_mode === "string" ? ` · Mode: ${c.split_mode}` : ""}`;
    case "backend.set_model": return `Model: ${shortName(String(c.model_id ?? ""))}`;
    case "backend.set_enabled": return c.enabled === true ? "Enable backend" : "Disable backend";
    case "group.set_members": return `Members: ${Array.isArray(c.members) ? (c.members as string[]).join(", ") : ""}`;
    case "group.set_strategy": return `Strategy: ${String(c.strategy ?? "")}`;
    case "group.set_fallbacks": return `Fallbacks: ${Array.isArray(c.fallbacks) && c.fallbacks.length ? (c.fallbacks as string[]).join(", ") : "none"}`;
    case "group.create": return `Members: ${Array.isArray(c.members) ? (c.members as string[]).join(", ") : ""} · Strategy: ${String(c.strategy ?? "simple-shuffle")}`;
    case "group.delete": return `Group: ${t}`;
    case "model.add": {
      const parts = [`Source: ${String(c.source_url ?? "")}`];
      if (typeof c.params_b === "number") parts.push(`Params: ${num(c.params_b)} B`);
      return parts.join(" · ");
    }
    case "model.download": return "Downloads the GGUF file";
    default: return "";
  }
}
