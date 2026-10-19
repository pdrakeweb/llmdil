/* Optimize view (docs/optimize-design.md §1).
   renderOptimize() returns the shell; wireOptimize(root) loads the
   page state from optimize_get_state, paints, and attaches handlers.
   The page never invents data: every figure on a plan row comes
   from the item's normalized change payload or the Rust-side
   validation (expected effects are app-computed, §1.3).

   Apply (Phase 4) is orchestrated here through the existing
   mutation commands only: non-destructive accepted items in plan
   order, then destructive items behind their own confirm —
   halt on first failure, outcomes recorded per item through
   optimize_set_item_outcome. Apply saves configuration; it never
   restarts anything — activation stays with the banner /
   Activation Plan flow (§5.3). */

import {
  addBackend, applyBackendConfig, deleteBackend, downloadModel, getAppConfig, getBackends,
  getBackendConfig, getModels, getOptimizeState, modelAdd, modelAssign,
  optimizeCancelConsult, optimizeCancelJob, optimizeChangeSummary, optimizeConsult,
  optimizeDiscardPlan, optimizeDiscover, optimizeEstimateBackend, optimizeEstimateExperiments, optimizeIsDestructive, optimizeKindLabel,
  optimizeMeasureBackend, optimizeRunBaseline, optimizeRunExperiments, optimizeSavePurpose, optimizeSetExperimentState, optimizeSetItemOutcome, optimizeSetItemState,
  setBackendOffload, stopBackend, updateBackendFlags, updateGatewayConfig,
} from "../api";
import type {
  CallResult, OptimizeCandidate, OptimizeExperiment, OptimizePlanItem, OptimizeState,
} from "../api";
import { trim1 } from "../charts";
import { esc } from "../html";
import { icon } from "../icons";

export function renderOptimize(): string {
  return `<div class="topbar"><h1>Optimize</h1><span class="spacer"></span><a class="btn icon-only" href="#/logs?scope=optimize&follow=1" title="Logs" aria-label="Optimize logs">${icon("logs")}</a></div>
  <div class="view"><div id="optRoot"><div class="card" style="color:var(--text2)">${icon("clock")} Loading…</div></div></div>`;
}

const SELECT_STYLE = "background:var(--panel);color:var(--text);border:1px solid var(--border);border-radius:8px;padding:6px 8px;font-size:12px";

function fmtGiB(mib: number): string {
  return `${trim1(mib / 1024)} GiB`;
}

function fmtTime(ts: string): string {
  const d = new Date(ts);
  return Number.isNaN(d.getTime()) ? ts : d.toLocaleString("en-GB", { dateStyle: "medium", timeStyle: "short" });
}

function fmtElapsed(secs: number): string {
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  return m > 0 ? `${m} min ${s} s` : `${s} s`;
}

function fmtAge(secs: number): string {
  if (secs < 60) return `${secs} s`;
  if (secs < 3600) return `${Math.floor(secs / 60)} min`;
  if (secs < 86400) return `${Math.floor(secs / 3600)} h`;
  return `${Math.floor(secs / 86400)} d`;
}

/** Unwrap a CallResult or throw its error as an Error. */
function unwrap<T>(r: CallResult<T>): T {
  if (r.error) throw new Error(r.error);
  return r.data;
}

interface CfgBackend {
  id: string;
  enabled: boolean;
  model_file: string;
  port: number;
  flags: { n_ctx: number; n_batch: number; tensor_split: number[]; split_mode: string };
  group?: string | null;
}
interface CfgGroup { name: string; members: string[]; strategy: string; fallbacks: string[]; purpose?: string }
interface CfgModel { id: string; gguf_path: string }

export async function wireOptimize(root: HTMLElement): Promise<() => void> {
  const host = root.querySelector<HTMLElement>("#optRoot");
  if (!host) return () => {};
  const hostEl: HTMLElement = host;

  let state: OptimizeState | null = null;
  let loadError: string | null = null;
  let disposed = false;
  let elapsedTimer: number | null = null;
  let consultStart = 0;
  /** Armed two-step confirms, by key ("consult", "discard", "destructive"). */
  const armed = new Set<string>();
  /** Purpose editor drafts, keyed "deployment" | "backend:<id>" | "group:<name>". */
  const drafts = new Map<string, string>();
  /** Directive field draft (addendum A.2). Null until the operator
   *  edits the field; the field's text otherwise comes from the
   *  stored plan's directive (`state.last_directive`). Editing it
   *  never touches the persisted purpose fields. */
  let directiveDraft: string | null = null;
  const openEditors = new Set<string>();
  let statusLine = "";
  /** The last consultation's failure as this page knows it:
   *  set from the run's own error return, and synced from the
   *  server's recorded `consult_error` on every refresh — a run
   *  that failed while the operator was on another view must
   *  still surface, with the stored plan marked as previous. */
  let lastConsultError = "";
  let applyRunning = false;
  /** The backend this page instance is measuring (Phase 5a), and
   *  its progress timer — the server payload carries only the
   *  shared job slot's occupied flag (`measuring`). */
  let measuringBackend: string | null = null;
  let measureStart = 0;
  let measureTimer: number | null = null;
  /** Estimate (tracker #56): the backend this page instance is
   *  estimating, and its progress timer. The estimate shares
   *  the GPU-job slot with measurements, so the server
   *  payload's `measuring` flag covers it too. */
  let estimatingBackend: string | null = null;
  let estimateStart = 0;
  let estimateTimer: number | null = null;
  /** Discovery pre-pass (Phase 5c): a run is in flight, and the
   *  candidate entries the operator has pruned from the stored
   *  set (keys "repo_id\nfile"). Pruning is local until consult:
   *  the consult invoke passes the remaining entries' repo ids
   *  as candidateRepos and the server intersects. */
  let discovering = false;
  /** Screening suite (Phase 5d): the backend this page instance
   *  is running a baseline for, and its progress timer. The run
   *  shares the GPU-job slot with measurements, so the server
   *  payload's `measuring` flag covers it too. */
  let evalBackend: string | null = null;
  let evalStart = 0;
  let evalTimer: number | null = null;
  /** Experiment batch (Phase 5e): a run is in flight from this
   *  page instance, and its progress timer. The batch shares
   *  the GPU-job slot, so the server payload's `measuring`
   *  flag covers it too. */
  let experimentsRunning = false;
  let expStart = 0;
  let expTimer: number | null = null;
  /** Experiment estimate batch (tracker #56): an estimate
   *  pass is in flight from this page instance, and its
   *  progress timer. Shares the GPU-job slot like the batch. */
  let estimatesRunning = false;
  let expEstStart = 0;
  let expEstTimer: number | null = null;
  const candidateRemoved = new Set<string>();
  const candidateKey = (c: OptimizeCandidate): string => `${c.repo_id}\n${c.file}`;

  const shortName = (modelId: string): string => {
    if (!state) return modelId;
    const b = state.backends.find((x) => x.model_id === modelId);
    return b?.short_name ?? modelId;
  };

  function expectedLine(item: OptimizePlanItem, st: OptimizeState): string {
    const v = st.plan?.validations[item.id - 1];
    if (!v) return "";
    if (v.status === "invalid") {
      return `<span class="opt-invalid">${icon("err")} Invalid — ${esc(v.reason ?? "")}</span>`;
    }
    const parts: string[] = [];
    if (v.expected.gpu_ram_mib) {
      parts.push(`GPU RAM: ${fmtGiB(v.expected.gpu_ram_mib[0])} → ${fmtGiB(v.expected.gpu_ram_mib[1])}`);
    }
    if (v.expected.sys_ram_mib) {
      parts.push(`System RAM: ${fmtGiB(v.expected.sys_ram_mib[0])} → ${fmtGiB(v.expected.sys_ram_mib[1])}`);
    }
    if (v.expected.note) parts.push(esc(v.expected.note));
    let html = parts.length ? `<span class="opt-expected">${parts.join(" · ")}</span>` : "";
    if (v.status === "warning") {
      html += `<span class="opt-warning">${icon("warn")} ${esc(v.reason ?? "")}</span>`;
    }
    return html;
  }

  function outcomeChip(item: OptimizePlanItem): string {
    if (item.apply_outcome === "applied") {
      return `<span class="opt-outcome ok">${icon("check")} Applied</span>`;
    }
    if (item.apply_outcome === "failed") {
      return `<span class="opt-outcome err">${icon("err")} Failed — ${esc(item.apply_error ?? "")}</span>`;
    }
    if (item.apply_outcome === "not_attempted") {
      return `<span class="opt-outcome idle">Not attempted</span>`;
    }
    return "";
  }

  function triState(item: OptimizePlanItem): string {
    if (item.kind === "advice") return "";
    const btn = (d: string, label: string) =>
      `<button class="opt-tri-btn ${item.decision === d ? `on ${d}` : ""}" data-opt-tri="${item.id}:${d}">${label}</button>`;
    return `<span class="opt-tri" role="group" aria-label="Decision for item ${item.id}">
      ${btn("accepted", "Accept")}${btn("undecided", "Undecided")}${btn("denied", "Deny")}
    </span>${item.previously_denied ? `<span class="opt-prev-denied">Denied last time</span>` : ""}`;
  }

  function planRow(item: OptimizePlanItem, st: OptimizeState): string {
    const v = st.plan?.validations[item.id - 1];
    const invalid = v?.status === "invalid";
    if (item.kind === "advice") {
      return `<div class="opt-row advice">
        <div class="opt-row-main">
          <span class="opt-num">${item.id}.</span>
          <span class="chip">${optimizeKindLabel(item.kind)}</span>
          <span class="opt-advice">${esc(item.advice_text ?? "")}</span>
        </div>
      </div>`;
    }
    const targetLabel = item.kind.startsWith("group.")
      ? `Group: ${esc(item.target ?? "")}`
      : item.kind === "model.add" || item.kind === "model.download"
        ? `Model: ${esc(item.target ?? "")}`
        : `${esc(item.target ?? "")}${item.target && shortNameForBackend(item.target, st) ? ` · ${esc(shortNameForBackend(item.target, st) ?? "")}` : ""}`;
    return `<div class="opt-row ${invalid ? "invalid" : ""}">
      <div class="opt-row-main">
        <span class="opt-num">${item.id}.</span>
        <span class="chip">${optimizeKindLabel(item.kind)}</span>
        <span class="opt-target">${targetLabel}</span>
        <span class="opt-change">${esc(optimizeChangeSummary(item, shortName))}</span>
        ${expectedLine(item, st)}
        <span class="opt-conf">Confidence: ${esc(item.confidence)}</span>
        ${triState(item)}
        ${outcomeChip(item)}
      </div>
      ${optimizeIsDestructive(item.kind) && item.kind === "backend.delete"
        ? `<div class="opt-note">Applying this item stops the backend first, then deletes it.</div>` : ""}
      ${item.provision_for_experiment
        ? `<div class="opt-note">Required by experiment E${item.provision_for_experiment} — accepting that experiment accepts this step; denying it denies this step.</div>` : ""}
      ${item.rationale || item.expected_effect
        ? `<details class="opt-why"><summary>Why</summary>
             ${item.rationale ? `<p>${esc(item.rationale)}</p>` : ""}
             ${item.expected_effect ? `<p><em>Model's expected effect:</em> ${esc(item.expected_effect)}</p>` : ""}
           </details>` : ""}
    </div>`;
  }

  function shortNameForBackend(backendId: string, st: OptimizeState): string | null {
    return st.backends.find((b) => b.id === backendId)?.short_name ?? null;
  }

  function expTriState(e: OptimizeExperiment): string {
    const btn = (d: string, label: string) =>
      `<button class="opt-tri-btn ${e.decision === d ? `on ${d}` : ""}" data-opt-exp-tri="${e.id}:${d}">${label}</button>`;
    return `<span class="opt-tri" role="group" aria-label="Decision for experiment ${e.id}">
      ${btn("accepted", "Accept")}${btn("undecided", "Undecided")}${btn("denied", "Deny")}
    </span>${e.previously_denied ? `<span class="opt-prev-denied">Denied last time</span>` : ""}`;
  }

  function expFlagsSummary(e: OptimizeExperiment): string {
    const parts: string[] = [];
    if (typeof e.flags.n_ctx === "number") parts.push(`Context: ${e.flags.n_ctx.toLocaleString("en-US")}`);
    if (typeof e.flags.offload_layers === "number") parts.push(`Offload: ${e.flags.offload_layers} layers`);
    if (Array.isArray(e.flags.tensor_split) && e.flags.tensor_split.length) {
      parts.push(`Split: ${e.flags.tensor_split.map((f) => `${Math.round(f * 100)}`).join(" / ")} %`);
    }
    return parts.length ? parts.join(" · ") : "Default flags";
  }

  /** The experiments section (Phase 5e, §A.3): proposal rows
   *  with tri-state controls under the plan items, the Run
   *  experiments action, and — once results exist — the §A.4
   *  results table with the verbatim screening caveat and the
   *  Consult-with-results action. Rendered only when the plan
   *  carries experiments. */
  function experimentsHtml(st: OptimizeState): string {
    const plan = st.plan;
    const exps = plan?.experiments ?? [];
    if (!plan || !exps.length) return "";
    const busy = st.measuring || experimentsRunning || estimatesRunning;
    const num = (x: number | null | undefined): string => x == null ? "—" : trim1(x);
    const accepted = exps.filter((e) => e.decision === "accepted").length;
    const withResults = exps.filter((e) => e.result);
    const notes: string[] = [];
    if (plan.experiments_dropped_invalid) notes.push(`${plan.experiments_dropped_invalid} experiment${plan.experiments_dropped_invalid === 1 ? " was" : "s were"} dropped as invalid`);
    if (plan.experiments_truncated) notes.push(`${plan.experiments_truncated} experiment${plan.experiments_truncated === 1 ? "" : "s"} cut by the 3-experiment cap`);
    const rows = exps.map((e) => {
      const v = plan.experiment_validations?.[e.id - 1];
      const invalid = v?.status === "invalid";
      return `<div class="opt-row ${invalid ? "invalid" : ""}">
      <div class="opt-row-main">
        <span class="opt-num">E${e.id}.</span>
        <span class="chip">Experiment</span>
        <span class="opt-target">Candidate: ${esc(e.candidate_model_id)}</span>
        <span class="opt-change">${esc(expFlagsSummary(e))}</span>
        ${invalid ? `<span class="opt-invalid">${icon("err")} Invalid — ${esc(v?.reason ?? "")}</span>` : ""}
        ${expTriState(e)}
      </div>
      <div class="opt-note">Serves: ${esc(e.serves)}</div>
      ${e.provisioning ? `<div class="opt-note">Provisioning: plan item${e.provisioning.item_ids.length === 1 ? "" : "s"} ${e.provisioning.item_ids.join(", ")} register and download '${esc(e.provisioning.model_id)}' — accepting this experiment accepts those items, and the experiment can run once they are applied.</div>` : ""}
      ${e.requires_stopping.length ? `<div class="opt-note">Requires stopping: ${esc(e.requires_stopping.join(", "))} — this proposal starts denied; accepting it allows the run to stop ${e.requires_stopping.length === 1 ? "that backend" : "those backends"} (restarted after the batch).</div>` : ""}
      ${e.predicted.predicted_per_second != null || e.predicted.fit ? `<div class="opt-note">Predicted: ${e.predicted.predicted_per_second != null ? `${trim1(e.predicted.predicted_per_second)} tok/s` : ""}${e.predicted.fit ? ` · Fit: ${esc(e.predicted.fit)}` : ""}</div>` : ""}
      ${e.hypothesis ? `<details class="opt-why"><summary>Why</summary><p>${esc(e.hypothesis)}</p></details>` : ""}
      ${e.result ? `<div class="opt-note">Result: ${esc(e.result.status)} — ${esc(e.result.detail)}</div>
      ${e.result.preflight ? `<div class="opt-note">Preflight: ${esc(e.result.preflight)}</div>` : ""}` : ""}
      ${e.estimate ? `<div class="opt-note">Estimate: Generation tok/s (short/long): ${num(e.estimate.short.predicted_per_second)} / ${num(e.estimate.long.predicted_per_second)} · Prompt tok/s: ${num(e.estimate.long.prompt_per_second ?? e.estimate.short.prompt_per_second)} · TTFT: ${e.estimate.short.ttft_ms == null && e.estimate.long.ttft_ms == null ? "—" : `${num(e.estimate.short.ttft_ms ?? e.estimate.long.ttft_ms)} ms`}${Number.isFinite((Date.now() - new Date(e.estimate.estimated_at).getTime()) / 1000) ? ` · Age: ${fmtAge(Math.max(0, Math.floor((Date.now() - new Date(e.estimate.estimated_at).getTime()) / 1000)))}` : ""}</div>
      <div class="opt-note">${esc(e.estimate.detail)}</div>
      ${e.estimate.preflight ? `<div class="opt-note">Preflight: ${esc(e.estimate.preflight)}</div>` : ""}` : ""}
    </div>`;
    }).join("");
    const resultsTable = withResults.length ? `<div class="opt-exp-results">
      <div class="opt-field-label" style="margin-top:10px">Experiment results</div>
      ${withResults.map((e) => {
        const r = e.result!;
        const m = r.measured;
        const genShort = m?.short.predicted_per_second;
        const genLong = m?.long.predicted_per_second;
        const prompt = m?.long.prompt_per_second ?? m?.short.prompt_per_second;
        const ttft = m?.short.ttft_ms ?? m?.long.ttft_ms;
        const num = (x: number | null | undefined): string => x == null ? "—" : trim1(x);
        const cats = r.eval ? Object.entries(r.eval.scores.per_category).map(([k, s]) => optKv(evalCategoryLabel(k), `${s.passed}/${s.total}`)).join("") : "";
        const delta = r.comparison
          ? `${r.comparison.overall.delta_passed >= 0 ? "+" : ""}${r.comparison.overall.delta_passed} passed · Screen at/above baseline: ${r.comparison.screen_at_or_above_baseline ? "yes" : "no"}${r.comparison.any_category_zero_that_baseline_passes ? ` · Categories at zero the baseline passes: ${r.comparison.zero_categories_that_baseline_passes.map(evalCategoryLabel).join(", ")}` : ""}`
          : "No baseline comparison";
        return `<div class="opt-measure-card">
          <div class="opt-measure-head">
            <span class="opt-target">${esc(e.candidate_model_id)}</span>
            <span class="opt-measure-state">${esc(r.status)}</span>
          </div>
          <div class="opt-measure-figs-block">
            ${optKv("Generation (short)", `${num(genShort)} tok/s`)}
            ${optKv("Generation (long)", `${num(genLong)} tok/s`)}
            ${optKv("Prompt", `${num(prompt)} tok/s`)}
            ${optKv("TTFT", ttft == null ? "—" : `${trim1(ttft)} ms`)}
            ${optKv("Screen overall", r.eval ? `${r.eval.scores.overall.passed}/${r.eval.scores.overall.total}` : "not run")}
            ${cats}
            ${optKv("Delta vs baseline", delta)}
          </div>
        </div>`;
      }).join("")}
      ${withResults.some((e) => e.result?.eval) ? `<div class="opt-note">${esc(SCREENING_CAVEAT)}</div>` : ""}
      <div class="opt-consult-actions">
        <button class="btn" data-opt-act="consult-results">${icon("target")} Consult with results</button>
        <span class="opt-note">A follow-up consultation carrying these measurements and scores, under the plan's directive.</span>
      </div>
    </div>` : "";
    return `<div class="opt-experiments">
      <div class="opt-field-label" style="margin-top:14px">Experiments</div>
      <div class="opt-counts">${exps.length} experiment${exps.length === 1 ? "" : "s"} · ${accepted} accepted${notes.length ? ` · ${notes.join(" · ")}` : ""}</div>
      <div class="opt-rows">${rows}</div>
      <div class="opt-consult-actions">
        ${experimentsRunning
          ? `<span class="opt-consulting">${icon("clock")} Running experiments <span id="optExpElapsed">${fmtElapsed(Math.floor((Date.now() - expStart) / 1000))}</span> — one at a time, up to 30 min each, 60 min per batch. Temp backends are cleaned up on every outcome.</span>
             <button class="btn danger" data-opt-act="cancel-job">${icon("x")} Cancel</button>`
          : estimatesRunning
            ? `<span class="opt-consulting">${icon("clock")} Estimating experiments <span id="optExpEstElapsed">${fmtElapsed(Math.floor((Date.now() - expEstStart) / 1000))}</span> — the abbreviated benchmark only, one at a time, same 60 min batch budget. Temp backends are cleaned up on every outcome.</span>
               <button class="btn danger" data-opt-act="cancel-job">${icon("x")} Cancel</button>`
            : `<button class="btn primary" data-opt-act="run-experiments" ${accepted && !busy ? "" : "disabled"}>${icon("bolt")} Run experiments (${accepted})</button>
               <button class="btn" data-opt-act="estimate-experiments" ${accepted && !busy ? "" : "disabled"}>${icon("gauge")} Estimate experiments (${accepted})</button>
               <span class="opt-note">Accepted experiments run one at a time on a temporary backend outside every gateway group; it is stopped and removed afterwards, on every outcome. Estimate runs the abbreviated benchmark only, to screen a batch before full runs.</span>`}
      </div>
      ${resultsTable}
    </div>`;
  }

  function planHtml(st: OptimizeState): string {
    const plan = st.plan;
    if (!plan) {
      return `<div class="card"><div class="empty">No plan yet — run a consultation.</div></div>`;
    }
    const accepted = plan.items.filter((i) => i.decision === "accepted");
    const denied = plan.items.filter((i) => i.decision === "denied").length;
    const undecided = plan.items.filter((i) => i.decision === "undecided").length;
    const eligible = (i: OptimizePlanItem) =>
      i.decision === "accepted"
      && plan.validations[i.id - 1]?.status !== "invalid"
      && i.apply_outcome !== "applied";
    const nonDestructive = accepted.filter((i) => eligible(i) && !optimizeIsDestructive(i.kind));
    const destructive = accepted.filter((i) => eligible(i) && optimizeIsDestructive(i.kind));
    const notes: string[] = [];
    if (plan.dropped_invalid > 0) notes.push(`${plan.dropped_invalid} item${plan.dropped_invalid === 1 ? " was" : "s were"} dropped as invalid`);
    if (plan.excluded_items?.length) notes.push(`${plan.excluded_items.length} item${plan.excluded_items.length === 1 ? " was" : "s were"} excluded at plan build — see summary`);
    if (plan.truncated > 0) notes.push(`${plan.truncated} item${plan.truncated === 1 ? "" : "s"} cut by the 25-item cap`);
    return `<div class="card">
      <div class="opt-plan-head">
        <strong>${lastConsultError || st.consult_error ? "Previous plan" : "Plan"}</strong>
        <span class="opt-plan-meta">from ${esc(plan.consulted_backend)} · ${esc(fmtTime(plan.created_at))}</span>
        ${plan.directive?.trim() ? `<span class="opt-plan-meta">Directive: ${esc(plan.directive)}</span>` : ""}
        ${plan.sample ? `<span class="chip">Sample plan</span>` : ""}
        ${plan.config_stale ? `<span class="stale-tag">Configuration changed since this plan</span>
          <button class="btn" data-opt-act="revalidate">${icon("restart")} Re-validate</button>` : ""}
        <span class="grow"></span>
        <button class="btn icon-only" data-opt-act="discard" title="Discard plan">${icon("x")}</button>
      </div>
      <details class="opt-why"><summary>Summary and raw response</summary>
        ${plan.summary ? `<p>${esc(plan.summary)}</p>` : ""}
        <pre class="opt-pre">${esc(plan.raw_response)}</pre>
        ${plan.dropped_fragments.length
          ? `<p class="opt-note">Dropped items:</p>${plan.dropped_fragments.map((f) => `<pre class="opt-pre">${esc(f)}</pre>`).join("")}` : ""}
      </details>
      <div class="opt-counts">${plan.items.length} items · ${accepted.length} accepted · ${denied} denied · ${undecided} undecided${notes.length ? ` · ${notes.join(" · ")}` : ""}</div>
      <div class="opt-rows">${plan.items.map((i) => planRow(i, st)).join("")}</div>
      ${experimentsHtml(st)}
      ${nonDestructive.length || destructive.length ? `<div class="opt-applybar">
        ${nonDestructive.length ? `<button class="btn primary" data-opt-act="apply" ${applyRunning ? "disabled" : ""}>${icon("check")} Apply accepted (${nonDestructive.length})</button>` : ""}
        ${destructive.length ? `<button class="btn danger ${armed.has("destructive") ? "armed" : ""}" data-opt-act="apply-destructive" ${applyRunning ? "disabled" : ""}>${icon("trash")} ${armed.has("destructive") ? `Confirm deletes (${destructive.length})` : `Apply destructive (${destructive.length})`}</button>` : ""}
        <span class="opt-note">Apply saves the configuration only — nothing restarts. Activation stays with the banner / Activation Plan flow.</span>
      </div>` : ""}
      ${statusLine ? `<div class="opt-status">${esc(statusLine)}</div>` : ""}
    </div>`;
  }

  function purposeEditor(key: string, label: string, current: string, isSeed = false): string {
    const open = openEditors.has(key);
    const draft = drafts.get(key) ?? current;
    if (!open) {
      return `<div class="opt-purpose-row">
        <span class="opt-purpose-label">${esc(label)}</span>
        <span class="opt-purpose-text">${current.trim() ? esc(current) : `<em>No purpose set${isSeed ? " — a generated description is used" : ""}</em>`}</span>
        <button class="btn" data-opt-purpose-edit="${esc(key)}">Edit</button>
      </div>`;
    }
    return `<div class="opt-purpose-row edit">
      <span class="opt-purpose-label">${esc(label)}</span>
      <textarea class="opt-purpose-input" data-opt-purpose-input="${esc(key)}" rows="3">${esc(draft)}</textarea>
      <button class="btn primary" data-opt-purpose-save="${esc(key)}">Save purpose</button>
      <button class="btn" data-opt-purpose-cancel="${esc(key)}">Cancel</button>
    </div>`;
  }

  /** One labeled figure row inside a measure card (design
   *  rule 9, Label: Value — figures are never joined into a
   *  running sentence). */
  function optKv(label: string, value: string): string {
    return `<div class="opt-kv"><span class="opt-kv-label">${esc(label)}:</span> <span class="opt-kv-value">${esc(value)}</span></div>`;
  }

  /** The measured/estimated figures of one backend as labeled
   *  rows. Null figures render nothing (rules 12–13: unknown is
   *  not zero, and an empty block does not render). */
  function measureFigRows(f: OptimizeState["backends"][number]["measured"]): string {
    if (!f) return "";
    return [
      f.prompt_per_second !== null ? optKv("Prompt", `${trim1(f.prompt_per_second)} tok/s`) : "",
      f.predicted_per_second !== null ? optKv("Generation", `${trim1(f.predicted_per_second)} tok/s`) : "",
      f.ttft_ms !== null ? optKv("TTFT", `${trim1(f.ttft_ms)} ms`) : "",
      optKv("Age", fmtAge(f.age_secs)),
    ].join("");
  }

  /** One backend's measure/estimate card (brief §4): a discrete
   *  card per target in the backend-card idiom — header (id,
   *  model, running state), labeled figure rows, and the actions
   *  anchored in the card's own action row. Replaces the single
   *  flex row whose figures read as one continuous blob of text
   *  with buttons floating at its end. */
  function measureRow(b: OptimizeState["backends"][number], busy: boolean): string {
    const m = b.measured;
    // The estimate (tracker #56) renders beside the measured
    // figures under its own label — never in the measured
    // figures' place, and never without the word "Estimate".
    const e = b.estimated;
    return `<div class="opt-measure-card">
      <div class="opt-measure-head">
        <span class="opt-target">${esc(b.id)}</span>
        ${b.short_name || b.model_id ? `<span class="opt-measure-sub">${esc(b.short_name ?? b.model_id ?? "")}</span>` : ""}
        <span class="opt-measure-state">${b.running ? "Running" : "Stopped"}</span>
      </div>
      ${m ? `<div class="opt-measure-figs-block">${measureFigRows(m)}</div>` : e ? "" : `<div class="opt-note">Not measured</div>`}
      ${e ? `<div class="opt-measure-figs-block"><div class="opt-kv"><span class="opt-kv-label">Estimate —</span></div>${measureFigRows(e)}</div>` : ""}
      <div class="opt-measure-actions">
        <button class="btn" data-opt-measure="${esc(b.id)}" ${busy ? "disabled" : ""}>${icon("bolt")} Measure</button>
        <button class="btn" data-opt-estimate="${esc(b.id)}" ${busy ? "disabled" : ""}>${icon("gauge")} Estimate</button>
      </div>
    </div>`;
  }

  function candidateFitLabel(fit: string): string {
    switch (fit) {
      case "fits": return "Fits";
      case "fits_with_split": return "Fits with tensor split";
      case "does_not_fit": return "Does not fit";
      default: return "Not checked";
    }
  }

  /** The discovery block under the directive field: the Find
   *  candidates action (shown when a directive is present) and
   *  the stored, operator-prunable candidate list. The list is
   *  the exact set a consultation with the same directive draws
   *  from, minus the pruned entries. */
  function candidatesHtml(st: OptimizeState): string {
    const directiveText = (directiveDraft ?? st.last_directive ?? "").trim();
    const stored = st.candidates;
    const visible = stored?.items.filter((c) => !candidateRemoved.has(candidateKey(c))) ?? [];
    const rows = visible.map((c) => `<div class="opt-candidate-row">
        <span class="opt-candidate-figs">Repo: ${esc(c.repo_id)} · File: ${esc(c.file)} · Size: ${fmtGiB(c.size_bytes / 1048576)} · Params: ${trim1(c.params_b)} B · Fit: ${esc(candidateFitLabel(c.fit))}</span>
        <button class="btn icon-only" data-opt-candidate-remove="${esc(candidateKey(c))}" title="Remove candidate ${esc(c.repo_id)}" aria-label="Remove candidate ${esc(c.repo_id)} ${esc(c.file)}">${icon("x")}</button>
      </div>`).join("");
    return `<div class="opt-candidates">
      <div class="opt-consult-row" id="optDiscoverRow" style="margin-top:8px${directiveText ? "" : ";display:none"}">
        <button class="btn" id="optDiscoverBtn" data-opt-act="discover" ${discovering ? "disabled" : ""}>${icon("search")} ${discovering ? "Finding candidates…" : "Find candidates"}</button>
      </div>
      ${stored ? `<div class="opt-field-label" style="margin-top:10px">Candidates</div>
        <div class="opt-note">Directive: ${esc(stored.directive)} · Retrieved: ${esc(fmtTime(stored.retrieved_at))} · Shown: ${visible.length} of ${stored.items.length}</div>
        ${stored.directive !== directiveText ? `<div class="opt-note">These candidates were gathered for a different directive — a consultation only receives candidates gathered for its own directive.</div>` : ""}
        ${rows || `<div class="opt-note">All candidates pruned — the consultation will carry no candidates block.</div>`}` : ""}
    </div>`;
  }

  /** The §A.4 caveat, verbatim — stated wherever scores appear. */
  const SCREENING_CAVEAT = "Screening suite — a small fixed set of checkable tasks. It screens out regressions; it is not a benchmark and does not rank models generally.";

  function evalCategoryLabel(key: string): string {
    const labels: Record<string, string> = {
      instruction_following: "Instruction following",
      factual_recall: "Factual recall",
      arithmetic_reasoning: "Arithmetic/reasoning",
      code_adjacent: "Code-adjacent",
      summarization: "Summarization",
      tool_call_formatting: "Tool-call formatting",
      long_context_retrieval: "Long-context retrieval",
    };
    return labels[key] ?? key.replace(/_/g, " ");
  }

  /** The screening-suite block (Phase 5d, §A.4): the stored
   *  baseline's scores — overall and per-category, as Label:
   *  Value — with its backend, run time, and a stale marker
   *  when the backend's config fingerprint has moved; a Run
   *  baseline action on each running backend; and the §A.4
   *  caveat wherever scores render. Scores sit beside speed
   *  (the Measured throughput block above), never merged. */
  function screeningHtml(st: OptimizeState, busy: boolean): string {
    const b = st.eval_baseline;
    const running = st.backends.filter((x) => x.running);
    const age = b ? Math.floor((Date.now() - new Date(b.run_at).getTime()) / 1000) : NaN;
    const baselineHtml = b ? `
      <div class="opt-measure-card">
        <div class="opt-measure-head">
          <span class="opt-target">${esc(b.backend_id)}</span>
          ${b.stale ? `<span class="stale-tag">Stale — configuration changed since this run</span>` : ""}
        </div>
        <div class="opt-measure-figs-block">
          ${optKv("Overall", `${b.overall.passed}/${b.overall.total}`)}
          ${optKv("Run", fmtTime(b.run_at))}
          ${Number.isFinite(age) && age >= 0 ? optKv("Age", fmtAge(age)) : ""}
          ${optKv("Suite version", String(b.suite_version))}
          ${Object.entries(b.per_category).map(([k, s]) => optKv(evalCategoryLabel(k), `${s.passed}/${s.total}`)).join("")}
        </div>
      </div>
      <div class="opt-note">${esc(b.caveat)}</div>`
      : `<div class="opt-note">No baseline yet — run the suite against a running backend to record one.</div>
      <div class="opt-note">${esc(SCREENING_CAVEAT)}</div>`;
    return `<div class="opt-screening">
      <div class="opt-field-label" style="margin-top:14px">Screening suite</div>
      ${baselineHtml}
      ${running.map((rb) => `<div class="opt-measure-card">
        <div class="opt-measure-head">
          <span class="opt-target">${esc(rb.id)}</span>
          <span class="opt-measure-state">Running</span>
        </div>
        <div class="opt-measure-actions">
          <button class="btn" data-opt-eval="${esc(rb.id)}" ${busy ? "disabled" : ""}>${icon("target")} Run baseline</button>
        </div>
      </div>`).join("")}
      ${evalBackend !== null ? `<div class="opt-consult-actions">
        <span class="opt-consulting">${icon("clock")} Running the screening suite on ${esc(evalBackend)}${evalStart ? ` <span id="optEvalElapsed">${fmtElapsed(Math.floor((Date.now() - evalStart) / 1000))}</span>` : ""} — twelve checkable tasks, inside a 10-minute time-box.</span>
        <button class="btn danger" data-opt-act="cancel-job">${icon("x")} Cancel</button>
      </div>` : ""}
    </div>`;
  }

  /** The repo ids a consultation should receive: the stored
   *  candidates' repos after pruning, deduplicated. Undefined
   *  when no discovery set is stored (the server then has
   *  nothing to intersect). */
  function candidateReposForConsult(): string[] | undefined {
    if (!state?.candidates) return undefined;
    const repos: string[] = [];
    for (const c of state.candidates.items) {
      if (candidateRemoved.has(candidateKey(c))) continue;
      if (!repos.includes(c.repo_id)) repos.push(c.repo_id);
    }
    return repos;
  }

  function paint(): void {
    if (disposed || !host) return;
    if (loadError) {
      host.innerHTML = `<div class="banner">${icon("err")} Backend error — ${esc(loadError)}</div>`;
      return;
    }
    if (!state) {
      host.innerHTML = `<div class="card" style="color:var(--text2)">${icon("clock")} Loading…</div>`;
      return;
    }
    const st = state;
    const running = st.backends.filter((b) => b.running);
    const purposeShown = st.purpose_is_seed ? st.purpose_seed : st.purpose;
    const summary = st.context_summary;
    const contextLine = `GPUs: ${summary.gpus} · Backends: ${summary.backends} · Groups: ${summary.groups} · Models: ${summary.models}${summary.traffic_window_label ? ` · Traffic window: ${esc(summary.traffic_window_label)}` : ""}`;
    const consulting = st.consulting;
    // The elapsed clock reads the server's recorded start when
    // there is one: this page instance's own consultStart is 0
    // for a run it did not start, and (now − 0) rendered the
    // Unix epoch as an elapsed time (Oct 19, 2026 field report:
    // "29873905 min 35 s"). No known start, no elapsed figure.
    const consultStartMs = st.consult_started_unix_ms ?? (consultStart > 0 ? consultStart : null);
    const consultFailure = lastConsultError || st.consult_error || "";
    const measuring = st.measuring || measuringBackend !== null || estimatingBackend !== null || estimatesRunning;
    host.innerHTML = `
    <div class="card" id="optConsultCard">
      <div class="opt-consult-grid">
        <div>
          <div class="opt-field-label">Deployment purpose</div>
          ${purposeEditor("deployment", "", purposeShown, st.purpose_is_seed)}
          ${st.purpose_is_seed ? `<div class="opt-note">This description was generated from your setup. Edit it to make it yours — every consultation uses it.</div>` : ""}
          <div class="opt-field-label" style="margin-top:10px">Directive (optional)</div>
          <textarea id="optDirective" class="opt-directive-input" rows="2" placeholder="Scope this consultation — e.g. reduce coder latency">${esc(directiveDraft ?? st.last_directive ?? "")}</textarea>
          ${candidatesHtml(st)}
        </div>
        <div>
          <div class="opt-field-label">Consulting</div>
          <div class="opt-consult-row">
            ${running.length
              ? `<select id="optBackendSel" style="${SELECT_STYLE}">${running.map((b) => `<option value="${esc(b.id)}" ${b.id === st.default_backend ? "selected" : ""}>${esc(b.id)}${b.short_name ? ` · ${esc(b.short_name)}` : ""}</option>`).join("")}</select>`
              : `<span class="opt-note">No backend is running — start one to consult it, or generate a sample plan.</span>`}
          </div>
          ${st.better_backend ? `<div class="opt-note">${esc(st.better_backend.short_name ?? st.better_backend.id)} (${esc(st.better_backend.id)}) would be the stronger consultant but is stopped — start it to consult it.</div>` : ""}
          <div class="opt-field-label" style="margin-top:10px">Context</div>
          <div class="opt-context-line">${esc(contextLine)}</div>
          <details class="opt-why"><summary>View context</summary>
            <pre class="opt-pre">${esc(JSON.stringify(st.context, null, 2))}</pre>
          </details>
        </div>
      </div>
      <div class="opt-consult-actions">
        ${consulting
          ? `<span class="opt-consulting">${icon("clock")} Consulting${consultStartMs !== null ? ` <span id="optElapsed">${fmtElapsed(Math.max(0, Math.floor((Date.now() - consultStartMs) / 1000)))}</span>` : ""} — a large model on local hardware can take several minutes.</span>
             <button class="btn danger" data-opt-act="cancel-consult">${icon("x")} Cancel</button>`
          : `<button class="btn primary" data-opt-act="consult" ${running.length ? "" : "disabled"}>${icon("target")} ${armed.has("consult") ? "Replace the current plan?" : st.plan ? "Consult again" : "Consult"}</button>
             <button class="btn" data-opt-act="sample">${icon("layers")} Sample plan</button>
             ${armed.has("consult") ? `<span class="opt-note">A new consultation replaces the plan and its decisions. Click again to confirm.</span>` : ""}`}
      </div>
      ${consultFailure ? `<div class="banner">${icon("err")} Consultation failed — ${esc(consultFailure)}${st.plan ? ` The plan below is the previous plan, from ${esc(fmtTime(st.plan.created_at))}.` : ""}</div>` : ""}
      ${statusLine && !st.plan ? `<div class="opt-status">${esc(statusLine)}</div>` : ""}
      ${running.length ? `<div class="opt-measurements">
        <div class="opt-field-label" style="margin-top:14px">Measured throughput</div>
        ${running.map((b) => measureRow(b, measuring)).join("")}
        ${measuring ? `<div class="opt-consult-actions">
          ${estimatingBackend
            ? `<span class="opt-consulting">${icon("clock")} Estimating ${esc(estimatingBackend)}${estimateStart ? ` <span id="optEstimateElapsed">${fmtElapsed(Math.floor((Date.now() - estimateStart) / 1000))}</span>` : ""} — the abbreviated benchmark: the same two prompts, one run each, reduced generation budgets.</span>`
            : `<span class="opt-consulting">${icon("clock")} Measuring${measuringBackend ? ` ${esc(measuringBackend)}` : ""}${measureStart ? ` <span id="optMeasureElapsed">${fmtElapsed(Math.floor((Date.now() - measureStart) / 1000))}</span>` : ""} — the standardized benchmark runs two prompts, warm-up plus three measured runs each.</span>`}
          <button class="btn danger" data-opt-act="cancel-job">${icon("x")} Cancel</button>
        </div>` : ""}
      </div>` : ""}
      ${screeningHtml(st, measuring)}
    </div>
    <div id="optPlanHost">${planHtml(st)}</div>
    <details class="card opt-purposes">
      <summary>Per-group and per-backend purposes</summary>
      <p class="opt-note">Optional. A purpose tells the consulted model what a group or backend is for; without one it infers cautiously from names.</p>
      ${st.groups.map((g) => purposeEditor(`group:${g.name}`, `Group: ${g.name}`, g.purpose)).join("")}
      ${st.backends.map((b) => purposeEditor(`backend:${b.id}`, `Backend: ${b.id}`, b.purpose)).join("")}
    </details>`;
    // Keep the elapsed ticker alive for any consultation this
    // page can see, not only one it started: entering the page
    // mid-run starts it from the server's start time; the run
    // ending stops it. (runConsult also manages the timer for
    // its own runs; the null checks keep the two from doubling.)
    if (consulting && consultStartMs !== null) {
      if (elapsedTimer === null) {
        elapsedTimer = window.setInterval(() => {
          const el = hostEl.querySelector("#optElapsed");
          const startMs = state?.consult_started_unix_ms ?? (consultStart > 0 ? consultStart : null);
          if (el && startMs !== null) {
            el.textContent = fmtElapsed(Math.max(0, Math.floor((Date.now() - startMs) / 1000)));
          }
        }, 1000);
      }
    } else if (!consulting && elapsedTimer !== null) {
      window.clearInterval(elapsedTimer);
      elapsedTimer = null;
    }
  }

  async function refresh(): Promise<void> {
    const r = await getOptimizeState();
    if (disposed) return;
    if (r.error) {
      loadError = r.error;
    } else if (r.data) {
      state = r.data;
      loadError = null;
      // The server's recorded failure is authoritative: it
      // survives this page not having run the consultation.
      lastConsultError = r.data.consult_error ?? "";
    }
    paint();
  }

  async function runConsult(sample: boolean, directiveOverride?: string): Promise<void> {
    const sel = hostEl.querySelector<HTMLSelectElement>("#optBackendSel");
    const backendId = sample ? undefined : sel?.value || undefined;
    // Capture the directive before paint() rebuilds the card (and
    // the field with it); the draft survives the re-render. A
    // caller-supplied directive (Consult with results, Phase 5e)
    // wins over the field: that action consults under the
    // plan's own directive.
    if (directiveOverride !== undefined) {
      directiveDraft = directiveOverride;
    } else {
      const directiveField = hostEl.querySelector<HTMLTextAreaElement>("#optDirective");
      if (directiveField) directiveDraft = directiveField.value;
    }
    const directive = directiveDraft ?? state?.last_directive ?? "";
    consultStart = Date.now();
    lastConsultError = "";
    if (state) state.consulting = true;
    paint();
    if (elapsedTimer !== null) window.clearInterval(elapsedTimer);
    elapsedTimer = window.setInterval(() => {
      const el = hostEl.querySelector("#optElapsed");
      if (el) el.textContent = fmtElapsed(Math.floor((Date.now() - consultStart) / 1000));
    }, 1000);
    const r = await optimizeConsult(backendId, sample, directive, candidateReposForConsult());
    if (elapsedTimer !== null) { window.clearInterval(elapsedTimer); elapsedTimer = null; }
    if (disposed) return;
    if (r.error) {
      if (state) state.consulting = false;
      // The failure also renders as the banner above the plan
      // (which is then headed "Previous plan"); a deliberate
      // cancel is not a failure and gets no banner.
      lastConsultError = r.error === "consultation cancelled" ? "" : r.error;
      statusLine = r.error === "consultation cancelled"
        ? "Consultation cancelled."
        : `Consultation failed — ${r.error}`;
      if (state) paint(); else { loadError = r.error; paint(); }
      return;
    }
    if (r.data) {
      state = r.data;
      lastConsultError = r.data.consult_error ?? "";
      // The field now defaults to the directive the new plan
      // recorded (the server's trimmed verbatim text).
      directiveDraft = r.data.last_directive ?? "";
      statusLine = "";
    }
    paint();
  }

  async function runMeasure(backendId: string): Promise<void> {
    measuringBackend = backendId;
    measureStart = Date.now();
    paint();
    if (measureTimer !== null) window.clearInterval(measureTimer);
    measureTimer = window.setInterval(() => {
      const el = hostEl.querySelector("#optMeasureElapsed");
      if (el) el.textContent = fmtElapsed(Math.floor((Date.now() - measureStart) / 1000));
    }, 1000);
    const r = await optimizeMeasureBackend(backendId);
    if (measureTimer !== null) { window.clearInterval(measureTimer); measureTimer = null; }
    measuringBackend = null;
    measureStart = 0;
    if (disposed) return;
    if (r.error) {
      if (state) state.measuring = false;
      statusLine = r.error === "measurement cancelled"
        ? "Measurement cancelled."
        : `Measurement failed — ${r.error}`;
      paint();
      return;
    }
    if (r.data) {
      state = r.data;
      statusLine = "";
    }
    paint();
  }

  async function runEstimate(backendId: string): Promise<void> {
    estimatingBackend = backendId;
    estimateStart = Date.now();
    paint();
    if (estimateTimer !== null) window.clearInterval(estimateTimer);
    estimateTimer = window.setInterval(() => {
      const el = hostEl.querySelector("#optEstimateElapsed");
      if (el) el.textContent = fmtElapsed(Math.floor((Date.now() - estimateStart) / 1000));
    }, 1000);
    const r = await optimizeEstimateBackend(backendId);
    if (estimateTimer !== null) { window.clearInterval(estimateTimer); estimateTimer = null; }
    estimatingBackend = null;
    estimateStart = 0;
    if (disposed) return;
    if (r.error) {
      if (state) state.measuring = false;
      statusLine = r.error === "estimate cancelled"
        ? "Estimate cancelled."
        : `Estimate failed — ${r.error}`;
      paint();
      return;
    }
    if (r.data) {
      state = r.data;
      statusLine = "";
    }
    paint();
  }

  async function runEvalBaseline(backendId: string): Promise<void> {
    evalBackend = backendId;
    evalStart = Date.now();
    paint();
    if (evalTimer !== null) window.clearInterval(evalTimer);
    evalTimer = window.setInterval(() => {
      const el = hostEl.querySelector("#optEvalElapsed");
      if (el) el.textContent = fmtElapsed(Math.floor((Date.now() - evalStart) / 1000));
    }, 1000);
    const r = await optimizeRunBaseline(backendId);
    if (evalTimer !== null) { window.clearInterval(evalTimer); evalTimer = null; }
    evalBackend = null;
    evalStart = 0;
    if (disposed) return;
    if (r.error) {
      if (state) state.measuring = false;
      statusLine = r.error === "screening eval cancelled"
        ? "Screening baseline cancelled."
        : `Screening baseline failed — ${r.error}`;
      paint();
      return;
    }
    if (r.data) {
      state = r.data;
      statusLine = r.data.eval_baseline
        ? `Baseline recorded for ${r.data.eval_baseline.backend_id} — Overall: ${r.data.eval_baseline.overall.passed}/${r.data.eval_baseline.overall.total}.`
        : "";
    }
    paint();
  }

  async function runExperiments(): Promise<void> {
    experimentsRunning = true;
    expStart = Date.now();
    paint();
    if (expTimer !== null) window.clearInterval(expTimer);
    expTimer = window.setInterval(() => {
      const el = hostEl.querySelector("#optExpElapsed");
      if (el) el.textContent = fmtElapsed(Math.floor((Date.now() - expStart) / 1000));
    }, 1000);
    const r = await optimizeRunExperiments();
    if (expTimer !== null) { window.clearInterval(expTimer); expTimer = null; }
    experimentsRunning = false;
    expStart = 0;
    if (disposed) return;
    if (r.error) {
      if (state) state.measuring = false;
      statusLine = r.error === "experiment batch cancelled"
        ? "Experiment batch cancelled — temp backends were cleaned up and any stopped backends restarted."
        : `Experiment batch failed — ${r.error}`;
      paint();
      return;
    }
    if (r.data) {
      state = r.data;
      const results = (r.data.plan?.experiments ?? []).filter((e) => e.result);
      statusLine = results.length
        ? `Experiments finished — ${results.filter((e) => e.result?.status === "completed").length} of ${results.length} completed. Results are in the Experiments section.`
        : "";
    }
    paint();
  }

  async function runEstimateExperiments(): Promise<void> {
    estimatesRunning = true;
    expEstStart = Date.now();
    paint();
    if (expEstTimer !== null) window.clearInterval(expEstTimer);
    expEstTimer = window.setInterval(() => {
      const el = hostEl.querySelector("#optExpEstElapsed");
      if (el) el.textContent = fmtElapsed(Math.floor((Date.now() - expEstStart) / 1000));
    }, 1000);
    const r = await optimizeEstimateExperiments();
    if (expEstTimer !== null) { window.clearInterval(expEstTimer); expEstTimer = null; }
    estimatesRunning = false;
    expEstStart = 0;
    if (disposed) return;
    if (r.error) {
      if (state) state.measuring = false;
      statusLine = r.error === "experiment estimate batch cancelled"
        ? "Experiment estimate batch cancelled — temp backends were cleaned up and any stopped backends restarted."
        : `Experiment estimate batch failed — ${r.error}`;
      paint();
      return;
    }
    if (r.data) {
      state = r.data;
      const estimated = (r.data.plan?.experiments ?? []).filter((e) => e.estimate);
      const acceptedCount = (r.data.plan?.experiments ?? []).filter((e) => e.decision === "accepted").length;
      statusLine = estimated.length
        ? `Experiment estimates finished — ${estimated.length} of ${acceptedCount} accepted estimated. Estimates are in the Experiments section.`
        : "";
    }
    paint();
  }

  async function runDiscover(): Promise<void> {
    const directiveField = hostEl.querySelector<HTMLTextAreaElement>("#optDirective");
    if (directiveField) directiveDraft = directiveField.value;
    const directive = (directiveDraft ?? "").trim();
    if (!directive) {
      statusLine = "Candidate discovery needs a directive — enter one first.";
      paint();
      return;
    }
    discovering = true;
    paint();
    const r = await optimizeDiscover(directive);
    discovering = false;
    if (disposed) return;
    if (r.error) {
      statusLine = `Candidate discovery failed — ${r.error}`;
      paint();
      return;
    }
    if (r.data) {
      state = r.data;
      // A fresh stored set replaces the pruned view of the old one.
      candidateRemoved.clear();
      statusLine = r.data.candidates
        ? `Found ${r.data.candidates.items.length} candidate${r.data.candidates.items.length === 1 ? "" : "s"} for this directive.`
        : "Discovery found no candidates for this directive.";
    }
    paint();
  }

  /* ---------------- Phase 4: apply orchestration ---------------- */

  async function cfgGroups(): Promise<CfgGroup[]> {
    const cfg = unwrap(await getAppConfig());
    const groups = (cfg?.gateway as { groups?: CfgGroup[] } | undefined)?.groups;
    return Array.isArray(groups) ? groups : [];
  }

  async function cfgBackends(): Promise<CfgBackend[]> {
    const cfg = unwrap(await getAppConfig());
    const backends = cfg?.backends as CfgBackend[] | undefined;
    return Array.isArray(backends) ? backends : [];
  }

  /** Execute one accepted item through the existing commands.
   *  Throws an Error with the failure message. */
  async function executeItem(item: OptimizePlanItem): Promise<void> {
    const c = item.change ?? {};
    const target = item.target ?? "";
    const str = (k: string): string => String(c[k] ?? "");
    const numOrUndef = (k: string): number | undefined => typeof c[k] === "number" ? (c[k] as number) : undefined;
    switch (item.kind) {
      case "backend.create": {
        const backends = await cfgBackends();
        const groups = await cfgGroups();
        const cfg = unwrap(await getAppConfig());
        const models = (cfg?.models as CfgModel[] | undefined) ?? [];
        const model = models.find((m) => m.id === str("model_id"));
        if (!model) throw new Error(`model '${str("model_id")}' is not registered`);
        let group = str("group");
        if (!group) {
          const sibling = backends.find((b) => b.model_file === model.gguf_path);
          const siblingGroup = sibling ? groups.find((g) => g.members.includes(sibling.id)) : undefined;
          group = siblingGroup?.name ?? target;
        }
        unwrap(await addBackend({
          id: target, modelId: model.id, port: numOrUndef("port") ?? 0,
          group, createGroup: !groups.some((g) => g.name === group), enabled: true,
        }));
        const nCtx = numOrUndef("n_ctx");
        const nBatch = numOrUndef("n_batch");
        if (nCtx !== undefined || nBatch !== undefined) {
          unwrap(await updateBackendFlags(target, nCtx ?? 32768, nBatch ?? 512));
        }
        const offload = numOrUndef("offload_layers");
        if (offload !== undefined) unwrap(await setBackendOffload(target, offload));
        if (Array.isArray(c.tensor_split)) {
          unwrap(await applyBackendConfig(target, {
            tensor_split: c.tensor_split, split_mode: "layer",
          }, "OnNextRestart"));
        }
        return;
      }
      case "backend.delete": {
        const rows = unwrap(await getBackends());
        const row = rows.find((b) => b.id === target);
        if (row && row.state === "running") {
          unwrap(await stopBackend(target));
          const deadline = Date.now() + 30000;
          for (;;) {
            const now = unwrap(await getBackends());
            const s = now.find((b) => b.id === target)?.state;
            if (s === "stopped" || s === undefined) break;
            if (Date.now() > deadline) throw new Error(`backend '${target}' did not stop within 30 s`);
            await new Promise((r) => window.setTimeout(r, 1000));
          }
        }
        unwrap(await deleteBackend(target));
        return;
      }
      case "backend.set_context":
      case "backend.set_flags": {
        const cfgB = unwrap(await getBackendConfig(target));
        if (!cfgB) throw new Error(`backend '${target}' not found`);
        const flags = cfgB.flags as { n_ctx?: number; n_batch?: number };
        unwrap(await updateBackendFlags(
          target,
          numOrUndef("n_ctx") ?? flags.n_ctx ?? 32768,
          numOrUndef("n_batch") ?? flags.n_batch ?? 512,
        ));
        return;
      }
      case "backend.set_offload": {
        unwrap(await setBackendOffload(target, numOrUndef("offload_layers") ?? 0));
        return;
      }
      case "backend.set_placement": {
        const patch: Record<string, unknown> = { tensor_split: c.tensor_split };
        if (typeof c.split_mode === "string") patch.split_mode = c.split_mode;
        unwrap(await applyBackendConfig(target, patch, "OnNextRestart"));
        return;
      }
      case "backend.set_model": {
        const backends = await cfgBackends();
        const b = backends.find((x) => x.id === target);
        unwrap(await modelAssign(target, str("model_id"), b?.flags.tensor_split ?? []));
        return;
      }
      case "backend.set_enabled": {
        unwrap(await applyBackendConfig(target, { enabled: c.enabled === true }, "OnNextRestart"));
        return;
      }
      case "group.set_members":
      case "group.set_strategy":
      case "group.set_fallbacks": {
        const groups = (await cfgGroups()).map((g) => {
          if (g.name !== target) return g;
          const next = { ...g };
          if (item.kind === "group.set_members") next.members = (c.members as string[]) ?? [];
          if (item.kind === "group.set_strategy") next.strategy = str("strategy");
          if (item.kind === "group.set_fallbacks") next.fallbacks = (c.fallbacks as string[]) ?? [];
          return next;
        });
        unwrap(await updateGatewayConfig({ groups }));
        return;
      }
      case "group.create": {
        const groups = await cfgGroups();
        groups.push({
          name: target,
          members: (c.members as string[]) ?? [],
          strategy: str("strategy") || "simple-shuffle",
          fallbacks: (c.fallbacks as string[]) ?? [],
          purpose: "",
        });
        unwrap(await updateGatewayConfig({ groups }));
        return;
      }
      case "group.delete": {
        const groups = (await cfgGroups()).filter((g) => g.name !== target);
        unwrap(await updateGatewayConfig({ groups }));
        return;
      }
      case "model.add": {
        unwrap(await modelAdd(target, str("source_url"), undefined, typeof c.quant === "string" ? (c.quant as string) : undefined, typeof c.params_b === "number" ? (c.params_b as number) : undefined));
        return;
      }
      case "model.download": {
        // Experiment provisioning: fetch the registered model's
        // file through the existing download machinery and wait
        // for it to land — an experiment runs its candidate from
        // disk, so the step is done only when the file is on
        // disk and verified (or failed, which the row reports).
        const before = unwrap(await getModels());
        const row = before.find((m) => m.id === target);
        if (!row) throw new Error(`model '${target}' is not registered`);
        if (row.on_disk && !row.downloading) return;
        // The op refuses a second spawn while one is in flight;
        // an already-running download is simply awaited.
        if (!row.downloading) unwrap(await downloadModel(target));
        const deadline = Date.now() + 2 * 60 * 60 * 1000;
        for (;;) {
          await new Promise((r) => window.setTimeout(r, 2000));
          const rows = unwrap(await getModels());
          const now = rows.find((m) => m.id === target);
          if (now?.download_error) throw new Error(now.download_error);
          if (now?.on_disk && !now.downloading && !now.verifying) return;
          if (Date.now() > deadline) {
            throw new Error(`download of '${target}' did not finish within 2 hours — it continues in the background; watch it in the Models view`);
          }
        }
      }
      default:
        throw new Error(`kind ${item.kind} has no executor`);
    }
  }

  async function runApplyPhase(destructivePhase: boolean): Promise<void> {
    if (!state?.plan || applyRunning) return;
    const plan = state.plan;
    const queue = plan.items.filter((i) =>
      i.decision === "accepted"
      && plan.validations[i.id - 1]?.status !== "invalid"
      && i.apply_outcome !== "applied"
      && optimizeIsDestructive(i.kind) === destructivePhase);
    if (!queue.length) return;
    applyRunning = true;
    statusLine = "";
    paint();
    let failed = false;
    for (const item of queue) {
      if (disposed) break;
      if (failed) {
        const r = await optimizeSetItemOutcome(item.id, "not_attempted");
        if (r.data) state = r.data;
        continue;
      }
      try {
        await executeItem(item);
        const r = await optimizeSetItemOutcome(item.id, "applied");
        if (r.data) state = r.data;
      } catch (e) {
        failed = true;
        const msg = e instanceof Error ? e.message : String(e);
        const r = await optimizeSetItemOutcome(item.id, "failed", msg);
        if (r.data) state = r.data;
        statusLine = `Apply stopped at item ${item.id} — ${msg}. Items after it were not attempted. Applied changes are saved, not live: activate them from the banner or the Activation Plan page.`;
      }
      paint();
    }
    if (!failed && !disposed) {
      statusLine = "Applied. The changes are saved, not live — activate them from the banner or the Activation Plan page.";
    }
    applyRunning = false;
    armed.delete("destructive");
    await refresh();
    if (statusLine) paint();
  }

  /* ---------------- events ---------------- */

  hostEl.addEventListener("click", (ev) => {
    const t = ev.target as HTMLElement;
    const expTri = t.closest<HTMLElement>("[data-opt-exp-tri]");
    if (expTri && state?.plan) {
      const [idStr, decision] = (expTri.dataset.optExpTri ?? "").split(":");
      const id = Number(idStr);
      if (id && (decision === "accepted" || decision === "denied" || decision === "undecided")) {
        void (async () => {
          const r = await optimizeSetExperimentState(id, decision);
          if (disposed) return;
          if (r.error) { statusLine = r.error; paint(); return; }
          if (r.data) state = r.data;
          paint();
        })();
      }
      return;
    }
    const tri = t.closest<HTMLElement>("[data-opt-tri]");
    if (tri && state?.plan) {
      const [idStr, decision] = (tri.dataset.optTri ?? "").split(":");
      const id = Number(idStr);
      if (id && (decision === "accepted" || decision === "denied" || decision === "undecided")) {
        void (async () => {
          const r = await optimizeSetItemState(id, decision);
          if (disposed) return;
          if (r.error) { statusLine = r.error; paint(); return; }
          if (r.data) state = r.data;
          paint();
        })();
      }
      return;
    }
    const purposeEdit = t.closest<HTMLElement>("[data-opt-purpose-edit]");
    if (purposeEdit) {
      const key = purposeEdit.dataset.optPurposeEdit ?? "";
      openEditors.add(key);
      paint();
      return;
    }
    const purposeCancel = t.closest<HTMLElement>("[data-opt-purpose-cancel]");
    if (purposeCancel) {
      const key = purposeCancel.dataset.optPurposeCancel ?? "";
      openEditors.delete(key);
      drafts.delete(key);
      paint();
      return;
    }
    const purposeSave = t.closest<HTMLElement>("[data-opt-purpose-save]");
    if (purposeSave) {
      const key = purposeSave.dataset.optPurposeSave ?? "";
      const input = hostEl.querySelector<HTMLTextAreaElement>(`[data-opt-purpose-input="${CSS.escape(key)}"]`);
      const text = input?.value ?? "";
      let scope: "deployment" | "group" | "backend" = "deployment";
      let target: string | undefined;
      if (key.startsWith("backend:")) { scope = "backend"; target = key.slice("backend:".length); }
      if (key.startsWith("group:")) { scope = "group"; target = key.slice("group:".length); }
      void (async () => {
        const r = await optimizeSavePurpose(scope, text, target);
        if (disposed) return;
        if (r.error) { statusLine = r.error; paint(); return; }
        openEditors.delete(key);
        drafts.delete(key);
        if (r.data) state = r.data;
        paint();
      })();
      return;
    }
    const measureBtn = t.closest<HTMLElement>("[data-opt-measure]");
    if (measureBtn) {
      const backendId = measureBtn.dataset.optMeasure ?? "";
      if (backendId) void runMeasure(backendId);
      return;
    }
    const estimateBtn = t.closest<HTMLElement>("[data-opt-estimate]");
    if (estimateBtn) {
      const backendId = estimateBtn.dataset.optEstimate ?? "";
      if (backendId) void runEstimate(backendId);
      return;
    }
    const evalBtn = t.closest<HTMLElement>("[data-opt-eval]");
    if (evalBtn) {
      const backendId = evalBtn.dataset.optEval ?? "";
      if (backendId) void runEvalBaseline(backendId);
      return;
    }
    const candidateRemove = t.closest<HTMLElement>("[data-opt-candidate-remove]");
    if (candidateRemove) {
      const key = candidateRemove.dataset.optCandidateRemove ?? "";
      if (key) candidateRemoved.add(key);
      paint();
      return;
    }
    const act = t.closest<HTMLElement>("[data-opt-act]")?.dataset.optAct;
    if (!act) return;
    switch (act) {
      case "cancel-job":
        void optimizeCancelJob();
        return;
      case "consult": {
        const hasDecisions = state?.plan?.items.some((i) => i.decision !== "undecided" || i.apply_outcome);
        if (state?.plan && hasDecisions && !armed.has("consult")) {
          armed.add("consult");
          paint();
          window.setTimeout(() => { armed.delete("consult"); if (!disposed) paint(); }, 5000);
          return;
        }
        armed.delete("consult");
        void runConsult(false);
        return;
      }
      case "sample":
        void runConsult(true);
        return;
      case "discover":
        void runDiscover();
        return;
      case "run-experiments":
        void runExperiments();
        return;
      case "estimate-experiments":
        void runEstimateExperiments();
        return;
      case "consult-results":
        void runConsult(false, state?.plan?.directive ?? "");
        return;
      case "cancel-consult":
        void optimizeCancelConsult();
        return;
      case "discard": {
        // Discarding deletes the operator's decisions along with
        // the plan, so it takes the same two-step confirm as the
        // other destructive actions.
        if (!armed.has("discard")) {
          armed.add("discard");
          statusLine = "Click the X again to discard the plan and its decisions.";
          paint();
          window.setTimeout(() => { armed.delete("discard"); if (!disposed) paint(); }, 5000);
          return;
        }
        armed.delete("discard");
        void (async () => {
          const r = await optimizeDiscardPlan();
          if (disposed) return;
          if (r.data) {
            state = r.data;
            directiveDraft = r.data.last_directive ?? "";
          }
          statusLine = "";
          paint();
        })();
        return;
      }
      case "revalidate":
        void refresh();
        return;
      case "apply":
        void runApplyPhase(false);
        return;
      case "apply-destructive": {
        if (!armed.has("destructive")) {
          armed.add("destructive");
          paint();
          window.setTimeout(() => { armed.delete("destructive"); if (!disposed) paint(); }, 5000);
          return;
        }
        void runApplyPhase(true);
        return;
      }
    }
  });

  hostEl.addEventListener("input", (ev) => {
    const t = ev.target as HTMLElement;
    if (t.id === "optDirective" && t instanceof HTMLTextAreaElement) {
      directiveDraft = t.value;
      // Toggle the Find candidates action with the field's
      // content, without a repaint (which would drop the caret).
      const row = hostEl.querySelector<HTMLElement>("#optDiscoverRow");
      if (row) row.style.display = t.value.trim() ? "" : "none";
      return;
    }
    const key = t.dataset?.optPurposeInput;
    if (key && t instanceof HTMLTextAreaElement) drafts.set(key, t.value);
  });

  await refresh();
  return () => {
    disposed = true;
    if (elapsedTimer !== null) window.clearInterval(elapsedTimer);
    if (measureTimer !== null) window.clearInterval(measureTimer);
    if (estimateTimer !== null) window.clearInterval(estimateTimer);
    if (evalTimer !== null) window.clearInterval(evalTimer);
    if (expTimer !== null) window.clearInterval(expTimer);
    if (expEstTimer !== null) window.clearInterval(expEstTimer);
  };
}
