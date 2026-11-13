/* Secondary views — all wired to the backend through api.ts.
   Each view follows the Setup wizard pattern: renderX() returns a shell
   with a loading placeholder; wireX(root) loads live data, paints it, and
   attaches handlers. Under Tauri the data is real; in a plain browser
   (live=false) views show a sample-mode banner and honest empty states
   instead of fabricated data. */

/* The secondary views are being split one module per view (T-02h); the
   views still defined below have not moved yet. Every name this file
   exported before the split is still exported from here. */

import { activationApply, activationCancelChanges, addBackend, applyBackendConfig, backendAddPreview, backendCachePreview, cancelDownload, clearBackendOffloadOverride, deleteBackend, deleteModel, downloadModel, getActivationExecution, getActivationHistory, getActivationPlan, getBackendConfig, getBackendLaunchCommand, getBackends, getGatewayConfig, getGatewayMasterKey, getGatewayStatus, getGatewayYaml, getGpuTelemetry, getLogs, getModels, isTauri, measurementLookup, modelAdd, modelAssign, modelHfHeader, modelHfLookup, modelHfSuggest, modelSetEnabled, modelSetMtp, modelSetOffloadSpec, modelSetShortName, openRouterSpendBannerHtml, openUrl, optimizeMeasureBackend, pauseModelDownload, placementApply, placementPropose, resumeModelDownload, setBackendCache, setBackendFlagsMode, setBackendMoeCache, setBackendMtp, setBackendOffload, setBackendOffloadOverride, setBackendParallel, setBackendRawFlags, setGatewayBindHost, testBackend, testBackendToolCall, updateBackendFlags, updateGatewayConfig, verifyModel } from "../api";
import type { ActivationChange, ActivationExecution, ActivationExecutionSummary, ActivationPlanData, BackendAddPreview, BackendMoeCache, BackendMtp, BackendOffload, BackendRow, CachePreview, CacheSelectionLike, CacheTypeName, FlashAttnName, GatewayConfigData, GatewayGroup, GatewayRule, GatewayRuleKind, GatewayStatusData, HfHeaderResult, HfLookupQuant, HfLookupRepo, LogRow, MeasurementLookupRequest, ModelRow, PlacementProjectionData, PredictedThroughput } from "../api";
import { fmtCount, fmtGb, meter, stackedBar, trim1 } from "../charts";
import { esc, setHtml } from "../html";
import { icon } from "../icons";
import { LIFECYCLE_STATES, META, applyBlockedReason, fromWire, lifecycleActions, lifecycleChip, lifecycleDot, lifecycleLabel, renderLifecycleActions, runLifecycle, serviceStore, wireLifecycleActions } from "../lifecycle";
import type { LifecycleResult, LifecycleState, ServiceSnapshot } from "../lifecycle";
import { enableSwitch } from "../switch";
import type { GpuInfo } from "../types";
import { C, SELECT_STYLE, bannerHtml, emptyHtml, fmtBytes, fmtDate, fmtElapsed, fmtTime, fmtUptime, gpuShortName, loadingHtml, modelFileShortName, modelOptions, modelShortName, onOvGoto, ovStatusBtn, quoteArg, sameModelFile, setBanner, topbar } from "./common";
import type { CardNote } from "./common";

/* ---------------- Gateway activation plan ----------------
 * Saved configuration changes that are not live yet — gateway
 * changes waiting on a gateway restart, backend changes (flags,
 * tensor split, model assignment) waiting on that backend's
 * (re)start. The card renders from the server-side plan (a
 * persisted journal plus live config drift), so it survives
 * navigation and app restarts until the operator executes, defers,
 * or cancels it — the pre-v0.1.22 behavior (a transient card on
 * the add-backend form, nothing after dismissal) is what Pete
 * reported on 2026-10-15. The same plan drives the app shell's
 * global banner and the dedicated Activation Plan view. */

interface PlanUiState {
  dismissed: boolean;
  status: string;
  statusErr: boolean;
  confirmCancel: boolean;
  acting: boolean;
  /** Manual-command block from the last failed start-gateway
   * outcome, rendered under the status line. */
  manualCommand: string;
}

function newPlanUi(): PlanUiState {
  return { dismissed: false, status: "", statusErr: false, confirmCancel: false, acting: false, manualCommand: "" };
}

/** The manual-command code block shared by the plan card surfaces:
 * the block's own first line says what it is, so it renders bare.
 * The commands are a value the operator must run by hand, so the
 * block carries a Copy button (principle 10) — word-break:break-all
 * makes manual selection error-prone. The button routes through the
 * plan surfaces' shared click handling (data-plan-act). */
function manualCommandHtml(cmd: string): string {
  return `<pre style="margin:6px 0;font-family:var(--mono);font-size:11px;white-space:pre-wrap;word-break:break-all;background:var(--panel);border:1px solid var(--border);border-radius:6px;padding:8px;text-align:left">${esc(cmd)}</pre>
  <div><button class="btn icon-only" data-plan-act="copy-manual" title="Copy commands" aria-label="Copy commands">${icon("copy")}</button></div>`;
}

/** Scope + activation label for one plan change, e.g.
 *  "Backend: coder-fast · restart required". */
function planChangeMeta(c: ActivationPlanData["changes"][number]): string {
  const scope = c.scope === "backend"
    ? `Backend${c.targets && c.targets.length ? `: ${c.targets.join(", ")}` : ""}`
    : "Gateway";
  const act = c.activation === "restart" || c.activation === "gateway_restart"
    ? "restart required"
    : "applies at next start";
  return `${scope} · ${act}`;
}

/** Join backend names the way the plan copy lists them:
 * "coder", "coder and planner", "coder, planner and coder-fast". */
function planNameList(names: string[]): string {
  if (names.length <= 1) return names.join("");
  return `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;
}

/** The plan's consequence line, composed from the pending journal
 * entries instead of static boilerplate: Apply restarts the
 * running backends the plan names (backend_restart_ids — the same
 * list applyActivationPlan restarts), then the gateway ONLY when a
 * gateway-scoped change is pending (a stopped gateway is started,
 * not restarted); targets whose process is stopped are named as
 * taking effect at their next start, and when nothing affected is
 * running the line says so. A gateway (re)start is always named
 * with what it can involve: the start-gateway step verifies the
 * gateway's install first and repairs it when a check fails
 * (venv, package install, Prisma generation), so the line says so
 * — an operator must never learn that a plan restarted (let alone
 * reinstalled) the gateway only by watching it happen (Pete's
 * report, 2026-10-18). Exported for the app shell's activation
 * banner, which carries the same line. Every surface renders this
 * line as visible text next to the Apply actions, not only in the
 * buttons' tooltip. */
export function planApplyLine(plan: ActivationPlanData): string {
  const restarting = plan.backend_restart_ids ?? [];
  const backendTargets: string[] = [];
  for (const c of plan.changes) {
    if (c.scope !== "backend") continue;
    for (const t of c.targets ?? []) {
      if (!backendTargets.includes(t)) backendTargets.push(t);
    }
  }
  const stopped = backendTargets.filter((t) => !restarting.includes(t));
  if (!restarting.length && !plan.gateway_pending) {
    const names = planNameList(backendTargets);
    return names
      ? `Nothing running holds the old state — these changes take effect at the next start of ${names}.`
      : "Nothing running holds the old state — these changes take effect at the next start of the affected process.";
  }
  let lead: string;
  if (restarting.length && plan.gateway_pending) {
    lead = `Apply restarts ${planNameList(restarting)}, then ${plan.gateway_up ? "the gateway" : "starts the gateway"}`;
  } else if (restarting.length) {
    lead = `Apply restarts ${planNameList(restarting)}`;
  } else {
    lead = plan.gateway_up ? "Apply restarts the gateway" : "Apply starts the gateway";
  }
  const gatewayNote = plan.gateway_pending
    ? " The gateway start verifies its install first and repairs or reinstalls it if a check fails."
    : "";
  const tail = !stopped.length
    ? ""
    : stopped.length === 1
      ? ` The change to ${stopped[0]} takes effect at its next start.`
      : ` Changes to ${planNameList(stopped)} take effect at their next start.`;
  return `${lead}.${gatewayNote}${tail}`;
}

/** The plan's aggregate counts, each computed once and rendered in
 * exactly one place per surface: changes split by scope (backend vs
 * gateway) and by activation class (restart required vs next start —
 * a change is "next start" exactly when its activation says so;
 * every other class needs a restart of something running). */
function planCounts(plan: ActivationPlanData): { backend: number; gateway: number; restart: number; nextStart: number } {
  const backend = plan.changes.filter((c) => c.scope === "backend").length;
  const nextStart = plan.changes.filter((c) => c.activation === "next_start").length;
  return {
    backend,
    gateway: plan.changes.length - backend,
    restart: plan.changes.length - nextStart,
    nextStart,
  };
}

/** Scope composition as a mini segmented bar: backend segment vs
 * gateway segment, widths proportional to the per-scope counts, the
 * counts in the segments' tooltips and the names as adjacent keys.
 * The drift-only state (no journal changes) is a single gray
 * segment keyed "Gateway drift". */
function planScopeBarHtml(plan: ActivationPlanData): string {
  const n = plan.changes.length;
  if (!n) {
    return `<span style="display:inline-flex;align-items:center;gap:8px;flex:0 1 260px;min-width:150px">${stackedBar([{ pct: 100, color: "#6d7686", label: "Gateway drift" }], 260, 8)}<span style="font-size:11px;color:var(--text3);white-space:nowrap">Gateway drift</span></span>`;
  }
  const { backend, gateway } = planCounts(plan);
  const segs: { pct: number; color: string; label?: string }[] = [];
  if (backend) segs.push({ pct: (backend / n) * 100, color: C.blue, label: `Backend: ${backend}` });
  if (gateway) segs.push({ pct: (gateway / n) * 100, color: C.purple, label: `Gateway: ${gateway}` });
  const keys = [
    backend ? `<span><span class="sw" style="background:${C.blue}"></span>Backend</span>` : "",
    gateway ? `<span><span class="sw" style="background:${C.purple}"></span>Gateway</span>` : "",
  ].join("");
  return `<span style="display:inline-flex;align-items:center;gap:8px;flex:0 1 340px;min-width:180px">${stackedBar(segs, 340, 8)}<span class="legend" style="margin-top:0;flex-wrap:nowrap">${keys}</span></span>`;
}

/** The Apply buttons shared by both plan variants, with the full
 * view's mode semantics: drain-then-restart or restart-now when the
 * gateway is up with a gateway change pending, else the single
 * Apply plan (start) action. The composed consequence line
 * (planApplyLine) rides the buttons' tooltip AND renders as visible
 * text beside the actions on every surface: what an apply will do
 * — above all a gateway restart and its possible install repair —
 * is a consequence the operator must see before clicking, not
 * explanation that can wait for a hover (Pete's requirement,
 * 2026-10-18; the visible rendering lives in activationPlanHtml
 * and the shell banner). */
function planApplyButtons(plan: ActivationPlanData, ui: PlanUiState): string {
  if (!plan.actionable) return "";
  // Disabled on every surface while ANY apply runs (this card's, the
  // banner's, another window's or a REST/CLI one): the server's
  // activation_applying is the shared signal (ACT-9).
  const dis = ui.acting || activationApplying() ? " disabled" : "";
  const tip = esc(planApplyLine(plan));
  // Drain-or-restart keys on the gateway's lifecycle state from the
  // shared store (the same rule as the shell banner); the plan's own
  // probe is the fallback before the store's first snapshot.
  const gs = serviceStore.gatewayState();
  const gwLive = gs !== null ? META[gs].isLive : plan.gateway_up;
  return plan.gateway_pending && gwLive
    ? `<button class="btn" data-plan-act="quiesce" title="${tip}"${dis}>${icon("pause")} Drain Requests and Apply</button>
       <button class="btn" data-plan-act="restart" title="${tip}"${dis}>${icon("restart")} Apply now</button>`
    : `<button class="btn" data-plan-act="start" title="${tip}"${dis}>${icon("play")} Apply plan</button>`;
}

/** The server-side activation apply in flight (any surface), from the
 *  shared service store; null when idle or not yet measured. */
export function activationApplying(): { message: string } | null {
  return serviceStore.get().gateway?.activation_applying ?? null;
}

function planStatusHtml(ui: PlanUiState): string {
  // While an apply started elsewhere runs, its progress line shows
  // here too (this surface's own status, when set, wins).
  const applying = activationApplying();
  if (applying && !ui.status) {
    return `<div style="font-size:12px;margin-top:6px;color:var(--text2)">${icon("clock")} Applying… ${esc(applying.message)}</div>`;
  }
  return `${ui.status ?`<div style="font-size:12px;margin-top:6px;color:${ui.statusErr ? "var(--err)" : "var(--text2)"}">${esc(ui.status)}</div>` : ""}
    ${ui.manualCommand ? manualCommandHtml(ui.manualCommand) : ""}`;
}

/** The activation plan card, in two variants of one component:
 * "summary" — one line (warn icon, the pending count as Label:
 * Value, the scope bar, Review + Apply) for the Backends and
 * Gateway views and for the dismissed state everywhere; "full" —
 * the Activation Plan view's summary header (count tiles + scope
 * bar + actions), whose per-change detail is the Pending changes
 * table rendered by wireActivationPlan below it. The card never
 * lists the changes itself: the table is the one detail view. */
function activationPlanHtml(plan: ActivationPlanData | null, ui: PlanUiState, variant: "summary" | "full" = "full"): string {
  if (!plan || !plan.pending) return "";
  const n = plan.changes.length;
  const dis = ui.acting ? " disabled" : "";
  if (variant === "summary" || ui.dismissed) {
    return `<div class="card" style="margin-bottom:12px;border-color:var(--amber)">
    <div style="display:flex;align-items:center;gap:10px;flex-wrap:wrap;font-size:12px">
      ${icon("warn")} <span>Pending changes: <b>${n}</b></span>
      ${planScopeBarHtml(plan)}
      <span class="spacer" style="flex:1"></span>
      ${ui.dismissed
        ? `<button class="btn" data-plan-act="review" title="Review activation plan"${dis}>${icon("eye")} Review</button>`
        : `<a class="btn" href="#/activation-plan" title="Review activation plan">${icon("eye")} Review</a>`}
      ${planApplyButtons(plan, ui)}
    </div>
    ${plan.actionable ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">${esc(planApplyLine(plan))}</div>` : ""}
    ${planStatusHtml(ui)}
  </div>`;
  }
  const { restart, nextStart } = planCounts(plan);
  const tiles: [string, string][] = [
    ["Pending changes", String(n)],
    ["Restart required", String(restart)],
    ["Next start", String(nextStart)],
  ];
  const cancelBtn = plan.snapshot_available
    ? (ui.confirmCancel
      ? `<button class="btn" data-plan-act="cancel-confirm" title="Cancelling restores the configuration from before these changes and clears the plan."${dis}>${icon("ban")} Confirm cancel changes</button>`
      : `<button class="btn" data-plan-act="cancel"${dis}>${icon("ban")} Cancel changes</button>`)
    : "";
  return `<div class="card" style="margin-bottom:12px;border-color:var(--amber)">
    <h3>${icon("warn")} Activation plan <span class="spacer" style="flex:1"></span><button class="btn icon-only" data-plan-act="later" title="Dismiss" aria-label="Dismiss"${dis}>${icon("x")}</button></h3>
    <div style="display:grid;grid-template-columns:repeat(3,1fr);gap:10px;margin-top:10px">
      ${tiles.map(([k, v]) => `<div class="kv"><span class="k">${k}</span><span class="v" style="font-size:13px">${esc(v)}</span></div>`).join("")}
    </div>
    <div style="margin-top:10px">${planScopeBarHtml(plan)}</div>
    ${plan.drift ? `<div style="font-size:12px;color:var(--text2);margin-top:8px">Gateway config: <b style="color:var(--text)">drifted</b><span class="info-tip" title="The running gateway's configuration differs from the saved configuration.">${icon("info")}</span></div>` : ""}
    ${plan.actionable ? `<div style="font-size:12px;color:var(--text2);margin-top:8px">${esc(planApplyLine(plan))}</div>` : ""}
    <div class="card-actions">${planApplyButtons(plan, ui)}
      ${cancelBtn}
      <button class="btn icon-only" data-plan-refresh title="Refresh" aria-label="Refresh">${icon("restart")}</button></div>
    ${planStatusHtml(ui)}
  </div>`;
}

/** Apply the pending activation plan through the ONE server-side
 *  apply (activation_apply): the server re-reads the plan, restarts
 *  each stale backend (waiting for /health), restarts the gateway
 *  when a gateway change is pending (never PostgreSQL), and records
 *  one execution in the history with source "desktop". Nothing is
 *  orchestrated client-side any more — no client gateway stop, no
 *  activationComplete, no client history record (ACT-3).
 *
 *  While the call runs, the server's progress line
 *  (gateway_status.activation_applying.message, polled by the shared
 *  service store) is piped into `setStatus`, and every status surface
 *  shows the services it restarts as "restarting…". The result's
 *  message (and manual commands from a failed gateway restart) are
 *  rendered at the end. The plan argument is kept for the callers'
 *  signature (banner, plan card): the server reads its own copy.
 *  Exported for the app shell's global activation banner. */
export async function applyActivationPlan(
  _plan: ActivationPlanData,
  mode: "quiesce" | "restart" | "start",
  setStatus: (text: string, err?: boolean) => void,
  setManual?: (cmd: string | null) => void,
): Promise<boolean> {
  setManual?.(null);
  setStatus(mode === "quiesce"
    ? "Applying the activation plan — draining in-flight gateway requests first…"
    : "Applying the activation plan…");
  let lastProgress = "";
  const unsubscribe = serviceStore.subscribe((snap) => {
    const msg = snap.gateway?.activation_applying?.message ?? "";
    if (msg && msg !== lastProgress) {
      lastProgress = msg;
      setStatus(msg);
    }
  });
  let r: Awaited<ReturnType<typeof activationApply>>;
  try {
    r = await activationApply(mode, "desktop");
  } finally {
    unsubscribe();
  }
  void serviceStore.refresh();
  if (r.data.busy) {
    setStatus(r.error ?? "Another activation apply is already running — wait for it to finish.", true);
    return false;
  }
  const ok = r.data.ok;
  const message = r.data.message
    || (ok ? "Activation plan applied — the saved changes are live." : `Activation apply failed${r.error ? `: ${r.error}` : "."}`);
  setStatus(message, !ok);
  if (r.data.manual_command) setManual?.(r.data.manual_command);
  return ok;
}

/** Shared click handling for the plan card's buttons, used by the
 *  Backends and Gateway views. `repaint` re-renders the card in
 *  place; `reload` refreshes the whole view afterwards. */
async function handlePlanAction(
  act: string,
  ui: PlanUiState,
  repaint: () => void,
  reload: () => Promise<void>,
  plan: ActivationPlanData | null,
): Promise<void> {
  if (act === "review") { ui.dismissed = false; repaint(); return; }
  if (act === "copy-manual") {
    if (!ui.manualCommand) return;
    try {
      await navigator.clipboard.writeText(ui.manualCommand);
      ui.status = "Manual commands copied to the clipboard.";
      ui.statusErr = false;
    } catch {
      ui.status = "Copy failed — select the commands in the block and copy them manually.";
      ui.statusErr = true;
    }
    repaint();
    return;
  }
  if (act === "later") {
    ui.dismissed = true; ui.confirmCancel = false; ui.status = "";
    repaint();
    return;
  }
  if (act === "cancel") { ui.confirmCancel = true; repaint(); return; }
  if (ui.acting) return;
  if (act === "cancel-confirm") {
    ui.acting = true;
    repaint();
    const r = await activationCancelChanges();
    ui.acting = false;
    ui.confirmCancel = false;
    if (r.error) {
      ui.status = `Cancel failed: ${r.error}`;
      ui.statusErr = true;
      repaint();
      return;
    }
    ui.status = r.data.cancelled
      ? "Changes cancelled — the previous configuration was restored."
      : "Nothing to cancel — the plan had already resolved.";
    ui.statusErr = false;
    repaint();
    await reload();
    return;
  }
  if (act === "quiesce" || act === "restart" || act === "start") {
    if (!plan) return;
    ui.acting = true;
    const setStatus = (text: string, err = false): void => {
      ui.status = text;
      ui.statusErr = err;
      repaint();
    };
    const setManual = (cmd: string | null): void => {
      ui.manualCommand = cmd ?? "";
      repaint();
    };
    await applyActivationPlan(plan, act, setStatus, setManual);
    ui.acting = false;
    repaint();
    await reload();
  }
}

/** The dedicated Activation Plan view: one section — the shared
 * plan card in its full variant (count tiles, scope bar, Apply /
 * Cancel actions, dismiss X) over the Pending changes detail
 * (gateway/drift chips, activation-class bar, and the changes
 * table). The global banner's Review button navigates here. */
export function renderActivationPlan(): string {
  return `${topbar("Activation Plan")}
  <div class="view"><div id="activationBody">${loadingHtml("Loading activation plan…")}</div></div>`;
}

/** Display label for a journal change's structured `kind` — the
 * kinds the backend writes (src-tauri's record_*_change call sites).
 * Unknown kinds fall back to a humanized form of the raw string. */
const PLAN_KIND_LABEL: Record<string, string> = {
  gateway_config: "Gateway config",
  gateway_bind: "Bind address",
  gateway_drift: "Gateway drift",
  backend_apply_config: "Backend config",
  backend_add: "Backend added",
  backend_flags: "Flags",
  backend_set_offload: "Offload",
  backend_set_mtp: "MTP draft",
  backend_set_raw_flags: "Raw flags",
  backend_delete: "Backend removed",
  backend_placement: "Placement",
  model_assign: "Model assignment",
};

function planKindLabel(kind: string): string {
  return PLAN_KIND_LABEL[kind] ?? (kind ? kind.replace(/_/g, " ").replace(/^\w/, (ch) => ch.toUpperCase()) : "Change");
}

/** The Change cell: the structured kind as the label and the value
 * core of the journal summary as the value. The summary's leading
 * "Backend `id` " prefix is dropped (the Target column carries it);
 * the offload and "changed to" shapes reduce to their value core.
 * When the core adds nothing beyond the label, the label stands
 * alone. The full journal sentence stays on the cell's tooltip. */
function planChangeCell(c: ActivationChange): string {
  const label = planKindLabel(c.kind);
  let core = c.summary.replace(/^Backend `[^`]*` /, "");
  const off = /expert offload set to (.+)$/.exec(core);
  const mtpSet = /MTP draft set to (.+)$/.exec(core);
  if (off) core = off[1];
  else if (mtpSet) core = mtpSet[1];
  else if (/^MTP draft turned off$/.test(core)) core = "";
  else if (/^raw flags changed$/.test(core)) core = "";
  else if (core.includes(" changed to ")) core = core.split(" changed to ").pop() ?? core;
  else if (/^gateway configuration changed$/i.test(core)) core = "";
  const redundant = !core || label.toLowerCase().includes(core.toLowerCase());
  return `<span title="${esc(c.summary)}">${esc(label)}${redundant ? "" : `: ${esc(core)}`}</span>`;
}

/** The Target cell: scope absorbed as a Label: Value ("Backend:
 * coder" / "Gateway"). A long target list is capped in the cell;
 * the full list rides the tooltip. */
function planTargetCell(c: ActivationChange): string {
  if (c.scope !== "backend") return "Gateway";
  const targets = c.targets ?? [];
  if (!targets.length) return "Backend";
  const shown = targets.slice(0, 2).join(", ") + (targets.length > 2 ? ` +${targets.length - 2}` : "");
  return `<span title="Backend: ${esc(targets.join(", "))}">Backend: ${esc(shown)}</span>`;
}

export async function wireActivationPlan(root: HTMLElement): Promise<() => void> {
  const planUi = newPlanUi();
  let lastPlan: ActivationPlanData | null = null;
  /** The last fetch error. A failed fetch must never paint the
   * all-clear: the view keeps the last good plan and surfaces the
   * error, or reports the plan as unavailable when none was ever
   * loaded (principle 12 — unknown is not "nothing pending"). */
  let lastError: string | null = null;
  /** Recorded executions (newest first) and their fetch state,
   * tracked independently of the plan: a history failure renders
   * in the history card only and never disturbs the plan above
   * it. Details load on first expansion and stay cached for the
   * view's lifetime (an execution record never changes). */
  let history: ActivationExecutionSummary[] | null = null;
  let historyError: string | null = null;
  const expandedExec = new Set<string>();
  const execDetails = new Map<string, ActivationExecution | null>();
  const execDetailErrors = new Map<string, string>();
  let disposed = false;
  let timer: number | null = null;

  const execStatusChip = (status: string): string => {
    if (status === "completed") return `<span class="chip status-tag status-on">Completed</span>`;
    if (status === "completed_with_failures") return `<span class="chip status-tag status-transition">Completed with failures</span>`;
    if (status === "failed") return `<span class="chip status-tag status-bad">Failed</span>`;
    return `<span class="chip status-tag status-off">${esc(status)}</span>`;
  };

  const execOutcomeChip = (outcome: string): string => {
    if (outcome === "applied") return `<span class="chip status-tag status-on">Applied</span>`;
    if (outcome === "failed") return `<span class="chip status-tag status-bad">Failed</span>`;
    return `<span class="chip status-tag status-off">Skipped</span>`;
  };

  const execDetailHtml = (id: string): string => {
    if (!execDetails.has(id)) {
      return `<div style="font-size:12px;color:var(--text2);margin-top:6px">Loading execution detail…</div>`;
    }
    const detail = execDetails.get(id);
    if (!detail) {
      return `<div style="font-size:12px;color:var(--err);margin-top:6px">${esc(execDetailErrors.get(id) ?? "Execution detail unavailable")}</div>`;
    }
    const modeLabel = detail.mode === "quiesce" || detail.mode === "drain" ? "Drain and apply"
      : detail.mode === "restart" || detail.mode === "now" ? "Apply now"
      : detail.mode === "start" ? "Apply plan" : (detail.mode ?? "");
    const meta = `<div style="font-size:12px;color:var(--text2);margin-top:6px">Mode: <b style="color:var(--text)">${esc(modeLabel)}</b> · Source: <b style="color:var(--text)">${esc(detail.source)}</b> · Gateway: ${lifecycleChip(detail.gateway_up ? "running" : "stopped", "The gateway's state when this apply started")}${detail.drift ? ` · Gateway config: <b style="color:var(--text)">drifted</b>` : ""}</div>`;
    if (!detail.items.length) {
      return `${meta}<div style="font-size:12px;color:var(--text2);margin-top:6px">No plan changes — the apply addressed gateway drift only.</div>`;
    }
    const rows = detail.items.map((i) => `<tr><td>${esc(fmtDate(i.ts))} ${esc(fmtTime(i.ts))}</td><td>${planTargetCell(i)}</td><td>${planChangeCell(i)}</td><td>${execOutcomeChip(i.outcome)}</td><td>${i.detail ? esc(i.detail) : ""}</td></tr>`).join("");
    return `${meta}<table class="dtab" style="margin-top:8px"><thead><tr><th>Saved</th><th>Target</th><th>Change</th><th>Outcome</th><th>Detail</th></tr></thead><tbody>${rows}</tbody></table>`;
  };

  const execRowHtml = (s: ActivationExecutionSummary): string => {
    const open = expandedExec.has(s.id);
    return `<div style="border-top:1px solid var(--border);padding:8px 0">
      <div style="display:flex;align-items:center;gap:10px;flex-wrap:wrap;font-size:12px">
        <span>${esc(fmtDate(s.started_at))} ${esc(fmtTime(s.started_at))}</span>
        ${execStatusChip(s.status)}
        <span>Applied: <b>${fmtCount(s.applied)}</b></span>
        <span>Skipped: <b>${fmtCount(s.skipped)}</b></span>
        <span>Failed: <b>${fmtCount(s.failed)}</b></span>
        <span class="spacer" style="flex:1"></span>
        <button class="btn icon-only" data-exec-toggle="${esc(s.id)}" title="${open ? "Hide" : "Show"} execution detail" aria-label="Toggle execution detail">${icon(open ? "x" : "eye")}</button>
      </div>
      ${open ? execDetailHtml(s.id) : ""}
    </div>`;
  };

  /** The Recent executions card: durable apply history under the
   * plan — summary rows first (time, status, outcome counts),
   * the recorded plan contents on demand per row. */
  const historySectionHtml = (): string => {
    let body: string;
    if (!history) {
      body = historyError
        ? `<div style="font-size:12px;color:var(--err);margin-top:6px">${esc(historyError)}</div>`
        : `<div style="font-size:12px;color:var(--text2);margin-top:6px">Loading execution history…</div>`;
    } else if (!history.length) {
      body = `<div style="font-size:12px;color:var(--text2);margin-top:6px">No executions recorded</div>`;
    } else {
      body = history.map(execRowHtml).join("");
    }
    return `<div class="card" style="margin-top:12px"><h3>${icon("clock")} Recent executions</h3>${body}</div>`;
  };

  const paint = (): void => {
    const b = root.querySelector("#activationBody");
    if (!b) return;
    if (!lastPlan && lastError) {
      b.innerHTML = `<div class="card"><h3>${icon("warn")} Activation plan: unavailable</h3>
        <div style="font-size:12px;color:var(--err);margin-top:6px">${esc(lastError)}</div>
        ${planStatusHtml(planUi)}
        <div class="card-actions"><button class="btn icon-only" data-plan-refresh title="Refresh" aria-label="Refresh">${icon("restart")}</button></div></div>`
        + historySectionHtml();
      return;
    }
    if (!lastPlan || !lastPlan.pending) {
      b.innerHTML = `<div class="card"><h3>${icon("check")} No activation plan pending<span class="info-tip" title="Every saved configuration change is live. A change that needs a restart to take effect — gateway routes, a backend's flags, tensor split, or model assignment — appears here, and in the banner at the top of the window, until it is applied.">${icon("info")}</span></h3>
        <div style="font-size:12px;color:var(--text2);margin-top:6px">Pending changes: <b style="color:var(--text)">0</b></div>
        ${lastPlan && lastPlan.files_differ && !lastPlan.gateway_up ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">Gateway config files on disk differ from the saved configuration — the next gateway start regenerates them, so nothing is pending.</div>` : ""}
        ${planStatusHtml(planUi)}
        <div class="card-actions"><button class="btn icon-only" data-plan-refresh title="Refresh" aria-label="Refresh">${icon("restart")}</button></div></div>`
        + historySectionHtml();
      return;
    }
    const p = lastPlan;
    const errNote = lastError
      ? `<div class="banner" style="margin-bottom:12px">${icon("warn")} Activation plan refresh failed: ${esc(lastError)} — showing the last loaded plan.</div>`
      : "";
    // The Pending changes detail: header (gateway state + drift as
    // chips), the activation-class bar, then the table. Refresh
    // lives on the plan card above — one Refresh for the section.
    // Drift arrives as a synthesized gateway_drift change item
    // (tracker #57), so a drift-only plan lists it in the table
    // like any journal change.
    // The gateway's live lifecycle state from the shared store (a
    // restart reads "restarting…", never "stopped"); the plan's own
    // probe is the fallback before the store's first snapshot.
    const gwState: LifecycleState = serviceStore.gatewayState() ?? (p.gateway_up ? "running" : "stopped");
    const gwChip = `<span style="font-size:12px;color:var(--text2);font-weight:400">Gateway:</span> ${lifecycleChip(gwState, { transition: serviceStore.transitionOf("gateway", serviceStore.get().gateway?.transition ?? null) })}`;
    const driftChip = p.drift
      ? ` <span class="chip status-tag status-transition" title="The saved gateway configuration differs from the running one (drift).">Drift</span>`
      : "";
    let detailBody = "";
    if (p.changes.length) {
      const n = p.changes.length;
      const bar = stackedBar(p.changes.map((c) => ({
        pct: 100 / n,
        color: c.activation === "next_start" ? "#6d7686" : C.amber,
        label: `${planChangeMeta(c)} — ${c.summary}`,
      })), 600, 8);
      const rows = p.changes.map((c) => `<tr><td>${esc(fmtDate(c.ts))} ${esc(fmtTime(c.ts))}</td><td>${planTargetCell(c)}</td><td>${planChangeCell(c)}</td><td>${c.activation === "next_start"
        ? `<span class="chip status-tag status-off">Next start</span>`
        : `<span class="chip status-tag status-transition">Restart</span>`}</td></tr>`).join("");
      detailBody = `<div style="margin-top:10px">${bar}
        <div class="legend" style="margin-top:4px"><span><span class="sw" style="background:${C.amber}"></span>Restart</span><span><span class="sw" style="background:#6d7686"></span>Next start</span></div></div>
      <table class="dtab" style="margin-top:8px"><thead><tr><th>Saved</th><th>Target</th><th>Change</th><th>Activation</th></tr></thead><tbody>${rows}</tbody></table>`;
    }
    const detail = `<div class="card"><h3>${icon("logs")} Pending changes <span class="spacer" style="flex:1"></span>${gwChip}${driftChip}</h3>${detailBody}</div>`;
    b.innerHTML = errNote + activationPlanHtml(p, planUi, "full") + detail + historySectionHtml();
  };

  const load = async (): Promise<void> => {
    const [r, h] = await Promise.all([getActivationPlan(), getActivationHistory()]);
    if (disposed) return;
    if (r.error) {
      // Keep the last good plan; the error surfaces in paint.
      lastError = r.error;
    } else {
      lastError = null;
      lastPlan = r.data;
    }
    if (h.error) {
      // Same discipline for the history: keep the last good list.
      historyError = h.error;
    } else {
      historyError = null;
      history = h.data.executions;
    }
    paint();
  };

  const onClick = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLElement;
    if (t.closest("[data-plan-refresh]")) { await load(); return; }
    const et = t.closest("[data-exec-toggle]") as HTMLElement | null;
    if (et) {
      const id = et.getAttribute("data-exec-toggle") ?? "";
      if (expandedExec.has(id)) {
        expandedExec.delete(id);
        paint();
        return;
      }
      expandedExec.add(id);
      paint();
      if (!execDetails.has(id)) {
        const d = await getActivationExecution(id);
        if (disposed) return;
        if (d.error) {
          execDetails.set(id, null);
          execDetailErrors.set(id, d.error);
        } else {
          execDetails.set(id, d.data.execution);
          if (!d.data.execution) {
            execDetailErrors.set(id, "This execution is no longer in the retained history");
          }
        }
        paint();
      }
      return;
    }
    const pt = t.closest("[data-plan-act]") as HTMLElement | null;
    if (!pt) return;
    await handlePlanAction(pt.getAttribute("data-plan-act") ?? "", planUi, paint, load, lastPlan);
  };
  root.addEventListener("click", onClick);
  timer = window.setInterval(() => { void load(); }, 10000);
  await load();
  // Gateway lifecycle and apply progress come from the shared store:
  // repaint when the gateway state or the server-side apply changes
  // (an apply started on another surface disables Apply here too),
  // and reload the plan when an apply finishes.
  let lcSig = "";
  let wasApplying = false;
  const unsubscribe = serviceStore.subscribe((snap) => {
    if (disposed) return;
    const applying = Boolean(snap.gateway?.activation_applying);
    const sig = `${serviceStore.gatewayState() ?? ""}|${snap.gateway?.activation_applying?.message ?? ""}`;
    if (wasApplying && !applying) void load();
    wasApplying = applying;
    if (sig !== lcSig) { lcSig = sig; paint(); }
  });

  return () => {
    disposed = true;
    unsubscribe();
    if (timer !== null) window.clearInterval(timer);
    root.removeEventListener("click", onClick);
  };
}

/** Context-length presets offered by the flags editor (tokens). */
const CTX_PRESETS = [8192, 16384, 32768, 65536, 131072, 262144, 524288, 1048576];
/** Batch-size presets offered by the flags editor. */
const BATCH_PRESETS = [128, 256, 512, 1024, 2048, 4096];
/** Parallel-slot presets offered by the flags editor (tracker #61). */
const PARALLEL_PRESETS = [1, 2, 4, 8];
/** The Parallel slots control's range — the same bounds
 *  backend_set_parallel validates (manager_models::parallel). */
const PARALLEL_MIN = 1;
const PARALLEL_MAX = 1024;
/** The slot count llama.cpp's `--parallel` default (auto, -1)
 *  initializes on the pinned build, banner-verified — see
 *  manager_models::parallel. The unset field state names it so
 *  the effective count is visible rather than a bare "auto", and
 *  it seeds the Custom input when the operator leaves the
 *  default to type a count. */
const PARALLEL_LLAMA_AUTO_SLOTS = 4;

/** 131072 -> "128K"; values under 1024 stay plain. The quotient
 *  renders through trim1: a non-power-of-two context (10000)
 *  reads "9.8K", never the raw 9.765625K. */
function ctxShort(n: number): string {
  return n >= 1024 ? `${trim1(n / 1024)}K` : String(n);
}

/** Expert-offload weight figures for a slider position, priced
 *  with the same byte arithmetic as the backend memory requirement
 *  (manager_models::memory_requirement): the offloaded share of
 *  the routed experts sits in system RAM, the remaining weights
 *  on the GPU. Weights only — the KV cache is offload-independent
 *  and shown on the card. */
function offloadFigures(o: BackendOffload, layers: number): { gpuMib: number; sysMib: number } {
  // Prefer the backend's true per-block pricing for this position
  // (UD quants price blocks unevenly, so a uniform share disagrees
  // with the backend card's RAM figures at partial positions); the
  // uniform share is the fallback for older payloads without it.
  const priced = Array.isArray(o.sys_bytes_by_position) ? o.sys_bytes_by_position[layers] : undefined;
  const sysBytes = typeof priced === "number" && Number.isFinite(priced)
    ? priced
    : Math.floor((o.expert_bytes * layers) / o.moe_layers);
  const mib = (bytes: number): number => Math.ceil(bytes / 1048576);
  return { gpuMib: mib(o.total_bytes - sysBytes), sysMib: mib(sysBytes) };
}

/** True when a slider position is outside the guardrail's verified
 *  set: the guardrail allows endpoints only and the position is
 *  neither endpoint. Such a position is untested on this GPU;
 *  saving it requires an explicit override. */
function offloadIsUntested(o: BackendOffload, layers: number): boolean {
  return o.guardrail?.allowed === "endpoints_only" && layers !== 0 && layers !== o.moe_layers;
}

/** The offload slider's layers label, naming the untested state
 *  when the (draft) position is outside the verified set. */
function offloadLayersLabel(o: BackendOffload, layers: number): string {
  return offloadIsUntested(o, layers)
    ? `${layers} of ${o.moe_layers} MoE layers \u2014 untested on this GPU`
    : `${layers} of ${o.moe_layers} MoE layers`;
}

/** The offload slider's info-tip text. An untested draft position
 *  gains a sentence naming the GPU architecture it is unverified
 *  on and the override rule for saving it. */
function offloadTipText(o: BackendOffload, layers: number): string {
  const base = "Routed-expert weights for the first N MoE layers run from system RAM instead of the GPU, freeing GPU memory at some speed cost. Green is this model's catalog default \u2014 the most offload that stays fast; past it, CPU expert computation increasingly dominates generation speed. Figures are weights only; the KV cache stays on the GPU. The slider saves when released and applies on the backend's next start.";
  if (!offloadIsUntested(o, layers) || !o.guardrail) return base;
  return `${base} This position is outside the range verified for this model's GPU architecture (compute capability ${o.guardrail.compute_capability}): partial offload positions are not verified there, and saving requires an explicit override while starts without one are refused.`;
}

/** The flag tokens a slider position writes — the canonical forms
 *  the app itself writes (backend_set_offload). */
function offloadTokenText(o: BackendOffload, layers: number): string {
  if (layers <= 0) return "no offload tokens";
  if (layers >= o.moe_layers) return "-ot exps=CPU";
  // The position counts MoE layers; the flag value is the backend's
  // block-index bound for it (they differ past leading dense blocks).
  return `--n-cpu-moe ${o.n_cpu_moe_by_position?.[layers] ?? layers}`;
}

/* ---------------- MTP draft control (tracker #53) ----------------
 * State arrives in the list_backends payload (`BackendMtp`,
 * decoded server-side from the backend's saved raw flags) and
 * commits go through backend_set_mtp, which rewrites only the
 * MTP tokens. Values are 0..4, where 0 is Off. The control is a
 * dropdown in the ctx/batch selects' idiom (Pete, Oct 18, 2026:
 * the original range slider showed neither its stops nor that
 * it was a slider until clicked). */
/** llama.cpp's `--spec-draft-n-max` default, used to place the
 *  control for a backend that enables MTP without an explicit
 *  length (and named in that state's readout). */
const MTP_LLAMA_N_MAX_DEFAULT = 3;

/** The values the dropdown offers: 0 is Off, 1..4 are
 *  `--spec-draft-n-max` draft lengths. */
const MTP_POSITIONS = [0, 1, 2, 3, 4];

/** The control value a saved MTP state implies: 0 when off;
 *  the explicit length when set; llama.cpp's default when MTP is
 *  on without one. */
function mtpPosition(m: BackendMtp): number {
  if (!m.enabled) return 0;
  return m.n_max ?? MTP_LLAMA_N_MAX_DEFAULT;
}

/** The flag form a chosen value writes (the readout once a
 *  value is chosen, and the saved message after a commit). */
function mtpTokenText(pos: number): string {
  return pos === 0 ? "Speculative decoding off" : `--spec-type draft-mtp --spec-draft-n-max ${pos}`;
}

/** The readout for the SAVED state, before any change: a backend
 *  that enables MTP without an explicit length runs at
 *  llama.cpp's default, and the readout says so rather than
 *  implying a saved value. */
function mtpSavedTokenText(m: BackendMtp): string {
  if (!m.enabled) return "Speculative decoding off";
  if (m.n_max == null) return `--spec-type draft-mtp (draft length: llama.cpp default, ${MTP_LLAMA_N_MAX_DEFAULT})`;
  return mtpTokenText(m.n_max);
}

/** The expert-cache size presets the control offers (MiB) —
 *  the server module's MOE_CACHE_PRESETS_MIB. */
const MOE_CACHE_PRESETS_MIB = [2048, 4096, 6144, 8192, 10240];

/** The size a fresh enable commits when nothing is saved or
 *  drafted — the server module's MOE_CACHE_DEFAULT_MIB. */
const MOE_CACHE_DEFAULT_MIB = 4096;

/** The flag form a chosen state writes (the readout once a
 *  value is chosen, and the saved message after a commit). */
function moeCacheTokenText(mib: number | null): string {
  return mib == null ? "Expert cache off" : `--moe-cache-mib ${mib}`;
}

/** The readout for the SAVED state, before any change. */
function moeCacheSavedTokenText(m: BackendMoeCache): string {
  return moeCacheTokenText(m.mib);
}

/* ---------------- Predicted throughput (design §6, §7) -----
 * One line shape for the flags forms (Edit Flags and Add
 * Backend) and one tile for the backend card, all fed by the
 * same resolved-measurement shape (api.ts PredictedThroughput),
 * so a figure reads identically on every surface. */

/** "Oct 14" — plus the year when it is not the current year
 *  (or always, when `alwaysYear` — the card tile's tooltip,
 *  §7). Unparseable input renders as the raw string. */
const MONTHS_SHORT = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
function fmtMeasuredDate(iso: string, alwaysYear: boolean): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const base = `${MONTHS_SHORT[d.getMonth()]} ${d.getDate()}`;
  return alwaysYear || d.getFullYear() !== new Date().getFullYear()
    ? `${base}, ${d.getFullYear()}`
    : base;
}

/** The source phrase a tip names (§5.5 disclosure). A null
 *  source is a v1 entry: the absence is stated, not filled in. */
function predictedSourceLabel(source: string | null): string {
  switch (source) {
    case "benchmark": return "Benchmark";
    case "experiment": return "Experiment run";
    case "live": return "Live observation";
    default: return "Source not recorded";
  }
}

/** Placement in short form from an inputs snapshot: the GPU
 *  names, "CPU only" for an empty GPU set, context and batch. */
function predictedPlacementText(p: PredictedThroughput): string | null {
  const inp = p.inputs;
  if (!inp) return null;
  const gpus = inp.gpus.length
    ? inp.gpus.map((g) => g.split(":").slice(1).join(":") || g).join(", ")
    : "CPU only";
  return `Context ${fmtCount(inp.n_ctx)} · Batch ${fmtCount(inp.n_batch)} · ${gpus}`;
}

/** The form line's info-tip (§6.1): what the figures are, when
 *  and how they were measured, the placement, and the
 *  environment (§5.5). */
function predictedFormTip(p: PredictedThroughput): string {
  const d = new Date(p.measured_at);
  const full = Number.isNaN(d.getTime())
    ? p.measured_at
    : `${MONTHS_SHORT[d.getMonth()]} ${d.getDate()}, ${d.getFullYear()}`;
  const sourcePhrase = p.source === "benchmark"
    ? "by the standardized benchmark (the Optimize page's Measure action)"
    : p.source === "experiment"
      ? "by an Optimize experiment run"
      : p.source === "live"
        ? "from live gateway traffic"
        : "before measurement sources were recorded (an early entry)";
  const placement = predictedPlacementText(p);
  const env = [`LLMdil ${p.app_version}`];
  if (p.driver_version.length) env.push(`driver ${p.driver_version.join(", ")}`);
  if (p.llama_build) env.push(`llama.cpp ${p.llama_build}`);
  return `Measured ${full} ${sourcePhrase}. `
    + (placement ? `Placement: ${placement}. ` : "Placement: not recorded for this entry. ")
    + "Prompt processing comes from the 4,000-token prompt; generation and time to first token from the 200-token prompt. "
    + `Environment: ${env.join(" · ")}.`;
}

/** The card tile's tooltip (§7): prompt rate, TTFT, measured
 *  date, source, placement, and environment on one line.
 *  Exported for the Groups page's compact cards (§8.2), which
 *  carry the same figure under the same rules. */
export function predictedCardTip(p: PredictedThroughput): string {
  const parts: string[] = [];
  if (p.prompt_per_second !== null) parts.push(`Prompt: ${trim1(p.prompt_per_second)} tok/s`);
  if (p.ttft_ms !== null) parts.push(`TTFT: ${trim1(p.ttft_ms / 1000)} s`);
  parts.push(`Measured: ${fmtMeasuredDate(p.measured_at, true)}`);
  parts.push(predictedSourceLabel(p.source));
  const placement = predictedPlacementText(p);
  if (placement) parts.push(placement);
  if (p.driver_version.length) parts.push(`Driver: ${p.driver_version.join(", ")}`);
  if (p.llama_build) parts.push(`llama.cpp ${p.llama_build}`);
  parts.push(`LLMdil ${p.app_version}`);
  return parts.join(" · ");
}

/** The flags form's predicted line for one lookup result
 *  (design §6.1 exact, §6.3 near): the exact line when an exact
 *  match exists, else the muted Similar measurement line with
 *  the differing fields named, else nothing at all (§5.4 —
 *  empty renders nothing). */
function predictedLineHtml(exact: PredictedThroughput | null, near: PredictedThroughput | null): string {
  const figures = (p: PredictedThroughput): string => {
    const parts: string[] = [];
    if (p.predicted_per_second !== null) parts.push(`${trim1(p.predicted_per_second)} tok/s generation`);
    if (p.prompt_per_second !== null) parts.push(`${trim1(p.prompt_per_second)} prompt`);
    return parts.join(" · ");
  };
  if (exact && figures(exact)) {
    return `<div style="font-size:12px;color:var(--text2);margin-top:6px">Predicted throughput: ${esc(figures(exact))} \u2014 measured ${esc(fmtMeasuredDate(exact.measured_at, false))}<span class="info-tip" title="${esc(predictedFormTip(exact))}">${icon("info")}</span></div>`;
  }
  if (near && figures(near)) {
    const differs = (near.differs ?? []).join(", ");
    return `<div style="font-size:12px;color:var(--text2);margin-top:6px">Similar measurement: ${esc(figures(near))}${differs ? ` \u2014 differs: ${esc(differs)}` : ""} \u2014 measured ${esc(fmtMeasuredDate(near.measured_at, false))}<span class="info-tip" title="${esc(predictedFormTip(near))}">${icon("info")}</span></div>`;
  }
  return "";
}

/** The --parallel conflict line's text. MTP drafting needs a
 *  single slot; the MTP control surfaces an explicit conflicting
 *  slot count and never rewrites it — the Parallel slots field
 *  in the same form is where the operator sets it (tracker #61),
 *  and the line says so. */
function mtpConflictText(parallel: number): string {
  return `Parallel slots: ${parallel} — MTP drafting needs --parallel 1; set Parallel slots to 1 in the Flags form — the next start will not draft until then`;
}

/* ---------------- Advanced flags validation (tracker #60) ----------------
 * A TypeScript mirror of manager-models' raw_flag_warnings, used
 * only to render the advanced editor's warnings live while the
 * operator types. The Rust function is the authority: the save op
 * returns its own warning list and the panel adopts that list
 * after saving. Keep the two in step — the flag set and the
 * warning wording come from the Rust module.
 *
 * Warnings never block: an unknown token or a malformed value is
 * passed to llama-server untouched and llama-server decides. */

export interface FlagWarningLike { token: string; message: string }

type FlagArity = { kind: "none" } | { kind: "value"; value: "int" | "any" } | { kind: "multi"; value: "int" | "any" };
interface FlagSpecLike { canonical: string; reserved: boolean; arity: FlagArity }

const V = (value: "int" | "any"): FlagArity => ({ kind: "value", value });
const FLAG_SPECS: ReadonlyArray<readonly [string, string, boolean, FlagArity]> = [
  ["--n-cpu-moe", "--n-cpu-moe", false, V("int")], ["-ncmoe", "--n-cpu-moe", false, V("int")],
  ["--cpu-moe", "--cpu-moe", false, { kind: "none" }], ["-cmoe", "--cpu-moe", false, { kind: "none" }],
  ["-ot", "--override-tensor", false, { kind: "multi", value: "any" }], ["--override-tensor", "--override-tensor", false, { kind: "multi", value: "any" }],
  ["--spec-type", "--spec-type", false, V("any")],
  ["--spec-draft-n-max", "--spec-draft-n-max", false, V("int")],
  ["--parallel", "--parallel", false, V("int")], ["-np", "--parallel", false, V("int")],
  ["--model", "--model", false, V("any")], ["-m", "--model", false, V("any")],
  ["--port", "--port", false, V("int")],
  ["--ctx-size", "--ctx-size", false, V("int")], ["-c", "--ctx-size", false, V("int")],
  ["--batch-size", "--batch-size", false, V("int")], ["-b", "--batch-size", false, V("int")],
  ["--tensor-split", "--tensor-split", false, V("any")], ["-ts", "--tensor-split", false, V("any")],
  ["--split-mode", "--split-mode", false, V("any")],
  ["--device", "--device", false, V("any")], ["-dev", "--device", false, V("any")],
  ["--main-gpu", "--main-gpu", false, V("int")],
  ["--gpu-layers", "--gpu-layers", false, V("int")], ["--n-gpu-layers", "--gpu-layers", false, V("int")], ["-ngl", "--gpu-layers", false, V("int")],
  ["--threads", "--threads", false, V("int")], ["-t", "--threads", false, V("int")],
  ["--jinja", "--jinja", false, { kind: "none" }],
  ["--host", "--host", true, V("any")],
  ["--no-jinja", "--no-jinja", true, { kind: "none" }], ["--no_jinja", "--no-jinja", true, { kind: "none" }],
];
const FLAG_SPEC_MAP: ReadonlyMap<string, FlagSpecLike> = new Map(FLAG_SPECS.map(([name, canonical, reserved, arity]) => [name, { canonical, reserved, arity }]));

function flagSpecFor(token: string): FlagSpecLike | undefined {
  if (token === "--jinja=false") return FLAG_SPEC_MAP.get("--no-jinja");
  return FLAG_SPEC_MAP.get(token);
}

const isIntText = (s: string): boolean => /^[+-]?\d+$/.test(s);

/** Validate raw flags text the way the save op will. Same
 *  whitespace tokenization the flag decoders use. */
export function rawFlagWarnings(raw: string): FlagWarningLike[] {
  const tokens = raw.split(/\s+/).filter((t) => t !== "");
  const out: FlagWarningLike[] = [];
  const seen = new Map<string, string>();
  const checkInt = (flagTok: string, value: string): void => {
    if (!isIntText(value)) out.push({ token: flagTok, message: `${flagTok} expects a whole number; "${value}" will make llama-server reject the flag` });
  };
  const noteSeen = (flagTok: string, spec: FlagSpecLike): void => {
    if (spec.arity.kind === "multi") return;
    if (seen.has(spec.canonical)) out.push({ token: flagTok, message: `${spec.canonical} is set more than once (${flagTok}); only the last value is used` });
    else seen.set(spec.canonical, flagTok);
  };
  let i = 0;
  while (i < tokens.length) {
    const tok = tokens[i];
    if (!tok.startsWith("-")) {
      out.push({ token: tok, message: `"${tok}" is not part of a flag llama-server knows; it will reject the launch arguments` });
      i += 1;
      continue;
    }
    const eq = tok.indexOf("=");
    const name = eq >= 0 ? tok.slice(0, eq) : tok;
    const inline = eq >= 0 ? tok.slice(eq + 1) : null;
    const spec = flagSpecFor(name) ?? flagSpecFor(tok);
    if (!spec) {
      out.push({ token: name, message: `${name} is not a flag this app recognizes; it will be passed to llama-server unchanged` });
      if (inline === null && i + 1 < tokens.length && !tokens[i + 1].startsWith("-")) i += 1;
      i += 1;
      continue;
    }
    noteSeen(name, spec);
    if (spec.reserved) {
      out.push({ token: name, message: `${spec.canonical} is reserved: the app filters it out before launch, so it has no effect here` });
    }
    if (spec.arity.kind === "none") {
      if (inline !== null && !(name === "--jinja" && inline === "false")) {
        out.push({ token: name, message: `${name} takes no value; "=${inline}" will make llama-server reject the flag` });
      }
      i += 1;
      continue;
    }
    const valueKind = spec.arity.value;
    if (inline !== null) {
      if (valueKind === "int") checkInt(name, inline);
      i += 1;
      continue;
    }
    if (i + 1 >= tokens.length || tokens[i + 1].startsWith("-")) {
      out.push({ token: name, message: `${name} expects a value but none follows it` });
      i += 1;
      continue;
    }
    if (valueKind === "int") checkInt(name, tokens[i + 1]);
    i += 2;
  }
  return out;
}

/** The advanced editor's warning lines (amber, inline under the
 *  text box). Empty when the text is clean. */
function flagsRawWarningsHtml(warnings: FlagWarningLike[]): string {
  if (!warnings.length) return "";
  return warnings.map((w) => `<div style="font-size:12px;color:#e3b341;margin-top:4px">${icon("warn")} ${esc(w.message)}</div>`).join("");
}

/** The offload slider track's green-to-yellow-to-red gradient for
 *  a model's band boundaries (fractions of the offloaded range,
 *  computed by the backend from the catalog green limit). */
function offloadGradient(o: BackendOffload): string {
  const pct = (f: number): string => `${(f * 100).toFixed(1)}%`;
  return `linear-gradient(90deg, ${C.green} 0%, ${C.green} ${pct(o.green_end)}, ${C.amber} ${pct(o.green_end)}, ${C.amber} ${pct(o.yellow_end)}, ${C.red} ${pct(o.yellow_end)}, ${C.red} 100%)`;
}

/** The offload slider track's background. With an endpoints-only
 *  guardrail the interior of the track (between the two endpoint
 *  positions' half-step zones at each end) is unverified, so it
 *  renders muted and hatched rather than in the green/yellow/red
 *  band gradient: a hatch layer and a muted fill span the interior
 *  (a centered span, so background-position 50% places it exactly),
 *  with the normal band gradient retained underneath and visible
 *  only at the endpoint zones. Without a guardrail the track is
 *  exactly the band gradient. */
function offloadTrackBackground(o: BackendOffload): string {
  if (o.guardrail?.allowed !== "endpoints_only" || o.moe_layers <= 1) return offloadGradient(o);
  const interior = (((o.moe_layers - 1) / o.moe_layers) * 100).toFixed(1);
  const span = `center / ${interior}% 100% no-repeat`;
  const hatch = `repeating-linear-gradient(45deg, rgba(232,235,240,0.30) 0 2px, rgba(18,21,26,0.30) 2px 4px) ${span}`;
  const muted = `linear-gradient(90deg, #3a4350, #3a4350) ${span}`;
  return `${hatch}, ${muted}, ${offloadGradient(o)}`;
}

/** The context/batch field pair of the flags editor — preset
 *  selects with a Custom numeric input when the value is not a
 *  preset — shared between the Edit Flags form and the Add backend
 *  form (tracker #49): one implementation of the parameter
 *  fields, two hosts. `fieldAttr` is the data attribute the host's
 *  draft sync reads ("data-flags-field" in the edit form,
 *  "data-add-flags-field" in the add form); ctxMax annotates
 *  presets beyond the model's rated context. */
function flagsCtxBatchHtml(o: {
  ctx: number;
  batch: number;
  ctxMax: number | null;
  fieldAttr: string;
  disabled: boolean;
}): string {
  const dis = o.disabled ? " disabled" : "";
  return `<div style="display:grid;grid-template-columns:1fr 1fr;gap:10px;margin-top:8px">
        <label style="font-size:12px;color:var(--text2)">Context length (-c)
          <span style="display:flex;gap:6px;margin-top:4px">
            <select ${o.fieldAttr}="ctx-preset" style="${SELECT_STYLE};flex:1;min-width:0"${dis}>
              ${CTX_PRESETS.map((v) => `<option value="${v}"${v === o.ctx ? " selected" : ""}>${fmtCount(v)} (${ctxShort(v)})${v === 32768 ? " \u2014 default" : ""}${o.ctxMax && v > o.ctxMax ? " \u2014 beyond rated" : ""}</option>`).join("")}
              <option value="custom"${CTX_PRESETS.includes(o.ctx) ? "" : " selected"}>Custom\u2026</option>
            </select>
            ${CTX_PRESETS.includes(o.ctx) ? "" : `<input ${o.fieldAttr}="ctx-custom" type="number" value="${o.ctx}" min="1024" max="1048576" style="${SELECT_STYLE};width:110px"${dis}>`}
          </span></label>
        <label style="font-size:12px;color:var(--text2)">Batch size (-b)
          <span style="display:flex;gap:6px;margin-top:4px">
            <select ${o.fieldAttr}="batch-preset" style="${SELECT_STYLE};flex:1;min-width:0"${dis}>
              ${BATCH_PRESETS.map((v) => `<option value="${v}"${v === o.batch ? " selected" : ""}>${fmtCount(v)}${v === 512 ? " \u2014 default" : ""}</option>`).join("")}
              <option value="custom"${BATCH_PRESETS.includes(o.batch) ? "" : " selected"}>Custom\u2026</option>
            </select>
            ${BATCH_PRESETS.includes(o.batch) ? "" : `<input ${o.fieldAttr}="batch-custom" type="number" value="${o.batch}" min="1" style="${SELECT_STYLE};width:110px"${dis}>`}
          </span></label>
      </div>`;
}

/** The flag form the Parallel slots selection writes: the
 *  canonical token for an explicit count, or the default's
 *  description when unset. */
function parallelTokenText(value: number | null): string {
  if (value === null) return `llama.cpp default: auto (${PARALLEL_LLAMA_AUTO_SLOTS} slots)`;
  if (value === -1) return "--parallel -1 (auto)";
  return `--parallel ${value}`;
}

/** The parallel-slots field of the flags editor — a preset select
 *  with a Custom numeric input when the value is not a preset,
 *  in the ctx/batch idiom — shared between the Edit Flags form
 *  and the Add backend form (tracker #61): one implementation of
 *  the field, two hosts. The value is the explicit slot count,
 *  or null for the unset state: no `--parallel` token is saved
 *  and llama.cpp's default (auto) applies — the select's first
 *  choice carries that state, so unset is a visible selection,
 *  not an absence. `savedCustom` marks the saved spelling as one
 *  the control does not write (an `-np` or `=` spelling, ...),
 *  the MTP control's marker; the readout beside the field names
 *  what the current selection writes. */
function flagsParallelHtml(o: {
  value: number | null;
  savedCustom: boolean;
  fieldAttr: string;
  disabled: boolean;
}): string {
  const dis = o.disabled ? " disabled" : "";
  const v = o.value;
  const isCustom = v !== null && !PARALLEL_PRESETS.includes(v);
  return `<div style="display:grid;grid-template-columns:1fr 1fr;gap:10px;margin-top:8px;align-items:end">
        <label style="font-size:12px;color:var(--text2)">Parallel slots (--parallel)<span class="info-tip" title="Request slots the server runs concurrently; each slot is an independent sequence with its own share of the work. An explicit count divides the context length across the slots; Auto (llama.cpp's default) keeps one unified KV cache at the full context. 1 suits a single request stream, and MTP drafting needs 1.">${icon("info")}</span>
          <span style="display:flex;gap:6px;margin-top:4px">
            <select ${o.fieldAttr}="parallel-preset" style="${SELECT_STYLE};flex:1;min-width:0"${dis}>
              <option value="default"${v === null ? " selected" : ""}>Auto \u2014 llama.cpp default</option>
              ${PARALLEL_PRESETS.map((p) => `<option value="${p}"${p === v ? " selected" : ""}>${p}</option>`).join("")}
              <option value="custom"${isCustom ? " selected" : ""}>Custom\u2026</option>
            </select>
            ${isCustom ? `<input ${o.fieldAttr}="parallel-custom" type="number" value="${v}" min="${PARALLEL_MIN}" max="${PARALLEL_MAX}" style="${SELECT_STYLE};width:110px"${dis}>` : ""}
          </span></label>
        <div style="font-size:12px;color:var(--text2)"><span style="font-family:var(--mono)">${esc(parallelTokenText(v))}</span>${o.savedCustom ? `<span title="The saved flags spell parallel slots in a form the control does not write (an -np or = spelling, repeated tokens, ...). Saving from the field writes the canonical --parallel N; every other flag is preserved."> \u00b7 custom flags</span>` : ""}</div>
      </div>`;
}

/* ---------------- KV cache / flash attention ----------------
 * The cache controls: K and V cache type selects plus the
 * flash-attention tri-state, committing through
 * backend_set_cache (edit form) or carried by backend_add (add
 * form). State arrives decoded server-side (BackendCache /
 * CachePreview) — the TS side never parses raw flags for these.
 * The predicted footprint and the predicted-best ladder come
 * from the preview payload, priced server-side by the same
 * requirement model as the start gate. */
const CACHE_TYPES: CacheTypeName[] = ["f16", "q8_0", "q4_0"];
const FLASH_ATTN_MODES: FlashAttnName[] = ["auto", "on", "off"];

/** The flag tokens a cache selection writes — the canonical
 *  forms the app itself writes (backend_set_cache). */
function cacheTokenText(sel: CacheSelectionLike): string {
  return `--cache-type-k ${sel.k} --cache-type-v ${sel.v} --flash-attn ${sel.flash_attn}`;
}

/** Whether the selection is the pairing llama.cpp refuses: a
 *  quantized V cache with flash attention off. The set op
 *  refuses it too; the form names it before committing. */
function cacheVQuantizedFaOff(sel: CacheSelectionLike): boolean {
  return sel.v !== "f16" && sel.flash_attn === "off";
}

function sameCacheSelection(a: CacheSelectionLike, b: CacheSelectionLike): boolean {
  return a.k === b.k && a.v === b.v && a.flash_attn === b.flash_attn;
}

/** The predicted-footprint block under the cache controls: the
 *  current selection's footprint as structured Label: Value
 *  rows — Weights, KV cache and State cache (each present only
 *  when its figure exists; an unknown figure renders no row,
 *  never a zero), the emphasized Total, then one capacity row
 *  per placed GPU carrying that GPU's required share of its
 *  total VRAM and its verdict as a status chip — and below it
 *  the predicted-best rung: as a button when it differs from
 *  the current selection (the host wires the attribute), as
 *  text when it is the current selection, and as the honest
 *  no-fit statement (with the lowest rung's numbers) when no
 *  rung fits at the chosen context. Numbers only; explanation
 *  in the tooltip. Shared by the Edit Flags form and the Add
 *  Backend form (via flagsCacheHtml): one renderer, two hosts,
 *  so the forms cannot diverge. */
function cachePreviewHtml(preview: CachePreview | null, sel: CacheSelectionLike, bestAttr: string): string {
  if (!preview) return "";
  const gib = (mib: number): string => fmtGb(mib / 1024);
  const row = (label: string, value: string, extra = ""): string => `<div>${label}: ${value}${extra}</div>`;
  if (preview.kv_mib === null) {
    // KV geometry unknown: nothing is priced beyond the weights
    // — no KV row, no total, no capacity verdict (a total priced
    // without KV would understate the footprint, and a fit claim
    // on it would imply a measurement that does not exist).
    return `<div style="font-size:12px;color:var(--text2);margin-top:6px">${row("Weights", `${gib(preview.weights_mib)} GiB`, `<span class="info-tip" title="This model's KV geometry is not known, so no KV figure or predicted-best rung can be priced for it. The cache types still apply at launch.">${icon("info")}</span>`)}</div>`;
  }
  const rows = [
    row("Weights", `${gib(preview.weights_mib)} GiB`),
    row("KV cache", `${gib(preview.kv_mib)} GiB`),
  ];
  if (preview.rs_mib !== null && preview.rs_mib !== undefined) {
    rows.push(row("State cache", `${gib(preview.rs_mib)} GiB`));
  }
  // The expert cache is itemized whenever one is priced, so the
  // rows sum to the Total (before Oct 23, 2026 the cache rode the
  // total silently and the itemized rows fell short by exactly
  // its size).
  if (preview.moe_cache_mib) {
    rows.push(row("Expert cache", `${gib(preview.moe_cache_mib)} GiB`));
  }
  rows.push(`<div style="font-weight:600;color:var(--text)">Total: ${gib(preview.total_mib)} GiB<span class="info-tip" title="Predicted footprint of the current selection, priced by the same requirement model as the start gate: GPU weights at the current offload, KV at the selected cache types and the form's context, and the recurrent-state cache when the model carries one. A selection fits when its per-GPU total is within ${preview.headroom_reserve_percent}% headroom of the target's VRAM — the reserve covers the compute buffers the requirement model does not price. Predicted best is the highest-quality rung that fits: f16, then q8_0/q8_0, then q8_0/q4_0 (V degrades before K), then q4_0/q4_0, flash attention on at every rung.">${icon("info")}</span></div>`);
  for (const t of preview.targets ?? []) {
    // The capacity chip is the three-state classification
    // against total VRAM (the start gate's basis) when the
    // server carries it: a WDDM GPU over total is a paged fit
    // with its amount, never a plain "Does not fit" the gate
    // would allow on confirmation — and never a plain "Fits"
    // the gate would refuse.
    const chip = t.fit_state === "wddm_paged"
      ? ` <span class="chip status-tag status-transition">Fits — ${gib(t.paged_mib ?? 0)} GiB paged</span>`
      : t.fit_state === "does_not_fit"
        ? ` <span class="chip status-tag status-bad">Does not fit</span>`
        : t.fit_state === "fits"
          ? ` <span class="chip status-tag status-on">Fits</span>`
          : t.fits === null || t.fits === undefined
            ? ""
            : ` <span class="chip status-tag ${t.fits ? "status-on" : "status-bad"}">${t.fits ? "Fits" : "Does not fit"}</span>`;
    const value = t.required_mib === null || t.required_mib === undefined
      ? `${gib(t.total_mib)} GiB`
      : `${gib(t.required_mib)} GiB of ${gib(t.total_mib)} GiB`;
    rows.push(row(`GPU ${t.gpu}`, value, chip));
  }
  let bestHtml = "";
  const best = preview.best;
  if (best && !sameCacheSelection(best, sel)) {
    bestHtml = `<div style="margin-top:6px"><button class="btn" ${bestAttr}>${icon("check")} Use predicted best: ${esc(`${best.k}/${best.v} · FA ${best.flash_attn}`)} (${gib(best.total_mib)} GiB)</button></div>`;
  } else if (best) {
    bestHtml = `<div style="font-size:12px;color:var(--text2);margin-top:6px">Predicted best: this selection</div>`;
  } else if (preview.ladder.length) {
    const lowest = preview.ladder[preview.ladder.length - 1];
    bestHtml = `<div style="font-size:12px;color:#e3b341;margin-top:6px">No cache selection fits at this context — lowest: ${esc(`${lowest.k}/${lowest.v}`)} at ${gib(lowest.total_mib)} GiB</div>`;
  }
  return `<div style="font-size:12px;color:var(--text2);margin-top:6px">${rows.join("")}</div>${bestHtml}`;
}

/** The cache controls block — three selects in the ctx/batch
 *  idiom plus the token readout and the predicted-footprint
 *  slot — shared between the Edit Flags form and the Add backend
 *  form: one implementation, two hosts. `fieldAttr` is the data
 *  attribute the host's draft sync reads; the preview slot's
 *  content is rendered from `preview` for `sel` and re-rendered
 *  by the host as previews land. */
function flagsCacheHtml(o: {
  sel: CacheSelectionLike;
  savedCustom: boolean;
  savedPresent: boolean;
  preview: CachePreview | null;
  fieldAttr: string;
  bestAttr: string;
  disabled: boolean;
}): string {
  const dis = o.disabled ? " disabled" : "";
  const selHtml = (field: string, label: string, values: string[], cur: string, tip: string): string => `
        <label style="font-size:12px;color:var(--text2)">${label}<span class="info-tip" title="${esc(tip)}">${icon("info")}</span>
          <span style="display:flex;gap:6px;margin-top:4px">
            <select ${o.fieldAttr}="${field}" style="${SELECT_STYLE};flex:1;min-width:0"${dis}>
              ${values.map((v) => `<option value="${v}"${v === cur ? " selected" : ""}>${v}</option>`).join("")}
            </select>
          </span></label>`;
  return `<div style="margin-top:12px">
      <div style="display:grid;grid-template-columns:1fr 1fr 1fr;gap:10px">
        ${selHtml("cache-k", "K cache type", CACHE_TYPES, o.sel.k, "The element type the key cache is stored in. f16 is full quality; q8_0 stores each element in about half the space with minimal quality loss; q4_0 in about a quarter, with more. Applies on the backend's next start.")}
        ${selHtml("cache-v", "V cache type", CACHE_TYPES, o.sel.v, "The element type the value cache is stored in. A quantized V cache requires flash attention (auto promotes to on; off is refused by llama.cpp and by this form).")}
        ${selHtml("flash-attn", "Flash attention", FLASH_ATTN_MODES, o.sel.flash_attn, "Flash attention computes attention in tiles, cutting memory traffic. Auto lets llama.cpp decide (and promotes to on when a quantized V cache or a tensor split requires it); on forces it; off disables it. On the pinned build it is supported on every GPU the app targets, Pascal included (generic tile kernel).")}
      </div>
      <div style="font-size:12px;color:var(--text2);margin-top:4px"><span style="font-family:var(--mono)">${esc(cacheTokenText(o.sel))}</span>${o.savedCustom ? `<span title="The saved flags spell the cache settings in a form the controls do not write (a -ctk or = spelling, a partial selection, ...). Saving from these controls writes the canonical tokens; every other flag is preserved."> · custom flags</span>` : ""}${o.savedPresent ? "" : `<span title="No cache tokens are saved for this backend: it runs at llama.cpp's defaults (f16/f16, flash attention auto), exactly as before these controls existed. Saving from these controls writes the tokens."> · llama.cpp defaults (no tokens saved)</span>`}</div>
      <div data-cache-slot>${cachePreviewHtml(o.preview, o.sel, o.bestAttr)}</div>
    </div>`;
}

/** The rated-context line under the flags fields, shared by the
 *  Edit Flags form and the Add backend form. `savedText` is the
 *  host's sentence for when the values take effect (the edit form
 *  saves to the configuration immediately; the add form saves at
 *  creation). */
function flagsRatedLineHtml(o: {
  ctxMax: number | null;
  modelId: string | null;
  savedText: string;
}): string {
  return `<div style="font-size:12px;color:var(--text2);margin-top:6px">${o.modelId && o.ctxMax
    ? `Rated context: ${fmtCount(o.ctxMax)}<span class="info-tip" title="${esc(`Model ${o.modelId} is rated to ${ctxShort(o.ctxMax)} context (${fmtCount(o.ctxMax)} tokens); larger values are accepted but exceed the rating. ${o.savedText}`)}">${icon("info")}</span>`
    : `Context range: 1,024\u20131,048,576<span class="info-tip" title="Accepted context range: 1,024 to 1,048,576. ${o.savedText}">${icon("info")}</span>`}</div>`;
}

/** The expert-offload slider block — label with guardrail-aware
 *  text, the slider track, the weight figures, and the host's
 *  guardrail decision row — shared between the Edit Flags form
 *  and the Add backend form. Dragging updates the readout in
 *  place via paintOffloadReadout; the host owns what releasing
 *  the slider means (the edit form saves immediately through
 *  backend_set_offload; the add form records the position for
 *  creation). */
function offloadSliderHtml(o: {
  offload: BackendOffload;
  layers: number;
  ownerId: string;
  disabled: boolean;
  decisionHtml: string;
}): string {
  const off = o.offload;
  const n = o.layers;
  const figs = offloadFigures(off, n);
  const dis = o.disabled ? " disabled" : "";
  return `<div style="margin-top:12px">
      <label style="font-size:12px;color:var(--text2);display:block">Expert offload to system RAM \u2014 <span data-offload-layers-label>${esc(offloadLayersLabel(off, n))}</span><span class="info-tip" data-offload-tip title="${esc(offloadTipText(off, n))}">${icon("info")}</span></label>
      <input type="range" class="offload-range" min="0" max="${off.moe_layers}" step="1" value="${n}" data-offload-slider data-offload-backend="${esc(o.ownerId)}" style="background:${offloadTrackBackground(off)}" aria-label="Expert offload layers for ${esc(o.ownerId)}"${dis}>
      <div style="font-size:12px;color:var(--text2);margin-top:4px"><span data-offload-token style="font-family:var(--mono)">${esc(offloadTokenText(off, n))}</span> \u00b7 GPU weights: <span data-offload-gpu>${fmtGb(figs.gpuMib / 1024)}</span> GiB \u00b7 System RAM: <span data-offload-sys>${fmtGb(figs.sysMib / 1024)}</span> GiB</div>
      ${o.decisionHtml}
    </div>`;
}

/** Update an offload slider's readout in place (layers label,
 *  flag tokens, weight figures, info tip) — shared by both forms,
 *  so a drag never needs a repaint in either. */
function paintOffloadReadout(card: Element, o: BackendOffload, n: number): void {
  const figs = offloadFigures(o, n);
  const setText = (sel: string, txt: string): void => {
    const el = card.querySelector(sel);
    if (el) el.textContent = txt;
  };
  setText("[data-offload-layers-label]", offloadLayersLabel(o, n));
  setText("[data-offload-token]", offloadTokenText(o, n));
  setText("[data-offload-gpu]", fmtGb(figs.gpuMib / 1024));
  setText("[data-offload-sys]", fmtGb(figs.sysMib / 1024));
  const tip = card.querySelector("[data-offload-tip]");
  if (tip) tip.setAttribute("title", offloadTipText(o, n));
}

/** Apply one flags-field event to a draft — the collection logic
 *  both forms share for the ctx/batch preset+custom pairs and
 *  the parallel preset+custom pair. Preset selects set the value
 *  on "change" and report that a repaint is needed (to show or
 *  hide the Custom numeric input); custom inputs only update the
 *  draft. Returns true when the host should repaint. The parallel
 *  value is the explicit slot count, or null for the field's
 *  Default (auto) choice; choosing Custom seeds the input from
 *  the current selection — the auto slot count when unset — so
 *  the operator edits a real number, never a blank. */
function applyFlagsField(
  draft: { ctx: number; batch: number; parallel: number | null },
  field: string,
  value: string,
  isChange: boolean,
): boolean {
  if (field === "ctx-custom") { draft.ctx = Number(value); return false; }
  if (field === "batch-custom") { draft.batch = Number(value); return false; }
  if (field === "parallel-custom") { draft.parallel = Number(value); return false; }
  if (isChange && field === "ctx-preset") {
    if (value !== "custom") draft.ctx = Number(value);
    return true;
  }
  if (isChange && field === "batch-preset") {
    if (value !== "custom") draft.batch = Number(value);
    return true;
  }
  if (isChange && field === "parallel-preset") {
    if (value === "default") draft.parallel = null;
    else if (value === "custom") draft.parallel = draft.parallel ?? PARALLEL_LLAMA_AUTO_SLOTS;
    else draft.parallel = Number(value);
    return true;
  }
  return false;
}

/** Draft state for the in-card flags form, non-null only for the
 *  backend whose form is open. ctxMax/modelId describe the backend's
 *  model (its rated context from the catalog), for annotating
 *  presets beyond the rating. offloadLayers is the offload slider's
 *  draft position (MoE layers on system RAM), null when the
 *  backend's model is not offload-capable. offloadPendingLayers is
 *  a released draft position the guardrail blocks, awaiting the
 *  operator's override/snap decision in the form, else null.
 *  mtpPosition is the MTP draft control's chosen value (0 = Off,
 *  1..4 = draft tokens), null when the backend's model is not
 *  MTP-capable. moeCacheOn / moeCacheMib are the expert-cache
 *  control's draft: the switch state and the chosen size (the
 *  size draft is retained while the switch is off, so toggling
 *  back on restores it); the size is null when the backend has
 *  never carried a cache and none has been chosen. parallel is the Parallel slots field's draft
 *  selection: the explicit slot count, or null for the unset
 *  state (llama.cpp's default, auto). advanced/rawText/rawWarnings
 *  carry the advanced flags editor's draft (tracker #60): while
 *  advanced is on, the raw text is the single source of truth and
 *  the structured fields are inert. editModelId is the Edit
 *  panel's model draft: the registered model chosen in the
 *  panel's Model select ("" when the backend's file matches no
 *  registered model), applied through model_assign only when the
 *  panel saves — the card face carries no model control. */
export interface FlagsDraft { ctx: number; batch: number; ctxMax: number | null; modelId: string | null; offloadLayers: number | null; offloadPendingLayers: number | null; mtpPosition: number | null; moeCacheOn: boolean; moeCacheMib: number | null; parallel: number | null; cacheSel: CacheSelectionLike | null; cachePreview: CachePreview | null; advanced: boolean; rawText: string; rawWarnings: FlagWarningLike[]; editModelId: string }

/** State for the in-card Placement form (tensor split editing,
 *  moved here from the GPUs page — Pete's direction, 2026-10-16),
 *  non-null only for the backend whose form is open. gpus mirrors
 *  the latest telemetry GPU list for the include/exclude
 *  checkboxes; excluded holds the unchecked GPU indices;
 *  projection is the last successful propose result for this
 *  backend; busy disables the form's buttons while a propose or
 *  apply invoke is in flight. */
export interface PlaceCardState {
  gpus: { index: number; name: string }[];
  excluded: Set<number>;
  modelId: string | null;
  projection: PlacementProjectionData | null;
  busy: boolean;
}

function backendCard(
  b: BackendRow,
  openArgv: Set<string>,
  openMenus: Set<string>,
  argvCache: Map<string, string[]>,
  msgs: Map<string, CardNote>,
  logTails: Map<string, LogRow[]>,
  inFlight: Set<string>,
  models: ModelRow[],
  confirmRemove: Set<string>,
  residency: Map<string, { gpu: number; name: string; mib: number | null }[]>,
  gpuNames: Map<number, string>,
  flags: FlagsDraft | null,
  predictedHtml: string,
  place: PlaceCardState | null,
  gpuTotalsMiB: Map<number, number>,
  detailsOpen: boolean,
  predictRefreshing: Map<string, number>,
  confirmOverride: Set<string>,
  armedWddmStart: Set<string>,
): string {
  // The row's state is the EFFECTIVE lifecycle state (wire state
  // with the shared store's pending overlay, normalized by the
  // view's load): a restart from any surface reads "restarting…".
  const st: LifecycleState = fromWire(b.lifecycle ?? b.state);
  const active = META[st].isLive || META[st].isTransition;
  // Buttons render disabled while this card's (non-lifecycle) action
  // is in flight. The set lives outside the repainted DOM, so the
  // disabled state survives the repaints a poll or a log-tail fetch
  // triggers. Lifecycle buttons follow the shared rule instead
  // (lifecycleActions: disabled with the reason mid-transition).
  const dis = inFlight.has(b.id) ? " disabled" : "";
  const applyBlocked = applyBlockedReason({ kind: "backend", id: b.id });
  const lcSpecs = lifecycleActions(st, {
    kind: "backend", enabled: b.enabled, configured: b.configured,
    ...(applyBlocked ? { blockedReason: applyBlocked } : {}),
  });
  const lcSpec = (a: "start" | "stop" | "restart") => lcSpecs.find((x) => x.action === a);
  const lcAttrs = (a: "start" | "stop" | "restart"): string => {
    const sp = lcSpec(a);
    return `${inFlight.has(b.id) || sp?.disabled ? " disabled" : ""}${sp?.reason ? ` title="${esc(sp.reason)}"` : ""}`;
  };
  const stopBtn = `<button class="btn" data-back-act="stop" data-back-id="${esc(b.id)}"${lcAttrs("stop")}>${icon("pause")} Stop</button>`;
  const restartBtn = `<button class="btn${b.fit?.state === "wddm_paged" && armedWddmStart.has(b.id) ? " armed" : ""}" data-back-act="restart" data-back-id="${esc(b.id)}"${lcAttrs("restart")}>${icon("restart")} Restart</button>`;
  // A WDDM paged fit starts only through the two-click confirm
  // (the delete-confirm idiom): the first click arms the button,
  // the second starts with the overcommit flag. The armed label
  // states the paged amount and nothing else.
  const wddmPaged = b.fit?.state === "wddm_paged";
  const startBtn = wddmPaged
    ? `<button class="btn${armedWddmStart.has(b.id) ? " armed" : ""}" data-back-act="start" data-back-id="${esc(b.id)}" title="${armedWddmStart.has(b.id) ? "Confirm start with paging" : "Start (pages to system RAM)"}"${inFlight.has(b.id) || lcSpec("start")?.disabled ? " disabled" : ""}>${icon("play")} ${armedWddmStart.has(b.id) ? `Start — ${fmtGb((b.fit?.paged_mib ?? 0) / 1024)} GiB paged` : "Start"}</button>`
    : `<button class="btn" data-back-act="start" data-back-id="${esc(b.id)}"${lcAttrs("start")}>${icon("play")} Start</button>`;
  const testBtn = `<button class="btn icon-only" data-back-act="test" data-back-id="${esc(b.id)}" title="Test backend" aria-label="Test ${esc(b.id)}"${dis}>${icon("bolt")}</button>`;
  // The tool-call probe (MCP design §7.2) and the go-to-logs link
  // (tracker #47) are NOT card-face buttons: the wrench read as an
  // edit control (Pete's report, 2026-10-20), so both live in the
  // card's Other-actions menu below. Every backend has a Logs-view
  // scope (its supervisor ring, retained even after removal), so
  // the menu's logs item is unconditional, like the button was.
  // Enable/disable is the shared state switch (rule 16). It renders
  // for every configured backend so the state is always visible;
  // the control is inert exactly when the old buttons were not
  // offered — an action in flight, or enabled in a live state (the
  // command layer refuses disable-while-running: a running backend
  // is stopped, not disabled). The data-back-act flips with the
  // state, so the delegated handler fires the same
  // applyBackendConfig call the two buttons fired.
  const toggleable = !b.enabled || st === "stopped" || st === "failed";
  const enableSw = enableSwitch({
    on: b.enabled,
    name: `Backend ${b.id}`,
    attrs: `data-back-act="${b.enabled ? "disable" : "enable"}" data-back-id="${esc(b.id)}"`,
    title: b.enabled && !toggleable ? `Backend ${b.id}: enabled (stop it before disabling)` : undefined,
    disabled: inFlight.has(b.id) || !toggleable,
  });
  const removeBtn = `<button class="btn icon-only danger${confirmRemove.has(b.id) ? " armed" : ""}" data-back-act="remove" data-back-id="${esc(b.id)}" title="${confirmRemove.has(b.id) ? "Confirm remove" : "Remove backend"}" aria-label="${confirmRemove.has(b.id) ? "Confirm remove" : "Remove"} ${esc(b.id)}"${dis}>${icon("trash")}</button>`;
  // The shared lifecycle rule (lifecycleActions) decides Start /
  // Stop / Restart and their disabled reasons — the same rule as the
  // Dashboard cards and Settings > Supervision:
  // - Stop while live or in transition, whether the backend is
  //   configured, enabled, or an orphan — stopping is always allowed
  //   (disabled mid-restart/stop with the reason).
  // - Restart for configured live backends (disabled with the reason
  //   when the backend is disabled — the shared rule).
  // - Start for stopped/failed configured backends (disabled with the
  //   reason when the backend is disabled).
  // - The enable/disable switch shows on every configured backend;
  //   it is toggleable only in the states listed for it above.
  // - Test only while running.
  const parts: string[] = [];
  if (lcSpec("stop")) parts.push(stopBtn);
  if (b.configured && lcSpec("restart") && !lcSpec("start")) parts.push(restartBtn);
  if (b.configured && lcSpec("start")) parts.push(startBtn);
  if (b.configured) parts.push(enableSw);
  if (st === "running") parts.push(testBtn);
  // Removal is refused by the command layer while running; offer it
  // only for configured backends with no live process.
  if (b.configured && !active) parts.push(removeBtn);
  const actions = parts.join("\n        ");
  // Advanced, infrequently used options live behind one hamburger
  // menu so the card's primary action row stays within the card.
  // Menu-open state lives outside the repainted DOM, like the other
  // card state, so a poll repaint cannot close an open menu.
  const menuOpen = openMenus.has(b.id);
  const advancedMenu = `<div class="advanced-menu-wrap" data-back-menu-wrap>
      <button class="btn icon-only" data-back-menu="${esc(b.id)}" aria-haspopup="menu" aria-expanded="${menuOpen}" title="Advanced options" aria-label="Advanced options for ${esc(b.id)}"${dis}>${icon("menu")}</button>
      ${menuOpen ? `<div class="advanced-menu" role="menu" aria-label="Advanced options for ${esc(b.id)}">
        ${b.configured ? `<button class="menu-item" role="menuitem" data-back-menu-item data-back-act="flags" data-back-id="${esc(b.id)}"${dis}>${icon("gear")} Edit</button>` : ""}
        <button class="menu-item" role="menuitem" data-back-menu-item data-back-act="argv" data-back-id="${esc(b.id)}"${dis}>${icon("logs")} Launch command</button>
        ${b.configured ? `<button class="menu-item" role="menuitem" data-back-menu-item data-back-act="toolcall" data-back-id="${esc(b.id)}"${dis}>${icon("wrench")} Test tool calls</button>` : ""}
        ${b.configured ? `<button class="menu-item" role="menuitem" data-back-menu-item data-back-act="tuning" data-back-id="${esc(b.id)}">${icon("gauge")} Optimize this backend</button>` : ""}
        <a class="menu-item" role="menuitem" data-back-menu-item data-back-id="${esc(b.id)}" href="#/logs?scope=${esc(encodeURIComponent(b.id))}&follow=1">${icon("logs")} View all logs</a>
      </div>` : ""}
    </div>`;
  const argvOpen = openArgv.has(b.id);
  const argv = argvOpen
    ? `<div style="margin-top:10px;font-size:11px;color:var(--text3)">Launch command (next start)</div>
      <pre style="margin-top:4px;padding:10px 12px;background:var(--panel2);border:1px solid var(--border);border-radius:8px;font-family:var(--mono);font-size:11px;white-space:pre-wrap;word-break:break-all">${esc((argvCache.get(b.id) ?? []).map(quoteArg).join(" "))}</pre>`
    : "";
  const note = msgs.get(b.id);
  const msgHtml = note
    ? `<span style="color:${note.err ? "var(--err)" : "var(--text2)"}">${esc(note.text)}</span>` : "";
  // Card narrative lives in header-chip tooltips, never as inline
  // prose (Pete's Label: Value rule, 2026-10-16): the orphan and
  // adopted explanations ride the title attribute of the chips that
  // already flag those states. The orphan text follows the process
  // state — a failed or stopping orphan is not "still running".
  const orphanText = !b.configured
    ? b.state === "failed"
      ? "It failed, and its entry was removed from the configuration. It cannot be restarted."
      : active
        ? "Still running, but its entry was removed from the configuration. It can be stopped here; it cannot be restarted."
        : "Its entry was removed from the configuration. It cannot be restarted."
    : "";
  // An adopted backend was started by a previous launch of the app
  // (its processes survive the app closing) and taken over by pid:
  // it can be stopped/restarted here, but its earlier output is not
  // in this session's log ring.
  const adoptedText = b.adopted
    ? "Started by a previous launch and adopted by this session. Output from before adoption is not in the Logs view."
    : "";
  // One rendering for a stored log line, shared by the failed
  // block below and the Details section's latest-lines block.
  const logLineHtml = (r: LogRow): string => `<div class="ln"><span class="t">${esc(fmtTime(r.ts))}</span> <span class="chip">${esc(r.stream)}</span> ${esc(r.line)}</div>`;
  let logsBlock = "";
  // Suppressed while Details are open: the expansion's
  // latest-lines block shows the same stored tail (all 5 lines)
  // directly above, and the lines render in one place only.
  if (b.state === "failed" && !detailsOpen) {
    const tail = logTails.get(b.id);
    // An empty tail renders no block at all (principle 13) — the
    // full log is one click away (View all logs beside the
    // caption). The caption's qualifier rides its
    // info-tip. The stored tail holds 5 lines (the Details
    // block's count); this diagnostic block keeps its last 3.
    const lines = tail && tail.length
      ? `<div style="margin-top:8px;font-size:11px;color:var(--text3);display:flex;align-items:baseline;gap:8px">Latest output<span class="info-tip" title="stderr preferred — that is where llama-server reports load failures">${icon("info")}</span><a href="#/logs?scope=${esc(encodeURIComponent(b.id))}&follow=1" style="margin-left:auto">View all logs</a></div><div class="logstream" style="margin-top:4px">${tail.slice(-3).map(logLineHtml).join("")}</div>`
      : "";
    logsBlock = lines;
  }
  // GPU residency for this backend, as bars (Pete's card redesign,
  // 2026-10-16): one horizontal bar per GPU, name at left, GiB at
  // right, filled to the backend's share of that GPU's total VRAM.
  // Measured per-process telemetry is authoritative while the
  // backend is active; under 1 GiB is a CUDA context only and
  // renders as a striped sliver (its tooltip says so). When the
  // backend is inactive there is no measured residency, so the bars
  // are ghost fills of the configured tensor split — where the
  // model will load at its next start. An unmeasured GPU (mib null
  // — WDDM, counter data unavailable) gets an empty bar; "not
  // measured" exists only as its tooltip, never as inline text.
  const res = residency.get(b.id) ?? [];
  const gpuColors = ["#4a9eff", "#a371f7", "#39c5cf", "#d29922", "#3fb950", "#f85149"];
  const splitEntries = (b.tensor_split ?? [])
    .map((frac, index) => ({ index, frac }))
    .filter((e) => e.frac > 0);
  const gpuLabel = (index: number): string => {
    const measured = res.find((r) => r.gpu === index);
    return `GPU ${index}${(measured?.name ?? gpuNames.get(index)) ? ` · ${esc(measured?.name ?? gpuNames.get(index) ?? "")}` : ""}`;
  };
  const splitSummary = splitEntries.length
    ? splitEntries.map((e) => `${gpuLabel(e.index)} — ${Math.round(e.frac * 100)}%`).join(" · ")
    : "";
  const barRow = (index: number, fillPct: number, valText: string, cls = "", tip = ""): string => `<div class="res-bar-row"${tip ? ` title="${esc(tip)}"` : ""}>
      <span class="res-bar-name">${gpuLabel(index)}</span>
      <span class="res-bar-track${cls === "ghost" ? " ghost" : ""}"><i class="${cls}" style="width:${Math.min(100, Math.max(0, fillPct)).toFixed(1)}%;background-color:${gpuColors[index % gpuColors.length]}"></i></span>
      <span class="res-bar-val">${valText}</span></div>`;
  let gpuHtml = "";
  if (b.configured) {
    const knownRes = res.filter((r) => r.mib !== null);
    let bars = "";
    let basis = "";
    let titleTip = "";
    if (knownRes.length) {
      basis = "measured";
      bars = res.map((r) => {
        if (r.mib === null) return barRow(r.gpu, 0, "", "", "Not measured — per-process usage is not measured on this GPU; placement follows the configured split.");
        const total = gpuTotalsMiB.get(r.gpu) ?? 0;
        const share = total > 0 ? (r.mib / total) * 100 : 0;
        const gib = `${fmtGb(r.mib / 1024)} GiB`;
        return r.mib < 1024
          ? barRow(r.gpu, share, gib, "ctx", `Context only: ${gib} — under 1 GiB on a GPU is the CUDA context only, not model weights.`)
          : barRow(r.gpu, share, gib);
      }).join("");
    } else if (res.length) {
      basis = "not measured";
      titleTip = "Per-process usage is not measured on this GPU — placement follows the configured split.";
      bars = res.map((r) => barRow(r.gpu, 0, "", "", "Not measured — per-process usage is not measured on this GPU; placement follows the configured split.")).join("");
    } else if (!active) {
      // Ghost bars from the configured split; an unsplit backend
      // loads whole on GPU 0 (the app's standing placement default,
      // the same basis the dashboard placements use).
      const ghost = splitEntries.length ? splitEntries : [{ index: 0, frac: 1 }];
      basis = "configured";
      bars = ghost.map((e) => barRow(e.index, e.frac * 100, `${Math.round(e.frac * 100)}%`, "ghost", "Configured split — this share of the model loads on this GPU at the backend's next start.")).join("");
    }
    // An active backend with no attributed process yet has nothing
    // real to encode; the section appears with the first sample.
    if (bars) {
      gpuHtml = `<div class="backend-gpus">
      <div class="backend-gpus-title">GPUs (${basis})${titleTip ? `<span class="info-tip" title="${esc(titleTip)}">${icon("info")}</span>` : ""}</div>
      ${bars}
    </div>`;
    }
  }
  // Gateway statistics for this backend (from the LiteLLM
  // metrics store). The card carries Throughput only: Requests
  // (1h) and Errors (1h) already live in the view's overview
  // list (numbers in one place, principle 6).
  // The stat tiles: live Throughput (the recent window's rate:
  // the backend's own /slots rate when available, the gateway
  // live rate as fallback — present only when the poller
  // actually measured a current rate, absent, never 0, when
  // neither signal is available) and Predicted throughput
  // (the saved configuration's exact-match measurement, design
  // §7 — generation only, the rest in the tooltip).
  const statTiles: [string, string, string | null][] = [];
  if (b.throughput_toks !== undefined) {
    statTiles.push(["Throughput", `${trim1(b.throughput_toks)} tok/s`, null]);
  }
  // Predicted throughput renders on EVERY card (Pete,
  // 2026-10-20): with no exact measurement the tile carries the
  // explicit "Not measured" empty state — never 0, never an
  // omitted field (principle 12) — and the Refresh control
  // beside it runs the standardized benchmark (the Optimize
  // Measure profile, the one whose entries feed the prediction)
  // to fill or update it. Measuring requires a running backend
  // (the measure op's own rule), so a stopped or unconfigured
  // backend's control is disabled with the reason as its title.
  const predictedTile = ((): string => {
    const startedAt = predictRefreshing.get(b.id);
    const fig = b.predicted && b.predicted.predicted_per_second !== null
      ? b.predicted.predicted_per_second
      : null;
    const canMeasure = b.configured && b.state === "running";
    const refreshTitle = startedAt !== undefined
      ? "Measurement running"
      : !b.configured
        ? "Not in the configuration — nothing to measure"
        : b.state !== "running"
          ? "Backend is not running — start it to refresh the prediction"
          : "Refresh the prediction — runs the standardized benchmark";
    const refreshBtn = `<button class="btn" data-back-act="predict-refresh" data-back-id="${esc(b.id)}" style="padding:2px 8px;font-size:11px" title="${esc(refreshTitle)}" aria-label="Refresh predicted throughput for ${esc(b.id)}"${startedAt !== undefined || !canMeasure ? " disabled" : ""}>${icon("restart")} Refresh</button>`;
    const tip = fig !== null && b.predicted
      ? predictedCardTip(b.predicted)
      : "No measurement of this backend's saved configuration yet. Refresh runs the standardized benchmark (the Optimize page's Measure action) against the running backend and stores the result as the prediction.";
    const value = startedAt !== undefined
      ? `Measuring <span data-pred-elapsed="${esc(b.id)}">${fmtElapsed(Math.max(0, Math.floor((Date.now() - startedAt) / 1000)))}</span>`
      : fig !== null
        ? `${trim1(fig)} tok/s`
        : `<span style="color:var(--text3);font-weight:400">Not measured</span>`;
    return `<div class="kv backend-stats-predicted"><span class="k">Predicted throughput<span class="info-tip" title="${esc(tip)}">${icon("info")}</span></span><span class="v" style="font-size:13px">${value} ${refreshBtn}</span></div>`;
  })();
  const statsHtml = `<div class="backend-stats">
        ${statTiles
          .map(([k, v, tip]) => `<div class="kv"><span class="k">${esc(k)}${tip ? `<span class="info-tip" title="${esc(tip)}">${icon("info")}</span>` : ""}</span><span class="v" style="font-size:13px">${esc(v)}</span></div>`).join("")}
        ${predictedTile}
      </div>`;
  // Offload guardrail and probe lines for the saved position
  // (at rest, not the flags-form draft). Status colors follow the
  // design principles: light red for blocked/failed, amber for
  // transitional/warning, gray for the neutral custom spelling.
  // Nothing renders when none of the states apply (principle 13).
  // A blocked-and-unresolved position carries the one-click
  // resolving action; a custom spelling in that same state reuses
  // that button rather than duplicating it.
  const offloadStatusHtml = (() => {
    const o = b.offload;
    const probe = b.offload_probe;
    if (!o && !probe) return "";
    const lines: string[] = [];
    const line = (color: string, inner: string): string =>
      `<div style="font-size:12px;color:${color};margin-top:6px">${inner}</div>`;
    const g = o?.guardrail ?? null;
    if (o && g?.status === "blocked" && !g.overridden) {
      // The blocked fact is a launch-preflight fact and renders in
      // both modes; the one-click resolving action is a flags
      // write, so in advanced flags mode (tracker #60) the line
      // carries no button — the editor is the remedy, and the
      // server refuses the structured write anyway. The deliberate
      // override is different: it writes only the activation
      // journal, never the flags, so its action renders in both
      // modes — a hand-edited blocked position is exactly the case
      // it exists for. The override is operation-critical, so it
      // keeps words + icon and takes a confirmation step whose
      // button names the exact position (layers + placed CC set)
      // being overridden (the armed state, the remove button's
      // idiom).
      const ccs = (g.placed_ccs ?? []).join(", ") || g.compute_capability;
      const overrideBtn = confirmOverride.has(b.id)
        ? `<button class="btn danger armed" data-back-act="offload-override-confirm" data-back-id="${esc(b.id)}"${dis}>${icon("warn")} Confirm override: ${o.layers} of ${o.moe_layers} layers, CC ${esc(ccs)}</button>`
        : `<button class="btn" data-back-act="offload-override-arm" data-back-id="${esc(b.id)}"${dis}>${icon("warn")} Override start block</button>`;
      lines.push(line("#ff7b72", b.flags_advanced
        ? `Start blocked: offload ${o.layers} of ${o.moe_layers} layers is outside the verified range for this GPU ${overrideBtn}`
        : `Start blocked: offload ${o.layers} of ${o.moe_layers} layers is outside the verified range for this GPU <button class="btn" data-back-act="offload-set-verified" data-back-id="${esc(b.id)}"${dis}>${icon("check")} Set to ${g.nearest_verified_layers} of ${o.moe_layers} layers</button> ${overrideBtn}`));
    } else if (o && g?.status === "blocked" && g.overridden) {
      // The overridden state is distinct (amber, named as an
      // override) and carries the clear action: clearing drops the
      // journal record and the position is blocked again.
      const ccs = (g.placed_ccs ?? []).join(", ") || g.compute_capability;
      lines.push(line("#e3b341", `Offload override recorded: ${o.layers} of ${o.moe_layers} layers is unverified on this GPU (CC ${esc(ccs)}) <button class="btn" data-back-act="offload-override-clear" data-back-id="${esc(b.id)}"${dis}>${icon("ban")} Clear override</button>`));
    } else if (o && g?.status === "untested") {
      lines.push(line("#e3b341", `Partial offload: unverified on this GPU`));
    }
    // The "custom spelling" line is a structured-form readout:
    // in advanced mode the operator's own string is displayed
    // verbatim (the raw flags line), so the line is redundant.
    if (o?.custom && !b.flags_advanced) {
      lines.push(line("var(--text2)", `Offload: Custom (${o.layers} of ${o.moe_layers} layers)`));
    }
    if (probe?.state === "probing") {
      lines.push(line("#e3b341", o ? `Probing offload: ${probe.layers} of ${o.moe_layers} layers` : `Probing offload: ${probe.layers} layers`));
    } else if (probe?.state === "failed") {
      const detail = (probe.detail ?? "").trim();
      const inner = !detail
        ? "Offload probe failed"
        : detail.length <= 120
          ? `Offload probe failed \u2014 ${esc(detail)}`
          : `Offload probe failed<span class="info-tip" title="${esc(detail)}">${icon("info")}</span>`;
      lines.push(line("#ff7b72", inner));
    }
    // A passed probe is transient assurance, not card state: no line.
    return lines.join("");
  })();
  // At-rest MTP lines (tracker #53), in the offload status
  // block's idiom: a non-standard saved spelling, and the
  // --parallel conflict when MTP is on — surfaced, never fixed
  // silently. Nothing renders in the ordinary states.
  const mtpStatusHtml = (() => {
    const m = b.mtp;
    // Advanced flags mode (tracker #60): both lines decode the
    // raw flags into structured terms the mode supersedes. The
    // raw string itself is displayed verbatim on the card, so a
    // custom spelling or a --parallel conflict is visible in it
    // directly, and the flags editor is the remedy the conflict
    // copy points at.
    if (!m || b.flags_advanced) return "";
    const lines: string[] = [];
    if (m.custom) {
      lines.push(`<div style="font-size:12px;color:var(--text2);margin-top:6px">MTP: Custom (${esc(mtpSavedTokenText(m))})</div>`);
    }
    if (m.enabled && m.parallel != null && m.parallel !== 1) {
      lines.push(`<div style="font-size:12px;color:#e3b341;margin-top:6px">${esc(mtpConflictText(m.parallel))}</div>`);
    }
    return lines.join("");
  })();
  // The flags editor renders inside this card (Pete's request,
  // 2026-10-16): preset dropdowns built from the model's rated
  // context, with a Custom numeric input when the value is not a
  // preset. Field events sync to the wire-level draft state, so the
  // poll repaint re-renders the form without losing input.
  // The expert-offload slider rides the same panel when the
  // backend's model is offload-capable: its position is the number
  // of MoE layers whose routed experts sit in system RAM. The
  // track's green band ends at the model's green limit (its
  // catalog default offload — the most offload that stays fast);
  // yellow and red split the range past it. Sliding updates the
  // readout in place; releasing the slider saves through
  // backend_set_offload, which rewrites only the offload tokens in
  // the backend's raw flags.
  const offloadHtml = flags && b.offload ? (() => {
    const o = b.offload;
    const n = flags.offloadLayers ?? o.layers;
    // A released position the guardrail blocks is not committed:
    // the form carries an inline decision row until the operator
    // saves it with an override or snaps to the nearest verified
    // position. Both actions are operation-critical, so both keep
    // their words and an icon.
    const pending = flags.offloadPendingLayers;
    const decisionHtml = pending !== null && o.guardrail && offloadIsUntested(o, pending) ? `<div data-offload-decision style="display:flex;gap:8px;align-items:center;flex-wrap:wrap;margin-top:8px">
      <span data-offload-decision-text style="font-size:12px;color:#e3b341">${pending} of ${o.moe_layers} layers is unverified on this GPU</span>
      <button class="btn" data-back-act="offload-override" data-back-id="${esc(b.id)}"${dis}>${icon("warn")} Save with override</button>
      <button class="btn" data-back-act="offload-snap" data-back-id="${esc(b.id)}"${dis}>${icon("check")} Snap to ${o.guardrail.nearest_verified_layers} of ${o.moe_layers} layers</button>
    </div>` : "";
    return offloadSliderHtml({ offload: o, layers: n, ownerId: b.id, disabled: inFlight.has(b.id), decisionHtml });
  })() : "";
  // The expert-cache control, directly under the offload slider
  // in the same panel: the shared enable switch (rule 16) plus a
  // size select in the ctx/batch selects' idiom — the presets,
  // Auto, and the saved value as its own option when it is not a
  // preset — whose state lives in the wire-level draft
  // (flags.moeCacheOn / flags.moeCacheMib), with the token
  // readout naming the flag the value writes. Rendered only for
  // offload-capable backends (the payload carries `moe_cache` —
  // the offload slider's eligibility). Both controls are
  // disabled while the saved offload is 0: with no experts in
  // system RAM there is nothing for the cache to hold, and the
  // label's tip says so.
  const moeCacheHtml = flags && b.moe_cache ? (() => {
    const mc = b.moe_cache;
    const on = flags.moeCacheOn;
    const size = flags.moeCacheMib ?? mc.mib ?? MOE_CACHE_DEFAULT_MIB;
    const noOffload = mc.offload_layers === 0;
    const ctlDisabled = inFlight.has(b.id) || noOffload;
    const tip = noOffload
      ? "Disabled: this backend offloads 0 MoE layers, so no experts sit in system RAM for the cache to hold. Raise the offload above 0 to enable it."
      : "Keeps the most recently used experts of the offloaded (system-RAM) layers in GPU memory and computes them there; experts not in the cache still run on the CPU. The size is priced into the backend's memory requirement, and a start that cannot fit it is refused with the numbers. The best size is empirical — the optimizer can sweep it once that support lands. Applies on the backend's next start.";
    const sizeOptions = MOE_CACHE_PRESETS_MIB.includes(size) ? MOE_CACHE_PRESETS_MIB : [...MOE_CACHE_PRESETS_MIB, size].sort((x, y) => x - y);
    const draftIsSaved = on === (mc.mib != null) && (!on || size === mc.mib);
    return `<div style="margin-top:12px">
      <div style="font-size:12px;color:var(--text2)">Expert cache<span class="info-tip" title="${esc(tip)}">${icon("info")}</span></div>
      <div style="display:flex;gap:6px;margin-top:4px;align-items:center">
        ${enableSwitch({ on, name: `Expert cache for ${b.id}`, attrs: `data-moe-cache-switch data-moe-cache-backend="${esc(b.id)}"`, disabled: ctlDisabled })}
        <select data-moe-cache-select data-moe-cache-backend="${esc(b.id)}" style="${SELECT_STYLE};flex:1;min-width:0" aria-label="Expert cache size for ${esc(b.id)}"${ctlDisabled ? " disabled" : ""}>
          ${sizeOptions.map((v) => `<option value="${v}"${v === size ? " selected" : ""}>${v} MiB</option>`).join("")}
          <option value="auto">Auto</option>
        </select>
      </div>
      <div style="font-size:12px;color:var(--text2);margin-top:4px"><span data-moe-cache-token style="font-family:var(--mono)">${esc(moeCacheSavedTokenText(mc))}</span>${mc.custom && draftIsSaved ? `<span title="The saved flags spell the expert cache in a form the control does not write (an = spelling, a 0 token, ...). Saving from this control writes the canonical token; every other flag is preserved."> · custom flags</span>` : ""}</div>
    </div>`;
  })() : "";
  // The MTP draft control (tracker #53), directly under the
  // offload slider in the same panel: a dropdown over
  // Off/1/2/3/4 in the ctx/batch selects' idiom (SELECT_STYLE,
  // the current value as the selected option, always visible at
  // rest), whose value lives in the wire-level draft state
  // (flags.mtpPosition), with the token readout naming the
  // flags the value writes, and the --parallel conflict line
  // when the saved flags set a slot count other than 1.
  // Rendered only for MTP-capable backends (the payload carries
  // `mtp`). The readout shows the SAVED spelling until the value
  // changes; a backend on without an explicit length reads as
  // llama.cpp's default rather than an implied saved value.
  const mtpHtml = flags && b.mtp ? (() => {
    const m = b.mtp;
    const pos = flags.mtpPosition ?? mtpPosition(m);
    const conflict = pos > 0 && m.parallel != null && m.parallel !== 1;
    return `<div style="margin-top:12px">
      <label style="font-size:12px;color:var(--text2);display:block">MTP draft tokens<span class="info-tip" title="Speculative decoding: the model drafts up to this many tokens ahead and verifies them in a single pass. More draft tokens can generate faster, but drafts the verifier rejects are wasted work, so gains depend on how often drafts are accepted. Applies on the backend's next start.">${icon("info")}</span>
        <span style="display:flex;gap:6px;margin-top:4px">
          <select data-mtp-select data-mtp-backend="${esc(b.id)}" style="${SELECT_STYLE};flex:1;min-width:0" aria-label="MTP draft tokens for ${esc(b.id)}"${dis}>
            ${MTP_POSITIONS.map((v) => `<option value="${v}"${v === pos ? " selected" : ""}>${v === 0 ? "Off" : String(v)}</option>`).join("")}
          </select>
        </span></label>
      <div style="font-size:12px;color:var(--text2);margin-top:4px"><span data-mtp-token style="font-family:var(--mono)">${esc(mtpSavedTokenText(m))}</span>${m.custom ? `<span title="The saved flags spell MTP in a form the control does not write (an = spelling, a shared --spec-type list, ...). Saving from this control writes the canonical tokens; every other flag is preserved."> · custom flags</span>` : ""}</div>
      <div data-mtp-conflict style="font-size:12px;color:#e3b341;margin-top:4px;${conflict ? "" : "display:none"}">${m.parallel != null ? esc(mtpConflictText(m.parallel)) : ""}</div>
    </div>`;
  })() : "";
  // The Parallel slots field (tracker #61), under the ctx/batch
  // pair: a preset+custom select over the backend's --parallel
  // count, committing through backend_set_parallel on change —
  // the sliders' commit model, since the value lives in raw flags
  // like theirs, not in the Save button's ctx/batch. The custom
  // marker shows while the draft still equals the saved value:
  // it describes the saved spelling, and a changed selection has
  // no saved spelling yet.
  const parallelHtml = flags && b.parallel ? flagsParallelHtml({
    value: flags.parallel,
    savedCustom: b.parallel.custom && flags.parallel === b.parallel.slots,
    fieldAttr: "data-flags-field",
    disabled: inFlight.has(b.id),
  }) : "";
  // The KV cache / flash-attention controls, under the parallel
  // field: selects committing through backend_set_cache on change
  // (the MTP/parallel commit model), with the predicted footprint
  // and predicted-best action rendered from the preview payload
  // the wire layer prices for the current draft.
  const cacheSel: CacheSelectionLike = flags?.cacheSel ?? (b.cache ? { k: b.cache.k, v: b.cache.v, flash_attn: b.cache.flash_attn } : { k: "f16", v: "f16", flash_attn: "auto" });
  const cacheHtml = flags && b.cache ? flagsCacheHtml({
    sel: cacheSel,
    savedCustom: b.cache.custom && sameCacheSelection(cacheSel, b.cache),
    savedPresent: b.cache.present,
    preview: flags.cachePreview,
    fieldAttr: "data-cache-field",
    bestAttr: `data-cache-best data-cache-backend="${esc(b.id)}"`,
    disabled: inFlight.has(b.id),
  }) : "";
  // Advanced flags mode (tracker #60): the panel is REPLACED by
  // the raw-flags text editor — the structured controls are not
  // rendered at all, so nothing inert invites an edit the server
  // would refuse. The textarea pre-fills with the backend's saved
  // raw flags (the string the structured decoders read); text
  // edits sync to the wire-level draft, so poll repaints keep the
  // text and the warning list renders live under the box.
  // The Edit panel's Model field (Pete's report, 2026-10-20):
  // the model is chosen here, as a draft, and applied through
  // model_assign only when the panel saves — the card face no
  // longer carries a model control that assigns on selection.
  // The select renders the DRAFT as selected (modelOptions'
  // selectedId), so a poll repaint shows the operator's unsaved
  // choice. Rendered in both panel variants: model_assign
  // preserves an advanced backend's raw flags verbatim
  // (flags_untouched), so the field is safe in advanced mode too.
  const editModelHtml = flags ? `<div style="margin-top:10px">
      <label style="font-size:12px;color:var(--text2);display:block">Model<span class="info-tip" title="The model this backend serves. The change applies only when you save from this panel; a running backend picks the new model up on its next start.">${icon("info")}</span>
        <span style="display:flex;gap:6px;margin-top:4px">
          <select data-edit-model="${esc(b.id)}" style="${SELECT_STYLE};flex:1;min-width:0" aria-label="Model for ${esc(b.id)}"${dis}>${modelOptions(b, models, flags.editModelId)}</select>
        </span></label>
    </div>` : "";
  const flagsHtml = flags ? (flags.advanced ? `<div class="backend-form-panel flags-panel">
      <div style="font-size:12px;font-weight:600">${icon("pencil")} Edit ${esc(b.id)}</div>
      ${editModelHtml}
      <div style="font-size:12px;color:var(--text2);margin-top:6px">Mode: Advanced<span class="info-tip" title="In advanced mode the text below is the backend's launch flags, edited directly. Context, batch, offload, MTP and placement are all set by this string; the structured form and its controls are disabled for this backend until you switch back. Warnings never block a save — unknown flags pass through to llama-server, which decides.">${icon("info")}</span></div>
      <textarea data-flags-raw="${esc(b.id)}" rows="4" spellcheck="false" style="width:100%;margin-top:8px;background:var(--panel2);border:1px solid var(--border);border-radius:8px;color:var(--text);padding:8px 10px;font-family:var(--mono);font-size:12px" aria-label="Raw flags for ${esc(b.id)}">${esc(flags.rawText)}</textarea>
      <div data-flags-warnings>${flagsRawWarningsHtml(flags.rawWarnings)}</div>
      <div style="font-size:12px;color:var(--text2);margin-top:6px">Saved to the configuration immediately; applies on the backend's next start.</div>
      <div data-predicted-slot>${predictedHtml}</div>
      <div class="card-actions"><button class="btn" data-back-act="flags-raw-save" data-back-id="${esc(b.id)}"${dis}>${icon("check")} Save</button>
        <button class="btn" data-back-act="flags-mode-structured" data-back-id="${esc(b.id)}"${dis}>${icon("gear")} Use structured form</button>
        <button class="btn icon-only" data-back-act="flags-cancel" data-back-id="${esc(b.id)}" title="Cancel" aria-label="Cancel">${icon("x")}</button></div>
    </div>` : `<div class="backend-form-panel flags-panel">
      <div style="font-size:12px;font-weight:600">${icon("pencil")} Edit ${esc(b.id)}</div>
      ${editModelHtml}
      ${flagsCtxBatchHtml({ ctx: flags.ctx, batch: flags.batch, ctxMax: flags.ctxMax, fieldAttr: "data-flags-field", disabled: inFlight.has(b.id) })}
      ${parallelHtml}
      ${cacheHtml}
      ${offloadHtml}
      ${moeCacheHtml}
      ${mtpHtml}
      ${flagsRatedLineHtml({ ctxMax: flags.ctxMax, modelId: flags.modelId, savedText: "Saved to the configuration immediately; applies on the backend's next start. Tensor split is set in the Placement section below." })}
      <div data-predicted-slot>${predictedHtml}</div>
      <div class="card-actions"><button class="btn" data-back-act="flags-save" data-back-id="${esc(b.id)}"${dis}>${icon("check")} Save</button>
        <button class="btn" data-back-act="flags-mode-advanced" data-back-id="${esc(b.id)}"${dis}>${icon("pencil")} Edit flags as text</button>
        <button class="btn icon-only" data-back-act="flags-cancel" data-back-id="${esc(b.id)}" title="Cancel" aria-label="Cancel">${icon("x")}</button></div>
    </div>`) : "";
  // The Placement form renders inside this card (moved from the
  // GPUs page, Pete's direction, 2026-10-16): the tensor split is a
  // per-backend setting, so it is edited here like the flags. The
  // model is derived from the backend's configuration; the
  // projection renders in the card; Apply is scoped to this
  // backend. Form state lives in the wire layer (checkbox changes
  // sync to wire state), so poll repaints keep the form intact.
  const placeDis = dis || (place && place.busy) ? " disabled" : "";
  const projHtml = place && place.projection ? (() => {
    const p = place.projection;
    // When the running backend's current allocation is credited,
    // the table shows the credit basis: what it already holds on
    // each GPU and the additional allocation that must fit in free
    // VRAM. Without a credit the two extra columns carry no
    // information and are omitted.
    const credited = (p.current_mib ?? []).some((v) => v > 0);
    const rows = p.split.map((frac, i) => {
      const gi = p.gpu_indices && p.gpu_indices[i] !== undefined ? p.gpu_indices[i] : i;
      const gname = place.gpus.find((g) => g.index === gi)?.name ?? `GPU ${gi}`;
      const free = p.free_mib && p.free_mib[i] !== undefined ? `${fmtCount(p.free_mib[i])} MiB` : "\u2014";
      // With a backend in scope the row's state is the three-state
      // classification of the full requirement against total VRAM;
      // the weights-only projection keeps its free-VRAM wording.
      const st = p.fit_states?.[i];
      const fit = frac === 0 ? "not used"
        : st === "wddm_paged" ? `fits — ${fmtCount(p.paged_mib?.[i] ?? 0)} MiB paged`
        : st === "does_not_fit" ? "does not fit"
        : st === "fits" ? "fits"
        : p.per_gpu_fits[i] ? "fits" : "does not fit";
      const creditCells = credited ? `<td>${fmtCount(p.current_mib?.[i] ?? 0)} MiB</td><td>${fmtCount(p.additional_mib?.[i] ?? p.per_gpu_mib[i])} MiB</td>` : "";
      return `<tr><td>${esc(gname)}</td><td>${Math.round(frac * 100)}%</td><td>${fmtCount(p.per_gpu_mib[i])} MiB</td>${creditCells}<td>${free}</td><td>${fit}</td></tr>`;
    }).join("");
    const ageMs = p.snapshot_age_ms ?? null;
    // The headline chip is the worst three-state classification
    // when the projection carries one (a backend in scope).
    const worstState = (p.fit_states ?? []).includes("does_not_fit") ? "does_not_fit"
      : (p.fit_states ?? []).includes("wddm_paged") ? "wddm_paged"
      : p.fit_states?.length ? "fits" : null;
    const pagedTotal = (p.paged_mib ?? []).reduce((a, v) => a + v, 0);
    const fitChip = worstState === "wddm_paged"
      ? `<span class="chip status-tag status-transition" title="The requirement exceeds total VRAM on a WDDM GPU; the excess pages to system RAM and the start asks for confirmation.">Fits — ${fmtGb(pagedTotal / 1024)} GiB paged</span>`
      : worstState === "does_not_fit"
        ? `<span class="chip status-tag status-bad" title="The requirement exceeds the selected GPUs' total VRAM with this split.">Does not fit</span>`
        : worstState === "fits"
          ? `<span class="chip status-tag status-on" title="The backend's full requirement fits the selected GPUs with the projected split.">Fits</span>`
          : `<span class="chip status-tag ${p.fits ? "status-on" : "status-bad"}" title="${p.fits ? "The model fits the selected GPUs with the projected split." : "The model does not fit the available VRAM with this split."}">${p.fits ? "Fits" : "Does not fit"}</span>`;
    return `<div style="margin-top:10px">
      ${p.down_indices && p.down_indices.length ? `<div style="font-size:12px;color:var(--err);margin-bottom:6px" title="${p.down_indices.map((i) => `GPU ${i}`).join(", ")} ${p.down_indices.length === 1 ? "is" : "are"} down and ${p.down_indices.length === 1 ? "is" : "are"} treated as having no free VRAM.">${p.down_indices.map((i) => `GPU ${i}`).join(", ")}: down — no free VRAM</div>` : ""}
      <div style="font-size:12px">Fit: ${fitChip}<span class="info-tip" title="${p.weights_only === false ? "Projection prices the backend's full requirement: weights, KV cache, state cache, and the expert cache on the GPUs holding offloaded layers." : "Projection uses model weights only; run-time allocations add overhead."}">${icon("info")}</span></div>
      <table class="dtab" style="margin-top:8px"><thead><tr><th>GPU</th><th>Fraction</th><th>Required</th>${credited ? `<th title="The allocation this backend already holds on the GPU. Applying the split restarts the backend, freeing it first.">Current</th><th title="Required minus current: what must fit in free VRAM.">Additional</th>` : ""}<th>Free (current)</th><th>Fit</th></tr></thead><tbody>${rows}</tbody></table>
      ${p.moe_cache_mib ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">Expert cache: ${fmtGb(p.moe_cache_mib / 1024)} GiB<span class="info-tip" title="The --moe-cache-mib budget, already inside the per-GPU Required figures: it lands only on the GPUs holding offloaded layers.">${icon("info")}</span></div>` : ""}
      ${p.sys_mib ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">System RAM: ${fmtGb(p.sys_mib / 1024)} GiB<span class="info-tip" title="Routed experts under expert offload. The per-GPU figures cover the GPU-resident requirement.">${icon("info")}</span></div>` : ""}
      <div style="font-size:12px;color:var(--text2);margin-top:6px">${ageMs === null ? "Snapshot: latest" : `Snapshot age: ${Math.round(ageMs / 1000)} s`}</div></div>`;
  })() : "";
  const placeHtml = place ? `<div class="backend-form-panel placement-panel">
      <div style="font-size:12px;font-weight:600">${icon("layers")} Placement for ${esc(b.id)}</div>
      ${place.modelId
        ? ""
        : `<div style="font-size:12px;color:var(--err);margin-top:4px">Placement: unavailable<span class="info-tip" title="This backend's model file matches no configured model, so a split cannot be proposed or applied for it.">${icon("info")}</span></div>`}
      ${place.gpus.length ? `<div style="margin-top:8px">
        <span style="font-size:12px;color:var(--text2)">GPUs in the split:</span><span class="info-tip" title="The split divides the model's weights across the selected GPUs, in nvidia-smi index order. Uncheck a GPU to leave it out \u2014 for example the display GPU, whose memory Windows already uses. The split is saved to the backend's configuration and applies on its next start.">${icon("info")}</span>
        <div class="gpu-chip-row">${place.gpus.map((g) => {
          const on = !place.excluded.has(g.index);
          return `<label class="gpu-chip${on ? " on" : ""}${placeDis ? " is-disabled" : ""}" title="${esc(g.name)}"><span class="gpu-chip-top"><input type="checkbox" data-place-gpu="${g.index}" data-place-backend="${esc(b.id)}"${on ? " checked" : ""}${placeDis}> GPU ${g.index}</span><span class="gpu-chip-name">${esc(gpuShortName(g.name))}</span></label>`;
        }).join("")}</div>
      </div>`
      : `<div style="font-size:12px;color:var(--text2);margin-top:6px">GPUs: none reported<span class="info-tip" title="No GPU telemetry yet \u2014 a split cannot be proposed until telemetry reports the GPUs.">${icon("info")}</span></div>`}
      ${projHtml}
      <div class="card-actions"><button class="btn" data-back-act="place-propose" data-back-id="${esc(b.id)}" title="Propose split"${placeDis || !place.modelId || !place.gpus.length ? " disabled" : ""}>${icon("search")} Propose split</button>
        <button class="btn" data-back-act="place-apply" data-back-id="${esc(b.id)}"${place.projection && !place.busy && !dis ? "" : " disabled"}>${icon("check")} Apply split</button>
        <button class="btn icon-only" data-back-act="place-cancel" data-back-id="${esc(b.id)}" title="Close" aria-label="Close">${icon("x")}</button></div>
    </div>` : "";
  // Basis of the memory figures (exact | catalog | estimate): the
  // explanation lives in a tooltip on the RAM line; the word itself
  // stays inline only for estimate, as the qualifier it was before.
  const basisTip = b.memory_basis === "exact"
    ? "Basis: exact — measured from the downloaded model file."
    : b.memory_basis === "catalog"
      ? "Basis: catalog — from the catalog entry for this model (used before download)."
      : b.memory_basis === "estimate"
        ? "Basis: estimate — estimated figures, not measured from the model file."
        : "";
  // Metric strip (Pete's card redesign): one row of labeled
  // single-value tiles — Model, Context, GPU RAM, System RAM, KV
  // cache, State cache. No sentences; the model tile is a VALUE (the served
  // model's short name, the file path in its tooltip) — the old
  // inline select assigned on selection with no save, so model
  // changes moved into the Edit panel (Pete's report, 2026-10-20).
  const matchedModel = models.find((m) => sameModelFile(b.model_file, m.gguf_path));
  const modelTileVal = b.configured
    ? `<span class="backend-model-file" title="${esc(b.model_file)}">${esc(matchedModel ? modelShortName(matchedModel) : modelFileShortName(b.model_file))}</span>`
    : `<span class="backend-model-file" title="${esc(b.model_file)}">${esc(b.model_file)}</span>`;
  const stripTiles: [string, string][] = [["Model", modelTileVal]];
  // Advanced flags mode (tracker #60): the structured readouts the
  // mode supersedes are not presented as authoritative — the
  // context value is decoded from flags the structured form no
  // longer owns, so the tile shows an em dash with the explanation
  // in its tooltip, and the KV estimate (derived from that
  // context) is omitted. The raw flags line below carries the
  // state itself.
  if (b.n_ctx !== undefined) stripTiles.push(["Context", b.flags_advanced ? `<span title="Set by the raw flags in advanced flags mode">&#8212;</span>` : fmtCount(b.n_ctx)]);
  if (b.gpu_ram_mib !== undefined) {
    stripTiles.push(["GPU RAM", `${fmtGb(b.gpu_ram_mib / 1024)} GiB`]);
    // An absent sys_ram_mib is unknown, never 0 (principle 12): the
    // tile renders only when the payload carries the figure.
    if (b.sys_ram_mib !== undefined) stripTiles.push(["System RAM", `${fmtGb(b.sys_ram_mib / 1024)} GiB`]);
    if (!b.flags_advanced && b.kv_ram_mib != null) stripTiles.push(["KV cache", `<span title="KV cache at the configured context">${fmtGb(b.kv_ram_mib / 1024)} GiB</span>`]);
    if (!b.flags_advanced && b.rs_ram_mib != null) stripTiles.push(["State cache", `<span title="Recurrent-state cache at the configured slot count">${fmtGb(b.rs_ram_mib / 1024)} GiB</span>`]);
    if (!b.flags_advanced && b.moe_cache_mib) stripTiles.push(["Expert cache", `<span title="MoE expert cache (--moe-cache-mib) priced into the requirement">${fmtGb(b.moe_cache_mib / 1024)} GiB</span>`]);
  }
  // The three-state fit verdict for the saved configuration, from
  // the server's requirement model (the start gate's numbers):
  // one chip, with the paged amount when the fit is a WDDM paged
  // fit. Absent when the server could not judge the backend.
  const fitHtml = b.fit ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">Fit: ${b.fit.state === "wddm_paged"
    ? `<span class="chip status-tag status-transition">Fits — ${fmtGb(b.fit.paged_mib / 1024)} GiB paged</span>`
    : b.fit.state === "does_not_fit"
      ? `<span class="chip status-tag status-bad">Does not fit</span>`
      : `<span class="chip status-tag status-on">Fits</span>`}</div>` : "";
  // The strip is a wrapping flex row (styles.css .backend-strip),
  // never a single-row grid sized to the tile count: bare fr
  // tracks cannot shrink below their tiles' min-content widths,
  // so the full tile set (Model through State cache) overflowed
  // the card and the last tile painted outside it (Pete's
  // report, 2026-10-22). Wrapping grows the card to fit; the
  // Model tile keeps its double share of a shared row.
  const stripHtml = `<div class="backend-strip">
      ${stripTiles.map(([k, v], i) => `<div class="kv${i === 0 ? " backend-strip-model" : ""}"><span class="k">${k}</span><span class="v" style="font-size:13px">${v}</span></div>`).join("")}
    </div>`;
  // Details expandable: everything demoted off the closed card —
  // State/PID/Uptime/Batch, the configured-split percentages, and
  // the memory basis (its explanation rides the info-tip). Open
  // state lives in the wire layer, so poll repaints keep it.
  // State is not repeated here — the header's state chip carries it
  // (numbers and states live in one place, principle 6). The
  // section renders BELOW the actions bar in the card template
  // (Pete's declutter, 2026-10-20).
  const detailKv: [string, string][] = [
    ["PID", b.pid !== null ? String(b.pid) : "—"],
    ["Uptime", fmtUptime(b.uptime_secs)],
  ];
  // Batch is a structured flags readout like Context: omitted in
  // advanced flags mode (tracker #60) rather than shown as fact.
  if (!b.flags_advanced && b.n_batch != null) detailKv.push(["Batch", fmtCount(b.n_batch)]);
  // Tool-call probe state (MCP design §7.2): the persisted probe
  // outcome, or "not measured" when the backend has never been
  // probed (principle 12 — absence is not a measured state). It
  // rendered on the card face until the 2026-10-20 declutter
  // moved it here; the probe itself runs from the menu's Test
  // tool calls item. Supervisor-only orphan rows carry no probe
  // and no tile.
  if (b.configured) detailKv.push(["Tool calls", b.tool_call_probe ? b.tool_call_probe.outcome : "not measured"]);
  // Offload Layers: the saved expert-offload position, from the
  // same offload info the Edit panel's slider renders. A model
  // with no routed experts has no offload position to report —
  // the value says so plainly, never 0 (principle 12).
  if (b.configured) {
    detailKv.push(["Offload Layers", b.offload
      ? b.offload.layers <= 0
        ? `Off (0 of ${b.offload.moe_layers})`
        : b.offload.layers >= b.offload.moe_layers
          ? `Full (${b.offload.moe_layers} of ${b.offload.moe_layers})`
          : `${b.offload.layers} of ${b.offload.moe_layers}`
      : "Not applicable"]);
  }
  // Latest log lines: the stored tail for this backend (the same
  // get_logs records behind View all logs, with the
  // failed block's stderr preference). The block renders only
  // once a fetch has landed — an unfetched tail omits it
  // (principle 13); a fetched-but-empty tail states the fact.
  // The caption carries its own View all logs link to the
  // Logs view filtered to this backend (same href as the
  // card menu's item), so the full set is one click away.
  const detailTail = logTails.get(b.id);
  const logLinesHtml = detailTail === undefined ? "" : `<div style="margin-top:10px">
      <div style="font-size:11px;color:var(--text3);display:flex;align-items:baseline;gap:8px">Latest log lines<span class="info-tip" title="The backend's most recent log lines, newest last — the same records the Logs view shows for this backend.">${icon("info")}</span><a href="#/logs?scope=${esc(encodeURIComponent(b.id))}&follow=1" style="margin-left:auto">View all logs</a></div>
      ${detailTail.length ? `<div class="logstream backend-logtail" style="margin-top:4px">${detailTail.map(logLineHtml).join("")}</div>` : `<div style="font-size:12px;color:var(--text2);margin-top:4px">None recorded</div>`}
    </div>`;
  const detailsHtml = detailsOpen ? `<div class="backend-details">
      <div class="backend-detail-grid">
        ${detailKv.map(([k, v]) => `<div class="kv"><span class="k">${k}</span><span class="v" style="font-size:13px">${esc(v)}</span></div>`).join("")}
      </div>
      ${splitEntries.length ? `<div style="font-size:12px;color:var(--text2);margin-top:6px;overflow-wrap:anywhere">Configured split: ${splitSummary}</div>` : ""}
      ${basisTip ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">Basis: ${esc(b.memory_basis ?? "")}<span class="info-tip" title="${esc(basisTip)}">${icon("info")}</span></div>` : ""}
      ${logLinesHtml}
    </div>` : "";
  // Advanced flags mode (tracker #60): the saved raw flags ARE
  // the configuration the operator owns in this mode, so they
  // render verbatim in Label: Value form in place of the
  // structured readouts the mode supersedes. An empty string
  // renders no line (principle 13); the header chip carries the
  // mode itself.
  const rawFlagsHtml = b.flags_advanced && b.raw_flags
    ? `<div class="backend-raw-flags" style="font-size:12px;color:var(--text2);margin-top:8px">Flags: <span style="font-family:var(--mono);color:var(--text)">${esc(b.raw_flags)}</span></div>`
    : "";
  // The supervisor's recorded failure reason, in the failed
  // state only: it names the cause (e.g. the device-unavailable
  // fail-fast verdict) the state chip cannot. Principle 9 form —
  // a noun label and the value, no sentence.
  const failureHtml = st === "failed" && b.failure_reason
    ? `<div style="font-size:12px;color:var(--err);margin-top:6px">Failure: ${esc(b.failure_reason)}</div>`
    : "";
  const detailsBtn = `<button class="btn icon-only" data-back-act="details" data-back-id="${esc(b.id)}" aria-expanded="${detailsOpen}" title="Details" aria-label="Details">${icon(detailsOpen ? "chevD" : "chevR")}</button>`;
  return `<div class="card backend-card" id="card-backend-${esc(b.id)}">
      <h3 class="backend-card-head">${lifecycleDot(st)} ${esc(b.id)} <span class="port">:${b.port}</span>
        <span class="spacer" style="flex:1"></span>
        ${b.configured ? "" : `<span class="chip status-tag status-off" title="${esc(orphanText)}">not in config</span>`}
        ${b.configured ? `<span class="chip status-tag ${b.enabled ? "status-on" : "status-off"}">${b.enabled ? "enabled" : "disabled"}</span>` : ""}
        ${b.adopted ? `<span class="chip status-tag status-on" title="${esc(adoptedText)}">adopted</span>` : ""}
        ${b.flags_advanced ? `<span class="chip status-tag" title="Advanced flags mode: this backend's launch flags are edited as raw text in the Flags panel, and the structured flags form is disabled for it.">advanced flags</span>` : ""}
        ${b.service_managed ? `<span class="chip status-tag status-off" title="Runs as a Windows service (SCM)">service</span>` : ""}
        ${lifecycleChip(st, { transition: b.transition ?? null, failure: b.failure_reason ?? null, detail: b.pid !== null ? `pid ${b.pid}` : undefined })}</h3>
      ${failureHtml}
      ${stripHtml}
      ${fitHtml}
      ${gpuHtml}
      ${statsHtml}
      ${rawFlagsHtml}
      ${offloadStatusHtml}
      ${mtpStatusHtml}
      ${flagsHtml}
      ${placeHtml}
      <div class="card-actions backend-actions">${detailsBtn}
        ${actions}
        ${advancedMenu}</div>
      <div class="back-msg" style="margin-top:8px;font-size:12px">${msgHtml}</div>
      ${detailsHtml}
      ${logsBlock}
      ${argv}
    </div>`;
}

export function renderBackends(): string {
  return `${topbar("Backends", `<button class="btn" id="backAdd" title="Add backend">${icon("plus")} Add backend</button> <button class="btn icon-only" id="backRefresh" title="Refresh" aria-label="Refresh">${icon("restart")}</button>`)}
  <div class="view"><div id="backBanner"></div>
  <div id="backOverview"></div>
  <div style="display:flex;gap:10px;align-items:center;margin-bottom:10px;flex-wrap:wrap">
    <label style="font-size:12px;color:var(--text2)">Sort
      <select id="backSortSel" style="${SELECT_STYLE}">
        <option value="name">Name</option>
        <option value="port">Port</option>
        <option value="state">State</option>
        <option value="requests">Requests (1h)</option>
      </select></label>
    <label style="font-size:12px;color:var(--text2)">State
      <select id="backStateSel" style="${SELECT_STYLE}">
        <option value="all">All</option>
        ${LIFECYCLE_STATES.filter((s) => s !== "not_installed").map((s) => `<option value="${s}">${esc(META[s].label.charAt(0).toUpperCase() + META[s].label.slice(1))}</option>`).join("")}
      </select></label>
    <input id="backFilter" type="text" placeholder="Filter by name, group, or model file" style="background:var(--panel2);border:1px solid var(--line);border-radius:6px;color:var(--text);padding:5px 8px;font-size:12px;min-width:200px">
  </div>
  <div id="backPlan"></div>
  <div id="backAddForm"></div>
  <div id="backBody">${loadingHtml("Loading backends\u2026")}</div></div>`;
}

export async function wireBackends(root: HTMLElement): Promise<() => void> {
  const openArgv = new Set<string>();
  /** Backend whose advanced-options menu is open (at most one). */
  const openMenus = new Set<string>();
  /** Backends whose Details expandable is open. */
  const openDetails = new Set<string>();
  const argvCache = new Map<string, string[]>();
  const msgs = new Map<string, CardNote>();
  /** Recent log lines for failed backends, fetched lazily after paint. */
  const logTails = new Map<string, LogRow[]>();
  /** Ids whose log tail has been fetched (or is in flight) this mount. */
  const tailsWanted = new Set<string>();
  /** Ids with a Details-driven tail refetch in flight: the
   *  open-Details path refetches on each load cycle instead of
   *  once per mount, so it tracks its own in-flight set. */
  const tailsFetching = new Set<string>();
  /** Backend ids with an action invoke in flight; their card buttons
   *  render disabled (the set outlives any single repaint). */
  const inFlight = new Set<string>();
  /** Backends whose predicted-throughput refresh (a Measure
   *  run started from the card) is in flight, by start time.
   *  Held outside the repainted DOM like the other card state,
   *  so poll repaints keep the Measuring state; a refresh
   *  deliberately does NOT join inFlight — a measurement takes
   *  minutes and must not disable the card or stall the poll. */
  const predictRefreshing = new Map<string, number>();
  /** The 1 s elapsed ticker for in-flight prediction
   *  refreshes (the Optimize view's pattern). */
  let predictTimer: number | null = null;
  const stopPredictTicker = (): void => {
    if (predictTimer !== null) { window.clearInterval(predictTimer); predictTimer = null; }
  };
  const startPredictTicker = (): void => {
    if (predictTimer !== null) return;
    predictTimer = window.setInterval(() => {
      if (!predictRefreshing.size) { stopPredictTicker(); return; }
      root.querySelectorAll<HTMLElement>("[data-pred-elapsed]").forEach((el) => {
        const startedAt = predictRefreshing.get(el.getAttribute("data-pred-elapsed") ?? "");
        if (startedAt !== undefined) {
          el.textContent = fmtElapsed(Math.max(0, Math.floor((Date.now() - startedAt) / 1000)));
        }
      });
    }, 1000);
  };
  let lastRows: BackendRow[] = [];
  /** Measured GPU residency by backend id (telemetry attribution). */
  let residency = new Map<string, { gpu: number; name: string; mib: number | null }[]>();
  /** Backends list controls (sort / state filter / text filter),
   *  held outside the repainted DOM. */
  let backSort = "name";
  let backFilter = "";
  let backState = "all";
  /** Registered models for the per-card model dropdowns. */
  let lastModels: ModelRow[] = [];
  /** Gateway groups + port, for the add-backend form. */
  let lastGroups: GatewayGroup[] = [];
  let gatewayPort = 4000;
  /** Backend ids whose Remove button is armed for confirmation. */
  const confirmRemove = new Set<string>();
  /** Backends whose WDDM-paged Start is armed (first click done);
   *  the second click starts with the overcommit confirmation.
   *  Cleared on state change and whenever the fit stops being a
   *  paged fit — the delete-confirm lifecycle. */
  const armedWddmStart = new Set<string>();
  /** Backend ids whose guardrail Override button is armed for
   *  confirmation (the confirm button names the exact position). */
  const confirmOverride = new Set<string>();
  /** Gateway activation plan (saved changes awaiting a gateway
   *  restart), painted into #backPlan above the add form. */
  let lastPlan: ActivationPlanData | null = null;
  const planUi = newPlanUi();
  const paintPlan = (): void => {
    const host = root.querySelector("#backPlan");
    if (host) host.innerHTML = activationPlanHtml(lastPlan, planUi, "summary");
  };
  let disposed = false;
  /** Load sequence: only the most recently started load may paint. */
  let loadSeq = 0;
  /** The rows as the server last reported them (list_backends, or a
   *  fresher shared-store snapshot). `lastRows` is these rows with
   *  the effective lifecycle state applied (the store's pending
   *  overlay), so every card, chip, filter and button rule reads one
   *  state — a restart started on any surface shows "restarting…". */
  let wireRows: BackendRow[] = [];
  const effRow = (row: BackendRow): BackendRow => {
    const st = serviceStore.backendState(row);
    return { ...row, state: st, lifecycle: st, transition: serviceStore.transitionOf(`backend:${row.id}`, row.transition ?? null) };
  };
  const lcSignature = (rows: BackendRow[]): string =>
    rows.map((r) => `${r.id}:${r.state}:${r.transition?.kind ?? ""}:${r.transition?.origin ?? ""}`).join("|");

  const paint = (): void => {
    const b = root.querySelector("#backBody");
    if (!b) return;
    const sorted = (list: BackendRow[]): BackendRow[] => {
      const c = [...list];
      if (backSort === "port") c.sort((a, z) => a.port - z.port);
      else if (backSort === "requests") c.sort((a, z) => (z.requests_1h ?? -1) - (a.requests_1h ?? -1));
      else if (backSort === "state") c.sort((a, z) => a.state.localeCompare(z.state) || a.id.localeCompare(z.id));
      else c.sort((a, z) => a.id.localeCompare(z.id));
      return c;
    };
    // Overview list: one line per backend (all rows, ignoring the
    // list filter), with column headings, divider lines, and a
    // status button that scrolls to the backend's card. The GPUs
    // column names only GPUs where the backend holds at least 1 GiB
    // measured — under that it holds only a CUDA context there.
    const ov = root.querySelector("#backOverview");
    if (ov) {
      ov.innerHTML = lastRows.length ? `<div class="ovlist ov-b">
        <div class="ov-head"><span></span><span>Backend</span><span>State</span><span>Group</span><span>Port</span><span>GPUs (measured)</span><span>Requests (1h)</span><span>Errors (1h)</span></div>
        ${sorted(lastRows).map((row) => {
          const res = residency.get(row.id) ?? [];
          // Unmeasured rows (mib null) are neither residency nor
          // context: the column says "not measured" rather than
          // claiming either.
          const resident = res.filter((r) => r.mib !== null && r.mib >= 1024);
          const gpuTxt = resident.length ? resident.map((r) => `GPU ${r.gpu}`).join(", ")
            : res.some((r) => r.mib !== null) ? "context only"
            : res.length ? "not measured" : "\u2014";
          const rst = fromWire(row.lifecycle ?? row.state);
          const rlabel = lifecycleLabel(rst, row.transition ?? null);
          const rdot = META[rst].dot;
          const btn = ovStatusBtn(rdot, rdot === "ok" ? "check" : rdot === "err" ? "err" : META[rst].isTransition || rst === "degraded" ? "clock" : "pause", `card-backend-${row.id}`, `${row.id}: ${rlabel} — go to card`);
          return `<div class="ov-row">${btn}<span class="ov-name">${esc(row.id)}</span><span class="ov-dim">${esc(rlabel)}</span><span class="ov-dim">${esc(row.group ?? "ungrouped")}</span><span class="ov-dim">${row.port}</span><span class="ov-dim">${esc(gpuTxt)}</span><span class="ov-dim">${row.requests_1h !== undefined ? fmtCount(row.requests_1h) : "\u2014"}</span><span class="ov-dim">${row.errors_1h !== undefined ? fmtCount(row.errors_1h) : "\u2014"}</span></div>`;
        }).join("")}</div>` : "";
    }
    const f = backFilter.trim().toLowerCase();
    const viewRows = lastRows.filter((row) =>
      (backState === "all" || row.state === backState) &&
      (!f || row.id.toLowerCase().includes(f) || (row.group ?? "").toLowerCase().includes(f) || row.model_file.toLowerCase().includes(f)));
    // Cards are grouped by gateway group (Pete's request, 2026-10-15):
    // a section per group in the order groups first appear across
    // the rows (backend config order), ungrouped rows and orphans
    // last under "Ungrouped".
    const sections: { name: string | null; rows: BackendRow[] }[] = [];
    const byKey = new Map<string, { name: string | null; rows: BackendRow[] }>();
    for (const row of viewRows) {
      const key = row.group ?? "";
      let sec = byKey.get(key);
      if (!sec) {
        sec = { name: row.group ?? null, rows: [] };
        byKey.set(key, sec);
        sections.push(sec);
      }
      sec.rows.push(row);
    }
    const ordered = [
      ...sections.filter((s) => s.name !== null),
      ...sections.filter((s) => s.name === null),
    ];
    b.innerHTML = ordered.map((sec) => `
      <div class="group-box">
      <div class="sec-title" style="margin:0 0 10px">${sec.name !== null ? `Group: ${esc(sec.name)}` : "Ungrouped"} <span style="color:var(--text3);font-weight:400">— ${sec.rows.length} backend${sec.rows.length === 1 ? "" : "s"}</span></div>
      <div class="grid2">${sorted(sec.rows).map((row) => {
        const mdl = flagsFor === row.id ? lastModels.find((m) => sameModelFile(row.model_file, m.gguf_path)) : undefined;
        const flagsArg: FlagsDraft | null = flagsFor === row.id
          ? { ctx: flagsCtx, batch: flagsBatch, ctxMax: mdl?.ctx_max ? mdl.ctx_max : null, modelId: mdl?.id ?? null, offloadLayers: flagsOffloadLayers, offloadPendingLayers: offloadPending?.backendId === row.id ? offloadPending.layers : null, mtpPosition: flagsMtpPosition, moeCacheOn: flagsMoeCacheOn, moeCacheMib: flagsMoeCacheMib, parallel: flagsParallel, cacheSel: flagsCacheSel, cachePreview: flagsCachePreview, advanced: row.flags_advanced === true, rawText: flagsRawText, rawWarnings: flagsRawWarnings, editModelId: flagsEditModelId }
          : null;
        const pmdl = placeFor === row.id ? lastModels.find((m) => sameModelFile(row.model_file, m.gguf_path)) : undefined;
        const placeArg: PlaceCardState | null = placeFor === row.id
          ? {
            gpus: lastGpuList.map((g) => ({ index: Number(g.id), name: g.name })),
            excluded: placeExcluded,
            modelId: pmdl?.id ?? null,
            projection: placeProjection && placeProjection.backendId === row.id ? placeProjection.data : null,
            busy: placeBusy,
          }
          : null;
        return backendCard(row, openArgv, openMenus, argvCache, msgs, logTails, inFlight, lastModels, confirmRemove, residency, new Map(lastGpuList.map((g) => [Number(g.id), g.name])), flagsArg, flagsFor === row.id ? predictedHtml : "", placeArg, new Map(lastGpuList.map((g) => [Number(g.id), g.vram_total_gb * 1024])), openDetails.has(row.id), predictRefreshing, confirmOverride, armedWddmStart);
      }).join("")}</div>
      </div>`).join("");
    if (!viewRows.length && lastRows.length) b.innerHTML = emptyHtml("No backends match the current filter.");
  };

  const backSortSel = root.querySelector("#backSortSel") as HTMLSelectElement | null;
  const backStateSel = root.querySelector("#backStateSel") as HTMLSelectElement | null;
  const backFilterInput = root.querySelector("#backFilter") as HTMLInputElement | null;
  backSortSel?.addEventListener("change", () => { backSort = backSortSel.value; paint(); });
  backStateSel?.addEventListener("change", () => { backState = backStateSel.value; paint(); });
  backFilterInput?.addEventListener("input", () => { backFilter = backFilterInput.value; paint(); });

  /** Lazily fetch the last log lines for the cards that show
   *  them, and repaint. One stored tail (up to 5 lines) serves
   *  two blocks: a failed backend's diagnostic block — fetched
   *  once per mount, as it has always been — and the Details
   *  section's latest-lines block, fetched while Details are
   *  open and refreshed on each load cycle, repainting only when
   *  the tail actually changed. get_logs takes the bare process
   *  id (the same scope the Logs view sends); records arrive
   *  newest-first. Stderr lines are preferred — that is where
   *  llama-server reports load failures. */
  const fetchLogTails = async (): Promise<void> => {
    const rows = lastRows;
    let fetched = false;
    for (const row of rows) {
      const detailsWants = openDetails.has(row.id);
      if (row.state !== "failed" && !detailsWants) continue;
      if (detailsWants ? tailsFetching.has(row.id) : tailsWanted.has(row.id)) continue;
      if (detailsWants) tailsFetching.add(row.id); else tailsWanted.add(row.id);
      try {
        const r = await getLogs(row.id, { limit: 50 });
        if (disposed) return;
        if (r.error) { if (!detailsWants) tailsWanted.delete(row.id); continue; }
        const stderr = r.data.lines.filter((x) => x.stream === "stderr");
        const source = stderr.length ? stderr : r.data.lines;
        const tail = source.slice(0, 5).reverse();
        const prev = logTails.get(row.id);
        const changed = !prev || prev.length !== tail.length
          || prev.some((p, i) => p.ts !== tail[i].ts || p.line !== tail[i].line);
        logTails.set(row.id, tail);
        if (changed) fetched = true;
      } finally {
        tailsFetching.delete(row.id);
      }
    }
    // Repaint only if these are still the current rows (a reload or an
    // error paint may have superseded them while fetches were in flight).
    if (fetched && !disposed && lastRows === rows) paint();
  };

  const load = async (): Promise<void> => {
    const seq = ++loadSeq;
    const [r, mr, gr, pr, tr] = await Promise.all([getBackends(), getModels(), getGatewayConfig(), getActivationPlan(), getGpuTelemetry()]);
    if (disposed || seq !== loadSeq) return; // a newer load supersedes this one
    if (!pr.error && pr.data) { lastPlan = pr.data; paintPlan(); }
    if (!tr.error && tr.data) {
      lastGpuList = tr.data.gpus;
      const m = new Map<string, { gpu: number; name: string; mib: number | null }[]>();
      for (const g of tr.data.gpus) {
        for (const p of g.processes ?? []) {
          if (p.backend === "\u2014") continue;
          const arr = m.get(p.backend) ?? [];
          arr.push({ gpu: Number(g.id), name: g.name, mib: p.vram_mib });
          m.set(p.backend, arr);
        }
      }
      residency = m;
    }
    if (!mr.error && mr.data) lastModels = mr.data;
    if (!gr.error && gr.data) {
      lastGroups = gr.data.groups;
      if (gr.data.port) gatewayPort = gr.data.port;
    }
    setBanner(root, "backBanner", bannerHtml(r));
    const b = root.querySelector("#backBody");
    if (!b) return;
    if (r.error) { lastRows = []; b.innerHTML = emptyHtml(`Backends unavailable: ${r.error}`); return; }
    if (!r.data.length) {
      lastRows = [];
      b.innerHTML = emptyHtml(r.live
        ? "No backends are configured. Backends are defined in the app configuration."
        : "No backend data in sample mode.");
      return;
    }
    // Note/arming lifetimes key to the SERVER state (not the local
    // pending overlay), so a lifecycle action's own overlay does not
    // clear the note it produces.
    const prevStates = new Map(wireRows.map((row) => [row.id, fromWire(row.lifecycle ?? row.state)]));
    wireRows = r.data;
    lastRows = wireRows.map(effRow);
    // Bench adoption path (Bench design §5.2): an arm detail's
    // "Open in backend editor" sets this key before navigating
    // here; open that backend's flags form once, prefilled from
    // its saved configuration, and clear the key.
    try {
      const wanted = sessionStorage.getItem("benchEditBackend");
      if (wanted) {
        sessionStorage.removeItem("benchEditBackend");
        const row = lastRows.find((r) => r.id === wanted);
        if (row && flagsFor !== row.id) openFlagsForm(row);
      }
    } catch { /* storage unavailable */ }
    for (const row of lastRows) {
      // A note belongs to the state it was produced in; when the
      // backend's state changes, the stale note goes away.
      const prev = prevStates.get(row.id);
      const wireNow = fromWire(wireRows.find((w) => w.id === row.id)?.lifecycle ?? row.state);
      if (prev !== undefined && prev !== wireNow) { msgs.delete(row.id); confirmRemove.delete(row.id); confirmOverride.delete(row.id); armedWddmStart.delete(row.id); }
      // A config change that ends the paged fit disarms the
      // confirmation with it.
      if (row.fit?.state !== "wddm_paged") armedWddmStart.delete(row.id);
      // A backend that is no longer failed gets a fresh tail fetch if it
      // fails again — unless its Details are open, where the stored
      // tail is the expansion's latest-lines data and stays.
      if (row.state !== "failed" && !openDetails.has(row.id)) { logTails.delete(row.id); tailsWanted.delete(row.id); }
    }
    // Prune per-backend card state for ids that no longer exist
    // (code-review 5.9): a deleted backend's notes, caches, and
    // open forms must not linger — or resurface if the id is ever
    // reused.
    {
      const alive = new Set(lastRows.map((row) => row.id));
      for (const map of [msgs, argvCache, logTails] as Map<string, unknown>[]) {
        for (const k of [...map.keys()]) if (!alive.has(k)) map.delete(k);
      }
      for (const set of [confirmRemove, confirmOverride, armedWddmStart, openArgv, openDetails, inFlight, tailsWanted, tailsFetching] as Set<string>[]) {
        for (const k of [...set]) if (!alive.has(k)) set.delete(k);
      }
      if (flagsFor !== null && !alive.has(flagsFor)) flagsFor = null;
      if (offloadPending !== null && !alive.has(offloadPending.backendId)) offloadPending = null;
      if (placeFor !== null && !alive.has(placeFor)) { placeFor = null; placeProjection = null; }
    }
    // While the offload slider is focused (mid-drag), a repaint
    // would recreate the range input and break the drag; the
    // draft is synced to wire state, so skipping one repaint
    // loses nothing (same rule as the model rename input). The
    // advanced flags textarea (tracker #60) follows the same
    // rule: recreating it mid-edit would lose the caret.
    const sliderFocused = flagsFor !== null
      && (document.activeElement as HTMLElement | null)?.hasAttribute?.("data-offload-slider");
    const rawFocused = flagsFor !== null
      && (document.activeElement as HTMLElement | null)?.hasAttribute?.("data-flags-raw");
    if (!sliderFocused && !rawFocused) paint();
    void fetchLogTails();
  };

  const refreshBtn = root.querySelector("#backRefresh");
  const onRefresh = (): void => { void load(); };
  refreshBtn?.addEventListener("click", onRefresh);

  /* ---------------- Flags editor ----------------
   * Rendered INSIDE the backend's own card (backendCard, from the
   * draft state below) — the previous placement above the list read
   * as if nothing had happened when Edit was clicked. The
   * draft values live outside the repainted DOM, so the poll repaint
   * cannot clobber an open form: paint() re-renders the form from
   * this state, and field events sync back into it. */
  let flagsFor: string | null = null;
  let flagsCtx = 32768;
  let flagsBatch = 512;
  /** The Edit panel's model draft: the registered model chosen
   *  in the panel's Model select ("" when the open backend's file
   *  matches no registered model). Synced on the select's change
   *  event and applied through model_assign only when the panel
   *  saves; closing the panel discards it. */
  let flagsEditModelId = "";
  /** Offload slider draft position for the open flags form (MoE
   *  layers on system RAM); null when the backend's model is not
   *  offload-capable. */
  let flagsOffloadLayers: number | null = null;
  /** A released offload position the guardrail blocks, awaiting the
   *  operator's decision in the flags form (save with override, or
   *  snap to the nearest verified position); null otherwise. */
  let offloadPending: { backendId: string; layers: number } | null = null;
  /** True while a slider commit (backend_set_offload) is in
   *  flight; the slider ignores further commits until it lands. */
  let offloadCommitting = false;
  /** The MTP draft control's chosen value while the flags form
   *  is open (0 = Off, 1..4 = draft tokens); null when the open
   *  backend's model is not MTP-capable. */
  let flagsMtpPosition: number | null = null;
  /** True while an MTP draft commit (backend_set_mtp) is in
   *  flight; the control ignores further commits until it
   *  lands. */
  let mtpCommitting = false;
  /** The expert-cache control's draft while the flags form is
   *  open: the switch state, and the chosen size — retained
   *  while the switch is off so toggling back on restores it;
   *  null when the backend has never carried a cache and none
   *  has been chosen. */
  let flagsMoeCacheOn = false;
  let flagsMoeCacheMib: number | null = null;
  /** True while an expert-cache commit (backend_set_moe_cache)
   *  is in flight; the control ignores further commits until it
   *  lands. */
  let moeCacheCommitting = false;
  /** The Parallel slots field's draft selection while the flags
   *  form is open: the explicit --parallel count, or null for the
   *  unset state (llama.cpp's default, auto). Unlike the MTP
   *  position this exists for every backend — the field is not
   *  capability-gated. */
  let flagsParallel: number | null = null;
  /** True while a parallel commit (backend_set_parallel) is in
   *  flight; the field ignores further commits until it lands. */
  let parallelCommitting = false;
  /** The KV cache / flash-attention draft selection while the
   *  flags form is open (the decoded saved selection at open);
   *  commits through backend_set_cache on change, like MTP. */
  let flagsCacheSel: CacheSelectionLike | null = null;
  /** The last cache preview priced for the current draft
   *  (predicted footprint + predicted-best ladder), held in wire
   *  state so poll repaints re-render it. */
  let flagsCachePreview: CachePreview | null = null;
  let cacheCommitting = false;
  let cachePreviewSeq = 0;
  let cachePreviewTimer: number | null = null;
  /** The advanced flags editor's draft text (tracker #60): the
   *  open backend's raw flags as edited, synced on every input
   *  event so poll repaints re-render the textarea from here.
   *  Initialized from the row's saved string when the form opens —
   *  opening the editor performs no write. */
  let flagsRawText = "";
  /** The draft's validation warnings, recomputed live as the
   *  operator types (the save op's own list replaces them after a
   *  save — the server is the authority). */
  let flagsRawWarnings: FlagWarningLike[] = [];
  /** The open flags form's predicted-throughput line
   *  (predicted-throughput design §6): the last lookup's
   *  rendered line for the CURRENT draft, held in wire state
   *  so poll repaints re-render it and a lookup in flight
   *  never blanks it (§6.2). Empty when the draft matches no
   *  measurement exactly or nearly. */
  let predictedHtml = "";
  let predictedSeq = 0;
  let predictedTimer: number | null = null;
  const runPredictedLookup = async (): Promise<void> => {
    const id = flagsFor;
    if (!id) return;
    const row = lastRows.find((r) => r.id === id);
    if (!row) return;
    const seq = ++predictedSeq;
    const req: MeasurementLookupRequest = row.flags_advanced
      ? { backend_id: id, raw_flags: flagsRawText }
      : {
          backend_id: id,
          n_ctx: flagsCtx,
          n_batch: flagsBatch,
          offload_layers: flagsOffloadLayers,
          mtp_position: flagsMtpPosition,
          parallel_slots: flagsParallel,
        };
    const r = await measurementLookup(req);
    if (disposed || seq !== predictedSeq || flagsFor !== id) return;
    predictedHtml = r.data ? predictedLineHtml(r.data.exact, r.data.near) : "";
    // Update the open form's slot in place: a repaint here
    // could disturb an in-progress slider drag or text edit.
    const slot = root.querySelector("[data-predicted-slot]");
    if (slot) slot.innerHTML = predictedHtml;
  };
  /** Debounced lookup trigger (§6.2, ~300 ms): fires on the
   *  same events that mutate the draft. */
  const schedulePredictedLookup = (): void => {
    if (predictedTimer !== null) window.clearTimeout(predictedTimer);
    predictedTimer = window.setTimeout(() => {
      predictedTimer = null;
      void runPredictedLookup();
    }, 300);
  };
  const runCachePreview = async (): Promise<void> => {
    const id = flagsFor;
    if (!id || !flagsCacheSel) return;
    const seq = ++cachePreviewSeq;
    // The Expert cache control's DRAFT prices with the preview
    // (its saved token does not change until the control's own
    // commit lands, and a cache change alone must move the fit
    // figures immediately — Pete's Oct 23, 2026 report).
    const moeDraft = flagsMoeCacheOn
      ? (flagsMoeCacheMib ?? MOE_CACHE_DEFAULT_MIB)
      : 0;
    const r = await backendCachePreview(id, { nCtx: flagsCtx, slots: flagsParallel ?? undefined, sel: flagsCacheSel, moeCacheMib: moeDraft });
    if (disposed || seq !== cachePreviewSeq || flagsFor !== id || r.error || !r.data) return;
    flagsCachePreview = r.data;
    const slot = root.querySelector("[data-cache-slot]");
    if (slot && flagsCacheSel) slot.innerHTML = cachePreviewHtml(flagsCachePreview, flagsCacheSel, `data-cache-best data-cache-backend="${esc(id)}"`);
  };
  const scheduleCachePreview = (): void => {
    if (cachePreviewTimer !== null) window.clearTimeout(cachePreviewTimer);
    cachePreviewTimer = window.setTimeout(() => {
      cachePreviewTimer = null;
      void runCachePreview();
    }, 300);
  };
  const closeFlagsForm = (): void => {
    flagsFor = null;
    offloadPending = null;
    flagsMoeCacheOn = false;
    flagsMoeCacheMib = null;
    flagsCacheSel = null;
    flagsCachePreview = null;
    cachePreviewSeq += 1;
    if (cachePreviewTimer !== null) { window.clearTimeout(cachePreviewTimer); cachePreviewTimer = null; }
    predictedHtml = "";
    predictedSeq += 1;
    if (predictedTimer !== null) { window.clearTimeout(predictedTimer); predictedTimer = null; }
    paint();
  };
  const openFlagsForm = (row: BackendRow): void => {
    if (flagsFor === row.id) { closeFlagsForm(); return; }
    flagsFor = row.id;
    flagsEditModelId = lastModels.find((m) => sameModelFile(row.model_file, m.gguf_path))?.id ?? "";
    flagsCtx = row.n_ctx ?? 32768;
    flagsBatch = row.n_batch ?? 512;
    flagsOffloadLayers = row.offload ? row.offload.layers : null;
    flagsMtpPosition = row.mtp ? mtpPosition(row.mtp) : null;
    flagsMoeCacheOn = row.moe_cache ? row.moe_cache.mib != null : false;
    flagsMoeCacheMib = row.moe_cache ? row.moe_cache.mib : null;
    flagsParallel = row.parallel ? row.parallel.slots : null;
    flagsCacheSel = row.cache ? { k: row.cache.k, v: row.cache.v, flash_attn: row.cache.flash_attn } : { k: "f16", v: "f16", flash_attn: "auto" };
    flagsCachePreview = null;
    offloadPending = null;
    // The advanced editor opens on the backend's CURRENT saved
    // raw flags — a pure read of the row payload; opening writes
    // nothing (tracker #60, entry performs no write).
    flagsRawText = row.raw_flags ?? "";
    flagsRawWarnings = rawFlagWarnings(flagsRawText);
    predictedHtml = "";
    predictedSeq += 1;
    paint();
    schedulePredictedLookup();
    scheduleCachePreview();
  };
  /** Keep the draft in step with the in-card form fields. Preset
   *  selects repaint (to show or hide the Custom numeric input);
   *  custom inputs only update the draft. Registered for both
   *  "change" and "input" so typing is captured before any repaint. */
  const syncFlagsDraft = (ev: Event): void => {
    const t = ev.target as HTMLElement;
    const field = t.getAttribute("data-flags-field");
    if (!field) return;
    const draft = { ctx: flagsCtx, batch: flagsBatch, parallel: flagsParallel };
    const repaint = applyFlagsField(draft, field, (t as HTMLInputElement).value, ev.type === "change");
    flagsCtx = draft.ctx;
    flagsBatch = draft.batch;
    flagsParallel = draft.parallel;
    if (repaint) paint();
    schedulePredictedLookup();
    scheduleCachePreview();
  };
  root.addEventListener("change", syncFlagsDraft);
  root.addEventListener("input", syncFlagsDraft);
  /** The Edit panel's Model select sync: the choice is a DRAFT,
   *  recorded here and applied through model_assign only when
   *  the panel saves (Pete's report, 2026-10-20 — the card-face
   *  dropdown this replaces assigned on selection). No repaint:
   *  the select already shows the chosen option, and the draft
   *  re-renders it on the next poll repaint. */
  const syncEditModel = (ev: Event): void => {
    const t = ev.target as HTMLSelectElement;
    if (!t.hasAttribute("data-edit-model")) return;
    if (t.getAttribute("data-edit-model") !== flagsFor) return;
    flagsEditModelId = t.value;
  };
  root.addEventListener("change", syncEditModel);
  /** The advanced editor's textarea sync (tracker #60): keep the
   *  draft text and its live warnings in wire state. The warning
   *  list updates in place — no repaint, so typing is never
   *  disturbed (the poll repaint is also skipped while the
   *  textarea is focused; see load()). */
  const syncRawDraft = (ev: Event): void => {
    const t = ev.target as HTMLTextAreaElement;
    if (!t.hasAttribute("data-flags-raw")) return;
    if (t.getAttribute("data-flags-raw") !== flagsFor) return;
    flagsRawText = t.value;
    flagsRawWarnings = rawFlagWarnings(flagsRawText);
    const box = t.closest(".backend-card")?.querySelector("[data-flags-warnings]");
    if (box) box.innerHTML = flagsRawWarningsHtml(flagsRawWarnings);
    schedulePredictedLookup();
  };
  root.addEventListener("input", syncRawDraft);

  /* ---------------- Offload slider ----------------
   * Part of the flags form when the backend's model is
   * offload-capable. "input" events move the draft and update the
   * readout in place — no repaint, so the drag is never disturbed;
   * the "change" event (slider release) commits through
   * backend_set_offload, which rewrites only the offload tokens
   * in the backend's raw flags. A rejected commit reverts the
   * draft to the saved position and keeps the form open with the
   * error on the card. */
  const syncOffloadDraft = (ev: Event): void => {
    const t = ev.target as HTMLInputElement;
    if (!t.hasAttribute("data-offload-slider")) return;
    const id = t.getAttribute("data-offload-backend") ?? "";
    if (id !== flagsFor) return;
    const row = lastRows.find((r) => r.id === id);
    if (!row?.offload) return;
    const n = Number(t.value);
    flagsOffloadLayers = n;
    schedulePredictedLookup();
    const card = t.closest(".backend-card");
    if (!card) return;
    const o = row.offload;
    paintOffloadReadout(card, o, n);
    // Keep a pending guardrail decision in step with the draft:
    // its text follows a still-blocked position, and the row hides
    // while the draft sits on a verified position (the release
    // handler re-decides from the final position).
    if (offloadPending?.backendId === id) {
      const dec = card.querySelector("[data-offload-decision]") as HTMLElement | null;
      if (dec) {
        if (offloadIsUntested(o, n)) {
          offloadPending.layers = n;
          dec.style.display = "";
          const dt = dec.querySelector("[data-offload-decision-text]");
          if (dt) dt.textContent = `${n} of ${o.moe_layers} layers is unverified on this GPU`;
        } else {
          dec.style.display = "none";
        }
      }
    }
  };
  /** Invoke backend_set_offload for one position and report the
   *  outcome on the card. An override save's message names the
   *  override; any other save keeps the token-form message. On
   *  error the message carries the server's error and the pending
   *  decision (if any) is left for the caller to restore. Returns
   *  true when the position was saved. */
  const saveOffloadPosition = async (id: string, n: number, allowOverride: boolean): Promise<boolean> => {
    if (offloadCommitting) return false;
    const row = lastRows.find((r) => r.id === id);
    offloadCommitting = true;
    inFlight.add(id);
    paint();
    try {
      const r = await setBackendOffload(id, n, allowOverride);
      if (disposed) return false;
      if (r.error) {
        msgs.set(id, { text: `Offload not saved: ${r.error}`, err: true });
        paint();
        return false;
      }
      offloadPending = null;
      if (flagsFor === id) flagsOffloadLayers = n;
      msgs.set(id, {
        text: allowOverride
          ? "Offload saved with override \u2014 applies on next start."
          : `Offload saved (${row?.offload ? offloadTokenText(row.offload, n) : n}) \u2014 applies on next start.`,
        err: false,
      });
      await load();
      schedulePredictedLookup();
      return true;
    } finally {
      offloadCommitting = false;
      inFlight.delete(id);
      if (!disposed) paint();
    }
  };
  const commitOffload = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLInputElement;
    if (!t.hasAttribute("data-offload-slider")) return;
    const id = t.getAttribute("data-offload-backend") ?? "";
    const row = lastRows.find((r) => r.id === id);
    if (!row?.offload || offloadCommitting) return;
    const n = Number(t.value);
    flagsOffloadLayers = n;
    if (n === row.offload.layers) { offloadPending = null; paint(); return; } // released where it started
    // A guardrail-blocked position is NOT invoked: the form renders
    // the override/snap decision row for it instead.
    if (offloadIsUntested(row.offload, n)) {
      offloadPending = { backendId: id, layers: n };
      paint();
      return;
    }
    const ok = await saveOffloadPosition(id, n, false);
    if (!ok && !disposed) {
      // A rejected commit reverts the draft to the saved position.
      flagsOffloadLayers = row.offload.layers;
      paint();
    }
  };
  root.addEventListener("input", syncOffloadDraft);
  root.addEventListener("change", commitOffload);

  /* ---------------- MTP draft control (tracker #53) ----------------
   * A dropdown (the ctx/batch selects' idiom), so there is no
   * drag preview to sync: the "change" event does the whole
   * job — it moves the draft, updates the readouts in place,
   * and commits through backend_set_mtp, which rewrites only
   * the MTP tokens in the backend's raw flags. Value 0 commits
   * as Off. Choosing the saved value is a no-op. A rejected
   * commit reverts the draft to the saved value. The
   * --parallel conflict line follows the draft: it shows
   * whenever the value enables MTP and the SAVED flags set a
   * slot count other than 1 — the control never rewrites that
   * setting. */
  /** Invoke backend_set_mtp for one value and report the
   *  outcome on the card. Returns true when the value was
   *  saved. */
  const saveMtpPosition = async (id: string, pos: number): Promise<boolean> => {
    if (mtpCommitting) return false;
    mtpCommitting = true;
    inFlight.add(id);
    paint();
    try {
      const r = await setBackendMtp(id, pos === 0 ? null : pos);
      if (disposed) return false;
      if (r.error) {
        msgs.set(id, { text: `MTP draft not saved: ${r.error}`, err: true });
        paint();
        return false;
      }
      if (flagsFor === id) flagsMtpPosition = pos;
      msgs.set(id, {
        text: pos === 0
          ? "MTP draft turned off — applies on next start."
          : `MTP draft saved (${mtpTokenText(pos)}) — applies on next start.`,
        err: false,
      });
      await load();
      schedulePredictedLookup();
      return true;
    } finally {
      mtpCommitting = false;
      inFlight.delete(id);
      if (!disposed) paint();
    }
  };
  const changeMtp = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLSelectElement;
    if (!t.hasAttribute("data-mtp-select")) return;
    const id = t.getAttribute("data-mtp-backend") ?? "";
    const row = lastRows.find((r) => r.id === id);
    if (!row?.mtp || mtpCommitting) return;
    const n = Number(t.value);
    flagsMtpPosition = n;
    schedulePredictedLookup();
    const card = t.closest(".backend-card");
    if (card) {
      const token = card.querySelector("[data-mtp-token]");
      if (token) token.textContent = mtpTokenText(n);
      const conflict = card.querySelector("[data-mtp-conflict]") as HTMLElement | null;
      if (conflict) {
        const show = n > 0 && row.mtp.parallel != null && row.mtp.parallel !== 1;
        conflict.style.display = show ? "" : "none";
      }
    }
    if (n === mtpPosition(row.mtp)) { paint(); return; } // the saved value chosen again
    const ok = await saveMtpPosition(id, n);
    if (!ok && !disposed) {
      // A rejected commit reverts the draft to the saved value.
      flagsMtpPosition = mtpPosition(row.mtp);
      paint();
    }
  };
  root.addEventListener("change", changeMtp);

  /* ---------------- Expert cache control ----------------
   * The switch commits immediately (the sliders' commit model):
   * on commits the drafted size (the saved size when one exists,
   * else the control's default); off commits null, and the size
   * draft is retained so toggling back on restores it. The size
   * select only drafts while the switch is off; while on, a
   * change commits the new size, and "Auto" commits with auto
   * set — the server resolves the size from priced free VRAM and
   * the draft takes the resolved value. A rejected commit
   * reverts the draft to the saved state. */
  /** Invoke backend_set_moe_cache and report the outcome on the
   *  card. Returns true when the value was saved. */
  const saveMoeCache = async (id: string, mib: number | null, auto: boolean): Promise<boolean> => {
    if (moeCacheCommitting) return false;
    moeCacheCommitting = true;
    inFlight.add(id);
    paint();
    try {
      const r = await setBackendMoeCache(id, mib, auto || undefined);
      if (disposed) return false;
      if (r.error) {
        msgs.set(id, { text: `Expert cache not saved: ${r.error}`, err: true });
        paint();
        return false;
      }
      const saved = r.data.mib;
      if (flagsFor === id) {
        flagsMoeCacheOn = saved != null;
        if (saved != null) flagsMoeCacheMib = saved;
      }
      msgs.set(id, {
        text: saved == null
          ? "Expert cache turned off — applies on next start."
          : auto
            ? `Expert cache saved (${moeCacheTokenText(saved)}) — size resolved by Auto from priced free VRAM; applies on next start.`
            : `Expert cache saved (${moeCacheTokenText(saved)}) — applies on next start.`,
        err: false,
      });
      await load();
      // A cache change re-prices the flags fit block at once —
      // toggling or resizing the Expert cache must move the fit
      // figures by itself, not only when an unrelated field
      // happens to re-trigger a preview (Pete, Oct 23, 2026).
      scheduleCachePreview();
      return true;
    } finally {
      moeCacheCommitting = false;
      inFlight.delete(id);
      if (!disposed) paint();
    }
  };
  const revertMoeCacheDraft = (row: BackendRow): void => {
    flagsMoeCacheOn = row.moe_cache ? row.moe_cache.mib != null : false;
    flagsMoeCacheMib = row.moe_cache ? row.moe_cache.mib : null;
  };
  const clickMoeCacheSwitch = async (ev: Event): Promise<void> => {
    const t = (ev.target as HTMLElement).closest("[data-moe-cache-switch]") as HTMLElement | null;
    if (!t) return;
    const id = t.getAttribute("data-moe-cache-backend") ?? "";
    const row = lastRows.find((r) => r.id === id);
    if (!row?.moe_cache || moeCacheCommitting) return;
    const on = !flagsMoeCacheOn;
    flagsMoeCacheOn = on;
    // The toggle alone re-prices the fit block: the preview
    // carries the control's draft, so the figures move before
    // the save even lands.
    scheduleCachePreview();
    const card = t.closest(".backend-card");
    const token = card?.querySelector("[data-moe-cache-token]");
    if (token) token.textContent = moeCacheTokenText(on ? (flagsMoeCacheMib ?? row.moe_cache.mib ?? MOE_CACHE_DEFAULT_MIB) : null);
    const ok = await saveMoeCache(id, on ? (flagsMoeCacheMib ?? MOE_CACHE_DEFAULT_MIB) : null, false);
    if (!ok && !disposed) {
      revertMoeCacheDraft(row);
      paint();
    }
  };
  root.addEventListener("click", clickMoeCacheSwitch);
  const changeMoeCacheSize = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLSelectElement;
    if (!t.hasAttribute("data-moe-cache-select")) return;
    const id = t.getAttribute("data-moe-cache-backend") ?? "";
    const row = lastRows.find((r) => r.id === id);
    if (!row?.moe_cache || moeCacheCommitting) return;
    if (t.value === "auto") {
      const ok = await saveMoeCache(id, null, true);
      if (!ok && !disposed) {
        revertMoeCacheDraft(row);
        paint();
      }
      return;
    }
    const n = Number(t.value);
    flagsMoeCacheMib = n;
    scheduleCachePreview();
    if (!flagsMoeCacheOn) { paint(); return; } // the size only drafts while off
    const card = t.closest(".backend-card");
    const token = card?.querySelector("[data-moe-cache-token]");
    if (token) token.textContent = moeCacheTokenText(n);
    if (n === row.moe_cache.mib) { paint(); return; } // the saved value chosen again
    const ok = await saveMoeCache(id, n, false);
    if (!ok && !disposed) {
      revertMoeCacheDraft(row);
      paint();
    }
  };
  root.addEventListener("change", changeMoeCacheSize);

  /* ---------------- Parallel slots field (tracker #61) --------
   * The ctx/batch idiom with the sliders' commit model: the
   * draft syncs through syncFlagsDraft (the shared collection
   * logic), and a "change" on the preset select or the Custom
   * input commits through backend_set_parallel, which rewrites
   * only the --parallel tokens in the backend's raw flags.
   * Choosing Custom… only reveals the numeric input — the value
   * commits when the input itself changes. A rejected or invalid
   * commit reverts the draft to the saved selection. Setting the
   * field to 1 is also what clears the MTP conflict line: the
   * reload after the commit refreshes the payload it renders
   * from. */
  const saveParallelValue = async (id: string, n: number | null): Promise<boolean> => {
    if (parallelCommitting) return false;
    parallelCommitting = true;
    inFlight.add(id);
    paint();
    try {
      const r = await setBackendParallel(id, n);
      if (disposed) return false;
      if (r.error) {
        msgs.set(id, { text: `Parallel slots not saved: ${r.error}`, err: true });
        paint();
        return false;
      }
      if (flagsFor === id) flagsParallel = n;
      msgs.set(id, {
        text: n === null
          ? "Parallel slots returned to the llama.cpp default (auto) \u2014 applies on next start."
          : `Parallel slots saved (${parallelTokenText(n)}) \u2014 applies on next start.`,
        err: false,
      });
      await load();
      schedulePredictedLookup();
      return true;
    } finally {
      parallelCommitting = false;
      inFlight.delete(id);
      if (!disposed) paint();
    }
  };
  const commitParallel = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLElement;
    const field = t.getAttribute("data-flags-field");
    if (field !== "parallel-preset" && field !== "parallel-custom") return;
    if (!flagsFor || parallelCommitting) return;
    // Choosing Custom… reveals the numeric input; nothing commits
    // until the input itself changes.
    if (field === "parallel-preset" && (t as HTMLSelectElement).value === "custom") return;
    const id = flagsFor;
    const row = lastRows.find((r) => r.id === id);
    if (!row?.parallel) return;
    const saved = row.parallel.slots;
    const n = flagsParallel;
    if (n === saved) { paint(); return; } // changed back to the saved selection
    // The same bounds the set op validates, checked here so the
    // error reads in the form's own terms; the op re-validates
    // regardless.
    if (n !== null && (!Number.isInteger(n) || n < PARALLEL_MIN || n > PARALLEL_MAX)) {
      msgs.set(id, { text: `Parallel slots must be between ${PARALLEL_MIN} and ${fmtCount(PARALLEL_MAX)}.`, err: true });
      flagsParallel = saved;
      paint();
      return;
    }
    const ok = await saveParallelValue(id, n);
    if (!ok && !disposed) {
      // A rejected commit reverts the draft to the saved selection.
      flagsParallel = saved;
      paint();
    }
  };
  root.addEventListener("change", commitParallel);

  /* ---------------- KV cache / flash attention ----------------
   * Three selects (the MTP idiom): a "change" on any of them
   * builds the full selection from the card's selects, moves the
   * draft, refreshes the preview, and commits through
   * backend_set_cache, which rewrites only the cache tokens in
   * the backend's raw flags. The quantized-V + FA-off pairing is
   * named in the form before committing (the op refuses it too).
   * Choosing the saved selection is a no-op; a rejected commit
   * reverts the draft to the saved selection. The predicted-best
   * button applies the preview's best rung through the same
   * commit path — nothing is ever applied without that click. */
  const saveCacheSelection = async (id: string, sel: CacheSelectionLike): Promise<boolean> => {
    if (cacheCommitting) return false;
    cacheCommitting = true;
    inFlight.add(id);
    paint();
    try {
      const r = await setBackendCache(id, sel);
      if (disposed) return false;
      if (r.error) {
        msgs.set(id, { text: `KV cache not saved: ${r.error}`, err: true });
        paint();
        return false;
      }
      if (flagsFor === id) flagsCacheSel = sel;
      msgs.set(id, { text: `KV cache saved (${cacheTokenText(sel)}) — applies on next start.`, err: false });
      await load();
      schedulePredictedLookup();
      scheduleCachePreview();
      return true;
    } finally {
      cacheCommitting = false;
      inFlight.delete(id);
      if (!disposed) paint();
    }
  };
  const commitCacheSelection = async (id: string, sel: CacheSelectionLike): Promise<void> => {
    const row = lastRows.find((r) => r.id === id);
    if (!row?.cache || cacheCommitting) return;
    flagsCacheSel = sel;
    scheduleCachePreview();
    if (cacheVQuantizedFaOff(sel)) {
      msgs.set(id, { text: "A quantized V cache requires flash attention: choose flash attention auto or on.", err: true });
      flagsCacheSel = { k: row.cache.k, v: row.cache.v, flash_attn: row.cache.flash_attn };
      paint();
      return;
    }
    const saved: CacheSelectionLike = { k: row.cache.k, v: row.cache.v, flash_attn: row.cache.flash_attn };
    if (row.cache.present && sameCacheSelection(sel, saved)) { paint(); return; }
    const ok = await saveCacheSelection(id, sel);
    if (!ok && !disposed) {
      flagsCacheSel = saved;
      paint();
    }
  };
  const changeCache = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLSelectElement;
    if (!t.hasAttribute("data-cache-field")) return;
    if (!flagsFor) return;
    const card = t.closest(".backend-card");
    if (!card) return;
    const read = (field: string): string => (card.querySelector(`[data-cache-field="${field}"]`) as HTMLSelectElement | null)?.value ?? "";
    const sel: CacheSelectionLike = {
      k: read("cache-k") as CacheTypeName,
      v: read("cache-v") as CacheTypeName,
      flash_attn: read("flash-attn") as FlashAttnName,
    };
    if (!CACHE_TYPES.includes(sel.k) || !CACHE_TYPES.includes(sel.v) || !FLASH_ATTN_MODES.includes(sel.flash_attn)) return;
    await commitCacheSelection(flagsFor, sel);
  };
  root.addEventListener("change", changeCache);
  const clickCacheBest = async (ev: Event): Promise<void> => {
    const t = (ev.target as HTMLElement).closest("[data-cache-best]") as HTMLElement | null;
    if (!t) return;
    const id = t.getAttribute("data-cache-backend") ?? "";
    if (!id || id !== flagsFor || !flagsCachePreview?.best) return;
    const best = flagsCachePreview.best;
    await commitCacheSelection(id, { k: best.k, v: best.v, flash_attn: best.flash_attn });
  };
  root.addEventListener("click", clickCacheBest);

  /** Apply the Edit panel's model draft through model_assign,
   *  carrying the backend's current tensor split so the
   *  assignment changes the model only (the split is the
   *  Placement section's own save). Returns "none" when the
   *  draft names the model the backend already serves (or no
   *  registered model), "saved" when the assignment landed, and
   *  "failed" when the op rejected it — the error is on the
   *  card and the caller keeps the panel open, saving nothing
   *  else, so a failed model change never reads as a partial
   *  save. model_assign itself is atomic (it commits only after
   *  placement, validation, journal, and save all succeed) and
   *  reports flags_untouched for advanced-flags backends. */
  const saveEditModel = async (id: string): Promise<"none" | "saved" | "failed"> => {
    const row = lastRows.find((r) => r.id === id);
    const model = lastModels.find((m) => m.id === flagsEditModelId);
    if (!row || !model || sameModelFile(row.model_file, model.gguf_path)) return "none";
    const cfgR = await getBackendConfig(id);
    const cfgFlags = cfgR.data?.flags as { tensor_split?: unknown } | undefined;
    const split = Array.isArray(cfgFlags?.tensor_split)
      && cfgFlags.tensor_split.every((x) => typeof x === "number" && Number.isFinite(x))
      ? (cfgFlags.tensor_split as number[]) : [];
    const r = await modelAssign(id, model.id, split);
    if (disposed) return "failed";
    if (r.error) {
      msgs.set(id, { text: `Model change failed: ${r.error}`, err: true });
      return "failed";
    }
    return "saved";
  };

  /* ---------------- Placement form ----------------
   * Rendered INSIDE the backend's own card (backendCard), like the
   * flags editor; the GPUs page shows placement read-only. State
   * lives outside the repainted DOM: which backend's form is open,
   * the unchecked GPU indices, and the last projection. Checkbox
   * changes sync into the state and invalidate the projection (it
   * no longer matches the selection). */
  let placeFor: string | null = null;
  const placeExcluded = new Set<number>();
  let placeProjection: { backendId: string; modelId: string; data: PlacementProjectionData } | null = null;
  let placeBusy = false;
  /** Latest telemetry GPU list (checkbox source), refreshed by load(). */
  let lastGpuList: GpuInfo[] = [];
  const closePlaceForm = (): void => {
    placeFor = null;
    placeProjection = null;
    placeExcluded.clear();
    paint();
  };
  /** Close the whole Edit session (the Edit panel and the
   *  Placement section). A successful Save closes the surface,
   *  returning the card to its normal state (Pete's report,
   *  2026-10-20: Save flags left the edit form open — most
   *  visibly the advanced editor, whose save deliberately kept
   *  the panel up). A failed save never calls this: the surface
   *  stays open with the operator's input intact and the error
   *  on the card. */
  const closeEditSession = (): void => { closeFlagsForm(); closePlaceForm(); };
  const openPlaceForm = (row: BackendRow): void => {
    if (placeFor === row.id) { closePlaceForm(); return; }
    placeFor = row.id;
    placeProjection = null;
    placeExcluded.clear();
    // Default the selection to the backend's CURRENT placement, not
    // every GPU: the configured tensor split's positive entries when
    // a split is configured; otherwise the GPUs the loaded process
    // measurably holds weights on (>= 1 GiB — under that is a bare
    // CUDA context); otherwise, for an unsplit backend that is not
    // loaded, the first GPU, which is where an unsplit launch loads.
    const allIdx = lastGpuList.map((g) => Number(g.id));
    const cfgIdx = (row.tensor_split ?? [])
      .map((f, p) => (f > 0 && p < allIdx.length ? allIdx[p] : -1))
      .filter((i) => i >= 0);
    let current = cfgIdx;
    if (!current.length) {
      // Unmeasured rows (mib null) never select a GPU: unknown usage
      // is not residency. The default then falls back to the first
      // GPU, as for an unloaded backend.
      const resIdx = (residency.get(row.id) ?? [])
        .filter((r) => r.mib !== null && r.mib >= 1024)
        .map((r) => r.gpu)
        .filter((i) => allIdx.includes(i));
      current = resIdx.length ? resIdx : allIdx.slice(0, 1);
    }
    for (const ix of allIdx) if (!current.includes(ix)) placeExcluded.add(ix);
    paint();
  };
  const syncPlaceDraft = (ev: Event): void => {
    const t = ev.target as HTMLElement;
    if (!t.hasAttribute("data-place-gpu")) return;
    if (t.getAttribute("data-place-backend") !== placeFor) return;
    const ix = Number(t.getAttribute("data-place-gpu"));
    if ((t as HTMLInputElement).checked) placeExcluded.delete(ix);
    else placeExcluded.add(ix);
    placeProjection = null;
    paint();
  };
  root.addEventListener("change", syncPlaceDraft);

  /* ---------------- Add backend form ----------------
   * Lives in #backAddForm, outside the repainted #backBody, so the
   * 5-second poll never clobbers typed input. */
  const addHost = root.querySelector("#backAddForm");
  const closeAddForm = (): void => { if (addHost) addHost.innerHTML = ""; };
  const nextFreePort = (): number => {
    const used = new Set<number>(lastRows.map((r) => r.port));
    used.add(gatewayPort);
    used.add(8090); // the Windows ML backend's conventional port
    let p = 8081;
    while (used.has(p)) p += 1;
    return p;
  };
  const openAddForm = (): void => {
    if (!addHost) return;
    if (addHost.innerHTML) { closeAddForm(); return; }
    // No-models state: a compact directive card — no form, no
    // disabled submit, no prose (principle 13).
    if (!lastModels.length) {
      addHost.innerHTML = `<div class="card"><h3>${icon("server")} Add a backend</h3>
        <div class="kv" style="margin-top:10px"><span class="k">Models</span><span class="v" style="font-size:12px">none registered</span></div>
        <div class="card-actions"><a class="btn" href="#/models">Add a model</a>
          <button class="btn icon-only" id="addBackCancel" title="Cancel" aria-label="Cancel">${icon("x")}</button></div></div>`;
      addHost.querySelector("#addBackCancel")?.addEventListener("click", closeAddForm);
      return;
    }
    const modelOpts = lastModels.map((m) => `<option value="${esc(m.id)}">${esc(modelShortName(m))}</option>`).join("");
    const groupOpts = lastGroups.map((g) => `<option value="${esc(g.name)}">${esc(g.name)}</option>`).join("")
      + `<option value="__new__">New group\u2026</option>`;
    addHost.innerHTML = `<div class="card"><h3>${icon("server")} Add a backend<span class="info-tip" title="The backend is created with the launch parameters below (32K context, 512 batch, and the model's catalog default offload unless changed); a tensor split is assigned from live GPU telemetry when available unless GPUs are picked under Placement. Joining an existing group makes the gateway share that group's traffic across its members; a new group gives the backend its own model name.">${icon("info")}</span></h3>
    <div style="display:grid;grid-template-columns:1fr 1fr;gap:10px;margin-top:10px">
      <label style="font-size:12px;color:var(--text2)">Name<span class="info-tip" title="lowercase, digits, hyphens">${icon("info")}</span><br>
        <input id="addBackName" type="text" placeholder="nemotron" style="${SELECT_STYLE};width:100%"></label>
      <label style="font-size:12px;color:var(--text2)">Model<br>
        <select id="addBackModel" style="${SELECT_STYLE};width:100%">${modelOpts}</select></label>
      <label style="font-size:12px;color:var(--text2)">Port<br>
        <input id="addBackPort" type="number" value="${nextFreePort()}" style="${SELECT_STYLE};width:100%"></label>
      <label style="font-size:12px;color:var(--text2)">Gateway group<br>
        <select id="addBackGroup" style="${SELECT_STYLE};width:100%">${groupOpts}</select></label>
    </div>
    <label id="addBackNewGroupWrap" style="font-size:12px;color:var(--text2);display:none;margin-top:10px">New group name<span class="info-tip" title="Defaults to the backend name.">${icon("info")}</span><br>
      <input id="addBackNewGroup" type="text" placeholder="nemotron" style="${SELECT_STYLE};width:100%"></label>
    <label style="font-size:12px;color:var(--text2);display:block;margin-top:10px" title="The gateway routes enabled backends; it can be started right away.">
      <input id="addBackEnabled" type="checkbox" checked> Enabled</label>
    <div id="addBackParams"></div>
    <div id="addBackStrip"></div>
    <div class="card-actions"><button class="btn" id="addBackSubmit">${icon("plus")} Add backend</button>
      <button class="btn icon-only" id="addBackCancel" title="Cancel" aria-label="Cancel">${icon("x")}</button></div>
    <div id="addBackMsg" style="font-size:12px;margin-top:6px"></div>
  </div>`;
    const stripEl = addHost.querySelector("#addBackStrip") as HTMLElement | null;
    const modelSelEl = addHost.querySelector("#addBackModel") as HTMLSelectElement | null;

    /* Launch parameters (tracker #49): the same fields the Edit
     * Flags and Placement forms expose for an existing backend,
     * rendered by the same helpers and collected into this draft
     * for backend_add. ctx/batch start at the creation defaults
     * and survive a model change (they are model-independent; only
     * their rated-context annotation follows the model). The
     * offload draft and the placement selection are
     * model-dependent: each backend_add_preview payload resets
     * them — the slider to the model's catalog default position,
     * the chips to the split creation would assign. */
    const pDraft: { ctx: number; batch: number; parallel: number | null } = { ctx: 32768, batch: 512, parallel: null };
    /** The add form's KV cache draft: starts at the preview's
     *  predicted-best rung for the model (the brief's starting
     *  point for a new backend), then follows the operator's
     *  changes to the cache selects; carried by backend_add. */
    let pCache: CacheSelectionLike | null = null;
    let preview: BackendAddPreview | null = null;
    let offLayers: number | null = null;
    /** A released offload position the guardrail blocks, awaiting
     *  the operator's decision in the form. `override` records the
     *  decision to create with an explicit override (backend_add
     *  journals it, as backend_set_offload does); without it the
     *  position must be snapped or moved before submit. */
    let offPending: { layers: number; override: boolean } | null = null;
    /** GPU indices the operator unchecked in the Placement chips.
     *  gpuTouched separates the previewed selection (creation
     *  auto-assigns) from an explicit one (creation writes the
     *  even split across it). */
    const gpuExcluded = new Set<number>();
    let gpuTouched = false;
    let previewSeq = 0;
    /** The predicted-throughput line for the add form's draft
     *  (design §6, shared with the Edit Flags form): the last
     *  measurement_lookup result for the configuration this
     *  form would create, debounced like the edit form's. */
    let addPredictedHtml = "";
    let addPredictedSeq = 0;
    let addPredictedTimer: number | null = null;
    const runAddLookup = async (): Promise<void> => {
      const modelId = modelSelEl?.value ?? "";
      if (!modelId) return;
      const seq = ++addPredictedSeq;
      const r = await measurementLookup({
        model_id: modelId,
        n_ctx: pDraft.ctx,
        n_batch: pDraft.batch,
        offload_layers: offLayers ?? preview?.offload?.layers ?? null,
        parallel_slots: pDraft.parallel,
        gpu_indices: gpuTouched ? selectedGpus() : null,
      });
      if (disposed || seq !== addPredictedSeq) return;
      addPredictedHtml = r.data ? predictedLineHtml(r.data.exact, r.data.near) : "";
      const slot = paramsEl?.querySelector("[data-add-predicted-slot]");
      if (slot) slot.innerHTML = addPredictedHtml;
    };
    const scheduleAddLookup = (): void => {
      if (addPredictedTimer !== null) window.clearTimeout(addPredictedTimer);
      addPredictedTimer = window.setTimeout(() => {
        addPredictedTimer = null;
        void runAddLookup();
      }, 300);
    };
    const paramsEl = addHost.querySelector("#addBackParams") as HTMLElement | null;
    const selectedGpus = (): number[] =>
      lastGpuList.map((g) => Number(g.id)).filter((ix) => !gpuExcluded.has(ix));
    /* Model summary strip (add-backend audit Q3/Q8): Size, then the
     * creation requirement as the backend card's GPU RAM / System
     * RAM tiles (backend_add_preview's `memory` — the same
     * requirement model, at the form's context/slot draft), then
     * the split preview: one segmented bar, one segment per GPU the
     * placement uses, width = its share of the split, keyed
     * "GPU n — %", the required GiB in each segment's tooltip. For
     * an offload-capable model the two weight tiles follow the
     * offload draft through the same per-position pricing the
     * slider readout uses. A figure the payload does not carry is
     * omitted, never zero-filled; no placement = no bar. */
    const splitColors = ["#4a9eff", "#a371f7", "#39c5cf", "#d29922", "#3fb950", "#f85149"];
    const paintStrip = (): void => {
      if (!stripEl || !modelSelEl) return;
      const m = lastModels.find((x) => x.id === modelSelEl.value);
      if (!m) { stripEl.innerHTML = ""; return; }
      const size = m.size_bytes ? fmtBytes(m.size_bytes)
        : m.catalog_size_bytes !== null ? `~${fmtBytes(m.catalog_size_bytes)} (estimate)` : null;
      const mem = preview && preview.model_id === m.id ? preview.memory ?? null : null;
      const tiles: [string, string, string][] = [];
      if (size !== null) tiles.push(["Size", size, ""]);
      if (mem) {
        let gpuMib = mem.gpu_ram_mib;
        let sysMib = mem.sys_ram_mib;
        if (preview?.offload) {
          const figs = offloadFigures(preview.offload, offLayers ?? preview.offload.layers);
          gpuMib = figs.gpuMib;
          sysMib = figs.sysMib;
        }
        tiles.push(["GPU RAM", `${fmtGb(gpuMib / 1024)} GiB`, "GPU-resident weights of the backend this form would create"]);
        tiles.push(["System RAM", `${fmtGb(sysMib / 1024)} GiB`, "Weights held in system RAM (routed experts under expert offload)"]);
      }
      let barHtml = "";
      const split = preview && preview.model_id === m.id ? preview.tensor_split : [];
      const entries: { gpu: number; share: number; required: number | null; total: number | null }[] = mem && mem.per_gpu.length
        ? mem.per_gpu.map((p) => ({ gpu: p.gpu, share: p.share, required: p.required_mib, total: p.total_mib }))
        : lastGpuList
          .map((g, p) => ({ gpu: Number(g.id), share: split[p] ?? 0, required: null, total: null }))
          .filter((e) => e.share > 0);
      if (entries.length) {
        const nameOf = (ix: number): string => lastGpuList.find((g) => Number(g.id) === ix)?.name ?? "";
        const segs = entries.map((e) => {
          const nm = nameOf(e.gpu);
          const req = e.required !== null && e.total !== null ? ` · Required: ${fmtGb(e.required / 1024)} of ${fmtGb(e.total / 1024)} GiB` : "";
          return { pct: e.share * 100, color: splitColors[e.gpu % splitColors.length], label: `GPU ${e.gpu}${nm ? ` · ${nm}` : ""} — ${trim1(e.share * 100)}%${req}` };
        });
        const keys = entries.map((e, i) => `<span title="${esc(segs[i].label)}"><span class="sw" style="background:${segs[i].color}"></span>GPU ${e.gpu} — ${trim1(e.share * 100)}%</span>`).join("");
        barHtml = `<div data-add-split-bar style="margin-top:10px">
        <span style="font-size:12px;color:var(--text2)">Split</span><span class="info-tip" title="The tensor split creation would assign: each segment is one GPU's share of the model's weights. The selection comes from live telemetry unless GPUs are picked under Placement.">${icon("info")}</span>
        <div style="margin-top:4px">${stackedBar(segs, 600, 10)}</div>
        <div class="legend" style="margin-top:4px">${keys}</div></div>`;
      }
      stripEl.innerHTML = `<div style="display:grid;grid-template-columns:repeat(4,1fr);gap:10px;margin-top:10px">
        ${tiles.map(([k, v, tip]) => `<div class="kv"${tip ? ` title="${esc(tip)}"` : ""}><span class="k">${k}</span><span class="v" style="font-size:12px">${esc(v)}</span></div>`).join("")}
      </div>${barHtml}`;
    };
    const paintParams = (): void => {
      paintStrip();
      if (!paramsEl) return;
      const m = lastModels.find((x) => x.id === modelSelEl?.value);
      const ctxMax = m?.ctx_max ? m.ctx_max : null;
      // The offload slider, when the previewed model is
      // offload-capable. The decision row carries the edit form's
      // two resolving actions with creation wording: nothing can
      // be saved yet, so an override is recorded in the draft and
      // carried by backend_add; snapping moves the draft to the
      // nearest verified position.
      let offloadHtml = "";
      if (preview?.offload) {
        const o = preview.offload;
        const n = offLayers ?? o.layers;
        let decisionHtml = "";
        if (offPending && o.guardrail && offloadIsUntested(o, offPending.layers)) {
          decisionHtml = offPending.override
            ? `<div data-add-offload-decision style="display:flex;gap:8px;align-items:center;flex-wrap:wrap;margin-top:8px">
      <span data-add-decision-text style="font-size:12px;color:#e3b341">${offPending.layers} of ${o.moe_layers} layers is unverified on this GPU \u2014 the backend will be created with an override recorded</span>
      <button class="btn" data-add-decision="snap">${icon("check")} Snap to ${o.guardrail.nearest_verified_layers} of ${o.moe_layers} layers</button>
    </div>`
            : `<div data-add-offload-decision style="display:flex;gap:8px;align-items:center;flex-wrap:wrap;margin-top:8px">
      <span data-add-decision-text style="font-size:12px;color:#e3b341">${offPending.layers} of ${o.moe_layers} layers is unverified on this GPU</span>
      <button class="btn" data-add-decision="override">${icon("warn")} Create with override</button>
      <button class="btn" data-add-decision="snap">${icon("check")} Snap to ${o.guardrail.nearest_verified_layers} of ${o.moe_layers} layers</button>
    </div>`;
        }
        offloadHtml = offloadSliderHtml({ offload: o, layers: n, ownerId: "new backend", disabled: false, decisionHtml });
      }
      // Placement chips: the Placement form's include/exclude
      // chips over the telemetry GPUs, defaulting to the split
      // creation would assign.
      let placeHtml = "";
      // The resulting split renders once, as the strip's split bar
      // directly below (its keys carry the percentages).
      if (lastGpuList.length) {
        placeHtml = `<div style="margin-top:12px">
        <span style="font-size:12px;color:var(--text2)">Placement (tensor split)</span><span class="info-tip" title="The split divides the model's weights across the selected GPUs, in nvidia-smi index order. The selection starts at the split a creation would assign from live telemetry; changing it makes the split an even division across the selected GPUs instead.">${icon("info")}</span>
        <div class="gpu-chip-row">${lastGpuList.map((g) => {
          const ix = Number(g.id);
          const on = !gpuExcluded.has(ix);
          return `<label class="gpu-chip${on ? " on" : ""}" title="${esc(g.name)}"><span class="gpu-chip-top"><input type="checkbox" data-add-place-gpu="${ix}"${on ? " checked" : ""}> GPU ${ix}</span><span class="gpu-chip-name">${esc(gpuShortName(g.name))}</span></label>`;
        }).join("")}</div>
      </div>`;
      } else {
        placeHtml = `<div style="font-size:12px;color:var(--text2);margin-top:12px">Placement: auto<span class="info-tip" title="No GPU telemetry yet \u2014 a tensor split cannot be previewed or selected; creation leaves the split unset, as it does today.">${icon("info")}</span></div>`;
      }
      // The cache controls for the backend being created: the
      // draft starts at the preview's predicted-best rung; the
      // footprint line re-prices from the ladder rung matching
      // the draft's K/V types when the draft differs from the
      // preview's own selection.
      let cacheHtml = "";
      if (preview?.cache) {
        const sel: CacheSelectionLike = pCache ?? (preview.cache.best ? { k: preview.cache.best.k, v: preview.cache.best.v, flash_attn: preview.cache.best.flash_attn } : preview.cache.selection);
        let renderPreview: CachePreview = preview.cache;
        if (!sameCacheSelection(sel, preview.cache.selection)) {
          const rung = preview.cache.ladder.find((r) => r.k === sel.k && r.v === sel.v);
          if (rung) renderPreview = { ...preview.cache, kv_mib: rung.kv_mib, total_mib: rung.total_mib, fits_current: rung.fits, selection: sel };
        }
        cacheHtml = flagsCacheHtml({ sel, savedCustom: false, savedPresent: false, preview: renderPreview, fieldAttr: "data-add-cache-field", bestAttr: "data-add-cache-best", disabled: false });
      }
      paramsEl.innerHTML = `<div style="margin-top:12px;border-top:1px solid var(--border);padding-top:10px">
      <div style="font-size:12px;font-weight:600">${icon("gear")} Launch parameters</div>
      ${flagsCtxBatchHtml({ ctx: pDraft.ctx, batch: pDraft.batch, ctxMax, fieldAttr: "data-add-flags-field", disabled: false })}
      ${flagsParallelHtml({ value: pDraft.parallel, savedCustom: false, fieldAttr: "data-add-flags-field", disabled: false })}
      ${cacheHtml}
      ${offloadHtml}
      ${flagsRatedLineHtml({ ctxMax, modelId: m?.id ?? null, savedText: "Saved when the backend is added; applies from its first start. Placement is set by the GPU selection below." })}
      <div data-add-predicted-slot>${addPredictedHtml}</div>
      ${placeHtml}
    </div>`;
    };
    const resetPlacementFromPreview = (): void => {
      gpuExcluded.clear();
      gpuTouched = false;
      const split = preview?.tensor_split ?? [];
      lastGpuList.forEach((g, p) => {
        // No assigned split (single GPU, or none yet): an unsplit
        // creation loads on the first GPU — the Placement form's
        // fallback selection for the same state.
        const on = split.length ? (split[p] ?? 0) > 0 : p === 0;
        if (!on) gpuExcluded.add(Number(g.id));
      });
    };
    const loadPreview = async (modelChanged: boolean): Promise<void> => {
      const modelId = modelSelEl?.value ?? "";
      if (!modelId) return;
      const seq = ++previewSeq;
      const r = await backendAddPreview(modelId, gpuTouched ? selectedGpus() : undefined, pDraft.ctx, pDraft.parallel ?? undefined);
      if (disposed || seq !== previewSeq) return;
      if (r.error || !r.data) { preview = null; paintParams(); scheduleAddLookup(); return; }
      preview = r.data;
      if (modelChanged) {
        offLayers = preview.offload ? preview.offload.layers : null;
        offPending = null;
        pCache = preview.cache ? (preview.cache.best ? { k: preview.cache.best.k, v: preview.cache.best.v, flash_attn: preview.cache.best.flash_attn } : { ...preview.cache.selection }) : null;
        resetPlacementFromPreview();
      }
      paintParams();
      scheduleAddLookup();
    };
    modelSelEl?.addEventListener("change", () => { paintStrip(); void loadPreview(true); });
    paramsEl?.addEventListener("input", (ev) => {
      const t = ev.target as HTMLElement;
      if (t.hasAttribute("data-offload-slider")) {
        if (!preview?.offload) return;
        const n = Number((t as HTMLInputElement).value);
        offLayers = n;
        scheduleAddLookup();
        const card = t.closest(".card");
        if (card) paintOffloadReadout(card, preview.offload, n);
        // The strip's GPU RAM / System RAM tiles follow the draft.
        paintStrip();
        // Keep a pending decision in step with the draft (the
        // edit form's rule): its text follows a still-blocked
        // position, and the row hides while the draft sits on a
        // verified position; the release handler re-decides.
        if (offPending) {
          const dec = paramsEl.querySelector("[data-add-offload-decision]") as HTMLElement | null;
          if (dec) {
            if (offloadIsUntested(preview.offload, n)) {
              offPending.layers = n;
              dec.style.display = "";
              const dt = dec.querySelector("[data-add-decision-text]");
              if (dt) dt.textContent = `${n} of ${preview.offload.moe_layers} layers is unverified on this GPU${offPending.override ? " \u2014 the backend will be created with an override recorded" : ""}`;
            } else {
              dec.style.display = "none";
            }
          }
        }
        return;
      }
      const field = t.getAttribute("data-add-flags-field");
      if (field) { applyFlagsField(pDraft, field, (t as HTMLInputElement).value, false); scheduleAddLookup(); }
    });
    paramsEl?.addEventListener("change", (ev) => {
      const t = ev.target as HTMLElement;
      if (t.hasAttribute("data-offload-slider")) {
        if (!preview?.offload) return;
        const n = Number((t as HTMLInputElement).value);
        offLayers = n;
        // Release decides the pending state: a guardrail-blocked
        // position becomes a decision the operator must make (an
        // override confirmed for one position does not carry to
        // another); a verified position clears it.
        if (offloadIsUntested(preview.offload, n)) {
          if (!offPending || offPending.layers !== n) offPending = { layers: n, override: false };
        } else {
          offPending = null;
        }
        paintParams();
        scheduleAddLookup();
        return;
      }
      if (t.hasAttribute("data-add-place-gpu")) {
        const ix = Number(t.getAttribute("data-add-place-gpu"));
        if ((t as HTMLInputElement).checked) gpuExcluded.delete(ix);
        else gpuExcluded.add(ix);
        gpuTouched = true;
        // The placement drives the guardrail evaluation: refresh
        // the preview against the explicit selection.
        void loadPreview(false);
        return;
      }
      if (t.hasAttribute("data-add-cache-field")) {
        const read = (f: string): string => (paramsEl.querySelector(`[data-add-cache-field="${f}"]`) as HTMLSelectElement | null)?.value ?? "";
        const sel: CacheSelectionLike = { k: read("cache-k") as CacheTypeName, v: read("cache-v") as CacheTypeName, flash_attn: read("flash-attn") as FlashAttnName };
        if (CACHE_TYPES.includes(sel.k) && CACHE_TYPES.includes(sel.v) && FLASH_ATTN_MODES.includes(sel.flash_attn)) { pCache = sel; paintParams(); scheduleAddLookup(); }
        return;
      }
      const field = t.getAttribute("data-add-flags-field");
      if (field && applyFlagsField(pDraft, field, (t as HTMLInputElement).value, true)) { paintParams(); scheduleAddLookup(); void loadPreview(false); }
    });
    paramsEl?.addEventListener("click", (ev) => {
      const bestBtn = (ev.target as HTMLElement).closest("[data-add-cache-best]") as HTMLElement | null;
      if (bestBtn && preview?.cache?.best) {
        const b = preview.cache.best;
        pCache = { k: b.k, v: b.v, flash_attn: b.flash_attn };
        paintParams();
        scheduleAddLookup();
        return;
      }
      const t = (ev.target as HTMLElement).closest("[data-add-decision]") as HTMLElement | null;
      if (!t || !preview?.offload || !offPending) return;
      if (t.getAttribute("data-add-decision") === "override") {
        offPending.override = true;
      } else {
        // Snap: move the draft to the nearest verified position.
        offLayers = preview.offload.guardrail?.nearest_verified_layers ?? offPending.layers;
        offPending = null;
      }
      paintParams();
    });
    paintParams();
    void loadPreview(true);
    const groupSelEl = addHost.querySelector("#addBackGroup") as HTMLSelectElement | null;
    const newWrap = addHost.querySelector("#addBackNewGroupWrap") as HTMLElement | null;
    groupSelEl?.addEventListener("change", () => {
      if (newWrap) newWrap.style.display = groupSelEl.value === "__new__" ? "block" : "none";
    });
    addHost.querySelector("#addBackCancel")?.addEventListener("click", closeAddForm);
    const submit = addHost.querySelector("#addBackSubmit") as HTMLButtonElement | null;
    submit?.addEventListener("click", async () => {
      const val = (sel: string): string => (addHost.querySelector(sel) as HTMLInputElement | null)?.value.trim() ?? "";
      const name = val("#addBackName").toLowerCase();
      const modelId = (addHost.querySelector("#addBackModel") as HTMLSelectElement | null)?.value ?? "";
      const port = Number(val("#addBackPort"));
      const groupSel = (addHost.querySelector("#addBackGroup") as HTMLSelectElement | null)?.value ?? "";
      const createGroup = groupSel === "__new__";
      const group = createGroup ? (val("#addBackNewGroup").toLowerCase() || name) : groupSel;
      const enabled = (addHost.querySelector("#addBackEnabled") as HTMLInputElement | null)?.checked ?? true;
      const msg = addHost.querySelector("#addBackMsg") as HTMLElement | null;
      if (!name || !modelId || !group || !Number.isInteger(port) || port <= 0) {
        if (msg) msg.innerHTML = `<span style="color:var(--err)">Name, model, port, and group are required.</span>`;
        return;
      }
      // Launch parameters: the same validation the create op
      // applies (context range; batch within 1..=context), checked
      // here so the error reads in the form's own terms; the op
      // re-validates regardless.
      if (!Number.isInteger(pDraft.ctx) || pDraft.ctx < 1024 || pDraft.ctx > 1048576) {
        if (msg) msg.innerHTML = `<span style="color:var(--err)">Context length must be between 1,024 and 1,048,576.</span>`;
        return;
      }
      if (!Number.isInteger(pDraft.batch) || pDraft.batch < 1 || pDraft.batch > pDraft.ctx) {
        if (msg) msg.innerHTML = `<span style="color:var(--err)">Batch size must be between 1 and the context length.</span>`;
        return;
      }
      // Parallel slots: the same bounds the create op applies
      // (backend_set_parallel's range), checked here in the form's
      // terms; an untouched field (null) sends nothing and the
      // backend is created with no --parallel token.
      if (pDraft.parallel !== null && (!Number.isInteger(pDraft.parallel) || pDraft.parallel < PARALLEL_MIN || pDraft.parallel > PARALLEL_MAX)) {
        if (msg) msg.innerHTML = `<span style="color:var(--err)">Parallel slots must be between ${PARALLEL_MIN} and ${fmtCount(PARALLEL_MAX)}.</span>`;
        return;
      }
      // KV cache: the pairing llama.cpp refuses cannot be carried
      // into creation (the op refuses it too).
      if (pCache && cacheVQuantizedFaOff(pCache)) {
        if (msg) msg.innerHTML = `<span style="color:var(--err)">A quantized V cache requires flash attention: choose flash attention auto or on.</span>`;
        return;
      }
      // An undecided guardrail-blocked offload position cannot be
      // carried into creation (the op would refuse it without the
      // override): the operator decides here first.
      if (offPending && !offPending.override) {
        if (msg) msg.innerHTML = `<span style="color:var(--err)">Offload position ${offPending.layers} of ${preview?.offload?.moe_layers ?? ""} layers is unverified on this GPU \u2014 create with an override or snap to a verified position.</span>`;
        return;
      }
      if (gpuTouched && !selectedGpus().length) {
        if (msg) msg.innerHTML = `<span style="color:var(--err)">Select at least one GPU for the split.</span>`;
        return;
      }
      submit.disabled = true;
      const r = await addBackend({
        id: name, modelId, port, group, createGroup, enabled,
        nCtx: pDraft.ctx,
        nBatch: pDraft.batch,
        offloadLayers: preview?.offload ? (offLayers ?? preview.offload.layers) : undefined,
        offloadOverride: offPending?.override ? true : undefined,
        gpuIndices: gpuTouched ? selectedGpus() : undefined,
        parallel: pDraft.parallel ?? undefined,
        cacheK: pCache?.k,
        cacheV: pCache?.v,
        flashAttn: pCache?.flash_attn,
      });
      if (disposed) return;
      if (r.error) {
        if (msg) msg.innerHTML = `<span style="color:var(--err)">Add failed: ${esc(r.error)}</span>`;
        submit.disabled = false;
        return;
      }
      const modelRow = lastModels.find((x) => x.id === modelId);
      addHost.innerHTML = `<div class="card"><h3>${icon("check")} Backend added</h3>
        <div style="display:grid;gap:4px;margin-top:8px;font-size:12px">
          <div><span style="color:var(--text2)">Backend:</span> ${esc(name)}</div>
          <div><span style="color:var(--text2)">Model:</span> ${esc(modelRow ? modelShortName(modelRow) : modelId)}</div>
          <div><span style="color:var(--text2)">Port:</span> ${port}</div>
          <div><span style="color:var(--text2)">Group:</span> ${esc(group)}</div>
          <div><span style="color:var(--text2)">Enabled:</span> ${enabled ? "yes" : "no"}</div>
          <div><span style="color:var(--text2)">Context:</span> ${fmtCount(pDraft.ctx)}</div>
          <div><span style="color:var(--text2)">Batch:</span> ${fmtCount(pDraft.batch)}</div>
          ${pDraft.parallel !== null ? `<div><span style="color:var(--text2)">Parallel slots:</span> ${pDraft.parallel}</div>` : ""}
          ${pCache ? `<div><span style="color:var(--text2)">KV cache:</span> ${esc(cacheTokenText(pCache))}</div>` : ""}
          ${preview?.offload ? `<div><span style="color:var(--text2)">Offload:</span> ${esc(offloadTokenText(preview.offload, offLayers ?? preview.offload.layers))}${offPending?.override ? " (override recorded)" : ""}</div>` : ""}
          ${gpuTouched ? `<div><span style="color:var(--text2)">GPUs:</span> ${esc(selectedGpus().map((ix) => `GPU ${ix}`).join(", "))}</div>` : ""}
          <div><span style="color:var(--text2)">Activation plan:</span> recorded<span class="info-tip" title="The change is recorded in the activation plan at the top of this page — the gateway routes the new backend after the plan is executed.">${icon("info")}</span></div>
        </div>
        <div class="card-actions"><button class="btn icon-only" id="addBackDone" title="Done" aria-label="Done">${icon("check")}</button></div></div>`;
      addHost.querySelector("#addBackDone")?.addEventListener("click", closeAddForm);
      await load();
    });
  };
  const addBtn = root.querySelector("#backAdd");
  addBtn?.addEventListener("click", openAddForm);

  const onClick = async (ev: Event): Promise<void> => {
    const target = ev.target as HTMLElement;
    const menuToggle = target.closest("[data-back-menu]") as HTMLElement | null;
    if (menuToggle) {
      const menuId = menuToggle.getAttribute("data-back-menu") ?? "";
      const wasOpen = openMenus.has(menuId);
      openMenus.clear();
      if (!wasOpen && menuId) openMenus.add(menuId);
      paint();
      return;
    }
    const menuItem = target.closest("[data-back-menu-item]") as HTMLElement | null;
    if (menuItem) openMenus.delete(menuItem.getAttribute("data-back-id") ?? "");
    const hadOpenMenu = openMenus.size > 0;
    if (!menuItem && !target.closest("[data-back-menu-wrap]") && hadOpenMenu) {
      openMenus.clear();
    }
    const pt = target.closest("[data-plan-act]") as HTMLElement | null;
    if (pt) {
      if (hadOpenMenu) paint();
      await handlePlanAction(pt.getAttribute("data-plan-act") ?? "", planUi, paintPlan, load, lastPlan);
      return;
    }
    const t = target.closest("[data-back-act]") as HTMLElement | null;
    if (!t) {
      if (hadOpenMenu && !target.closest("[data-back-menu-wrap]")) paint();
      return;
    }
    const id = t.getAttribute("data-back-id") ?? "";
    const act = t.getAttribute("data-back-act") ?? "";

    if (act === "tuning") {
      // Per-backend tuning (Addendum B §B.2): the sweep lives
      // on the Optimize page; preselect this backend there.
      try { sessionStorage.setItem("tuningBackend", id); } catch { /* storage unavailable */ }
      location.hash = "#/optimize";
      return;
    }

    if (act === "details") {
      const opening = !openDetails.has(id);
      if (opening) openDetails.add(id); else openDetails.delete(id);
      paint();
      // The expansion's latest-lines block wants this backend's
      // tail now, not at the next poll's load.
      if (opening) void fetchLogTails();
      return;
    }

    if (act === "predict-refresh") {
      // A Measure run against the running backend, awaited here
      // the way the Optimize view awaits it: the tile carries
      // the Measuring state from the wire-level map while the
      // invoke is outstanding, the field updates from the
      // refreshed rows on completion (no page reload), and a
      // failure lands on the card's note — never silent. The
      // op's own refusals (job slot busy, backend stopped
      // mid-run) arrive as the error and surface the same way.
      if (predictRefreshing.has(id)) return;
      const row = lastRows.find((r) => r.id === id);
      if (!row || !row.configured || row.state !== "running") return;
      predictRefreshing.set(id, Date.now());
      msgs.delete(id);
      paint();
      startPredictTicker();
      const r = await optimizeMeasureBackend(id);
      predictRefreshing.delete(id);
      if (!predictRefreshing.size) stopPredictTicker();
      if (disposed) return;
      if (r.error) {
        msgs.set(id, {
          text: r.error === "measurement cancelled"
            ? "Prediction refresh cancelled."
            : `Prediction refresh failed — ${r.error}`,
          err: true,
        });
        paint();
      } else {
        msgs.delete(id);
        await load();
      }
      return;
    }

    if (act === "flags") {
      // The one Edit entry point: it opens the whole Edit
      // session — the Edit panel (model, flags) and the
      // Placement section below it — or closes the session when
      // any part of it is open.
      const row = lastRows.find((r) => r.id === id);
      if (!row) return;
      if (flagsFor === id || placeFor === id) closeEditSession();
      else { openFlagsForm(row); openPlaceForm(row); }
      return;
    }

    if (act === "flags-cancel") {
      closeFlagsForm();
      return;
    }

    if (act === "place-cancel") {
      closePlaceForm();
      return;
    }

    if (act === "place-propose") {
      if (placeBusy) return;
      const row = lastRows.find((r) => r.id === id);
      const model = lastModels.find((m) => row !== undefined && sameModelFile(row.model_file, m.gguf_path));
      if (!row || !model) return;
      if (!lastGpuList.length) {
        msgs.set(id, { text: "No GPU telemetry yet \u2014 a split cannot be proposed.", err: true });
        paint();
        return;
      }
      // Compose the split: preserve the backend's configured split
      // when it covers exactly the selected set of GPUs (the norm
      // right after loading); otherwise an even split across the
      // selection. Positions follow the telemetry GPU order, which
      // is nvidia-smi index order — the same order tensor_split
      // uses.
      const usedPos = lastGpuList
        .map((g, p) => (!placeExcluded.has(Number(g.id)) ? p : -1))
        .filter((p) => p >= 0);
      if (!usedPos.length) {
        msgs.set(id, { text: "Select at least one GPU for the split.", err: true });
        paint();
        return;
      }
      const cfgSplit = row.tensor_split ?? [];
      const nz = cfgSplit.map((f, i) => (f > 0 ? i : -1)).filter((i) => i >= 0);
      const sameSet = cfgSplit.length === lastGpuList.length
        && nz.length === usedPos.length
        && nz.every((i) => usedPos.includes(i));
      const split = sameSet
        ? [...cfgSplit]
        : lastGpuList.map((_, p) => (usedPos.includes(p) ? 1 / usedPos.length : 0));
      placeBusy = true;
      paint();
      try {
        const r = await placementPropose(model.id, split, id);
        if (disposed) return;
        if (r.error || !r.data) {
          msgs.set(id, { text: `Propose failed: ${r.error ?? "the command returned no projection"}`, err: true });
          placeProjection = null;
        } else {
          placeProjection = { backendId: id, modelId: model.id, data: r.data };
        }
      } finally {
        placeBusy = false;
        if (!disposed) paint();
      }
      return;
    }

    if (act === "place-apply") {
      if (placeBusy || !placeProjection || placeProjection.backendId !== id) return;
      const proj = placeProjection;
      // The projection prices the model it was proposed for. If the
      // backend's model changed since (a model assignment reloads
      // the rows), the projection is stale: drop it rather than
      // applying the old model's split to the new model
      // (code-review 5.7).
      const prow = lastRows.find((r) => r.id === id);
      const currentModel = lastModels.find((m) => prow !== undefined && sameModelFile(prow.model_file, m.gguf_path));
      if (!prow || !currentModel || currentModel.id !== proj.modelId) {
        msgs.set(id, { text: "The backend's model changed since this split was proposed — propose again to price the current model.", err: true });
        placeProjection = null;
        paint();
        return;
      }
      placeBusy = true;
      paint();
      try {
        const r = await placementApply(proj.modelId, proj.data.split, id);
        if (disposed) return;
        if (r.error) {
          // Keep the form and projection open with the error on the
          // card — closing would read as if the apply had happened.
          msgs.set(id, { text: `Split not saved: ${r.error}`, err: true });
          paint();
        } else if (r.data.persisted === false) {
          // Applied in memory only (code-review 5.2): say so on
          // the card instead of the saved message — the split is
          // lost when the app restarts.
          msgs.set(id, { text: "Split applied for this session only — it was not saved and will be lost when the app restarts.", err: true });
          placeFor = null;
          placeProjection = null;
          placeExcluded.clear();
          await load();
        } else {
          msgs.set(id, { text: `Split saved to ${id}. Restart the backend to use it.`, err: false });
          placeFor = null;
          placeProjection = null;
          placeExcluded.clear();
          await load();
        }
      } finally {
        placeBusy = false;
      }
      return;
    }

    if (act === "flags-save") {
      // The Edit panel's Save: the model draft applies first —
      // a failed assignment saves nothing else and keeps the
      // panel open — then the ctx/batch draft goes through the
      // flags save. The card note names both halves of the
      // outcome, so a flags failure after a landed model change
      // never reads as a full save or a full failure.
      inFlight.add(id);
      paint();
      try {
        const mres = await saveEditModel(id);
        if (disposed) return;
        if (mres === "failed") { paint(); return; }
        const modelNote = mres === "saved" ? `Model set to ${flagsEditModelId}. ` : "";
        const r = await updateBackendFlags(id, flagsCtx, flagsBatch);
        if (disposed) return;
        if (r.error) {
          // Keep the form open with the error on the card — closing on
          // a rejected value would read as if the save had happened.
          msgs.set(id, { text: mres === "saved" ? `Model set to ${flagsEditModelId} — flags not saved: ${r.error}` : `Flags not saved: ${r.error}`, err: true });
          paint();
        } else if (r.data.persisted === false) {
          // Code-review 5.2: a non-persisted save is not a save.
          msgs.set(id, { text: `${modelNote}Flags applied for this session only — they were not saved and will be lost when the app restarts.`, err: true });
          closeEditSession();
          await load();
        } else {
          msgs.set(id, { text: `${modelNote}Flags saved (context ${fmtCount(flagsCtx)}, batch ${fmtCount(flagsBatch)}) — applies on next start.`, err: false });
          closeEditSession();
          await load();
        }
      } finally {
        inFlight.delete(id);
        if (!disposed) paint();
      }
      return;
    }

    if (act === "flags-mode-advanced") {
      // Enter advanced flags mode (tracker #60). The flip writes
      // only the mode — the saved flags do not change — but it
      // closes the structured form, so unsaved structured edits
      // (a moved ctx/batch draft or a pending offload decision)
      // are discarded: confirm first, per the app's confirm
      // idiom, only when there is something to lose.
      const row = lastRows.find((r) => r.id === id);
      if (!row) return;
      const dirty = flagsCtx !== (row.n_ctx ?? 32768)
        || flagsBatch !== (row.n_batch ?? 512)
        || offloadPending !== null;
      if (dirty && !confirm("Switch to advanced flags? Unsaved changes in this form will be discarded. The saved flags themselves do not change.")) return;
      inFlight.add(id);
      paint();
      try {
        const r = await setBackendFlagsMode(id, true);
        if (disposed) return;
        if (r.error) {
          msgs.set(id, { text: `Advanced mode not enabled: ${r.error}`, err: true });
          paint();
          return;
        }
        msgs.set(id, { text: "Advanced flags mode on — the flags are edited as text in the panel.", err: false });
        await load();
        // Reopen the panel in its advanced form, on the saved
        // string the mode flip returned (byte-identical to what
        // was live — entering the mode writes no flags).
        const fresh = lastRows.find((x) => x.id === id);
        if (fresh) { flagsFor = null; openFlagsForm(fresh); }
      } finally {
        inFlight.delete(id);
        if (!disposed) paint();
      }
      return;
    }

    if (act === "flags-mode-structured") {
      // Leave advanced mode (tracker #60): the structured form
      // decodes the SAVED raw string through the existing
      // decoders, so unsaved text edits would be lost — confirm
      // when the text differs from the saved string.
      const row = lastRows.find((r) => r.id === id);
      if (!row) return;
      if (flagsRawText !== (row.raw_flags ?? "")
        && !confirm("Return to the structured form? The unsaved flag edits in the text box will be discarded. The saved flags do not change.")) return;
      inFlight.add(id);
      paint();
      try {
        const r = await setBackendFlagsMode(id, false);
        if (disposed) return;
        if (r.error) {
          msgs.set(id, { text: `Structured form not restored: ${r.error}`, err: true });
          paint();
          return;
        }
        msgs.set(id, { text: "Structured flags form restored — it shows the values decoded from the saved flags.", err: false });
        await load();
        const fresh = lastRows.find((x) => x.id === id);
        if (fresh) { flagsFor = null; openFlagsForm(fresh); }
      } finally {
        inFlight.delete(id);
        if (!disposed) paint();
      }
      return;
    }

    if (act === "flags-raw-save") {
      // Save the advanced editor's text (tracker #60). The op
      // journals like the other flags writes and applies on the
      // next start; nothing restarts. Warnings ride the result
      // and never block (the operator saw the same list live
      // while typing; the server's list replaces the draft's in
      // wire state). On success the Edit surface closes like the
      // structured Save's — see closeEditSession.
      inFlight.add(id);
      paint();
      try {
        // The Edit panel's model draft rides this Save too, with
        // the structured Save's rule: a failed assignment saves
        // nothing else and keeps the panel open.
        const mres = await saveEditModel(id);
        if (disposed) return;
        if (mres === "failed") { paint(); return; }
        const modelNote = mres === "saved" ? `Model set to ${flagsEditModelId}. ` : "";
        const text = flagsRawText;
        const r = await setBackendRawFlags(id, text.trim() === "" ? null : text);
        if (disposed) return;
        if (r.error) {
          msgs.set(id, { text: mres === "saved" ? `Model set to ${flagsEditModelId} — flags not saved: ${r.error}` : `Flags not saved: ${r.error}`, err: true });
          paint();
          return;
        }
        flagsRawText = r.data.raw_flags ?? "";
        flagsRawWarnings = r.data.warnings ?? [];
        msgs.set(id, {
          text: modelNote + (r.data.changed
            ? "Raw flags saved — applies on next start."
            : "Raw flags unchanged."),
          err: false,
        });
        // A successful save closes the Edit surface (Pete's
        // report, 2026-10-20) — the advanced editor previously
        // stayed open on the adopted text by design, which is
        // the behavior Pete filed against. The adopted text and
        // warnings above keep the wire state coherent; reopening
        // Edit reseeds from the saved row regardless.
        closeEditSession();
        await load();
      } finally {
        inFlight.delete(id);
        if (!disposed) paint();
      }
      return;
    }

    if (act === "offload-override") {
      // Save the pending guardrail-blocked position with an explicit
      // override. A server error surfaces on the card and the
      // decision row stays (the helper keeps the pending state and
      // the draft), so the operator can still snap instead.
      const pending = offloadPending?.backendId === id ? offloadPending : null;
      if (!pending) return;
      await saveOffloadPosition(id, pending.layers, true);
      return;
    }

    if (act === "offload-snap") {
      // Move the draft to the nearest verified position and save it
      // without an override. On error the draft returns to the
      // blocked position, keeping the decision row coherent.
      const pending = offloadPending?.backendId === id ? offloadPending : null;
      const row = lastRows.find((r) => r.id === id);
      const target = row?.offload?.guardrail?.nearest_verified_layers;
      if (!pending || !row?.offload || target === undefined) return;
      flagsOffloadLayers = target;
      const ok = await saveOffloadPosition(id, target, false);
      if (!ok && !disposed) {
        flagsOffloadLayers = pending.layers;
        paint();
      }
      return;
    }

    if (act === "offload-set-verified") {
      // The blocked card line's resolving action: save the nearest
      // verified position (no override). On error the helper has
      // already surfaced the message on the card.
      const row = lastRows.find((r) => r.id === id);
      const target = row?.offload?.guardrail?.nearest_verified_layers;
      if (!row?.offload || target === undefined) return;
      await saveOffloadPosition(id, target, false);
      return;
    }

    if (act === "offload-override-arm") {
      // Arm the deliberate override's confirmation: the button
      // re-renders naming the exact position (layers + placed CC
      // set) the confirm will record. No command fires here.
      confirmOverride.add(id);
      paint();
      return;
    }

    if (act === "offload-override-confirm") {
      // Record the deliberate override for the SAVED position.
      // The key comes from the live row (saved layers + the
      // guardrail's placed CC set); the server re-derives it and
      // refuses any mismatch, so a placement or position change
      // since the arm surfaces as an error, never a wider grant.
      const row = lastRows.find((r) => r.id === id);
      const g = row?.offload?.guardrail;
      if (!row?.offload || !g) { confirmOverride.delete(id); paint(); return; }
      confirmOverride.delete(id);
      inFlight.add(id);
      paint();
      try {
        const r = await setBackendOffloadOverride(id, row.offload.layers, g.placed_ccs ?? [g.compute_capability]);
        if (disposed) return;
        if (r.error) msgs.set(id, { text: `Override not recorded: ${r.error}`, err: true });
        else msgs.set(id, { text: `Offload override recorded (${row.offload.layers} of ${row.offload.moe_layers} layers) — starts proceed.`, err: false });
        await load();
      } finally {
        inFlight.delete(id);
        if (!disposed) paint();
      }
      return;
    }

    if (act === "offload-override-clear") {
      // Drop the standing override: the saved position is blocked
      // again at the next start's preflight.
      inFlight.add(id);
      paint();
      try {
        const r = await clearBackendOffloadOverride(id);
        if (disposed) return;
        if (r.error) msgs.set(id, { text: `Override not cleared: ${r.error}`, err: true });
        else msgs.set(id, { text: "Offload override cleared — the saved position is blocked again.", err: false });
        await load();
      } finally {
        inFlight.delete(id);
        if (!disposed) paint();
      }
      return;
    }

    if (act === "argv") {
      if (openArgv.has(id)) { openArgv.delete(id); paint(); return; }
      if (!argvCache.has(id)) {
        // Only successful fetches are cached (per mount); a failure is
        // shown in the message area and the next toggle retries.
        inFlight.add(id);
        paint();
        try {
          const r = await getBackendLaunchCommand(id);
          if (disposed) return;
          if (r.error) msgs.set(id, { text: `Launch command failed: ${r.error}`, err: true });
          else argvCache.set(id, r.data.argv);
        } finally {
          inFlight.delete(id);
        }
        if (!argvCache.has(id)) { paint(); return; } // fetch failed: show the note, stay closed
      }
      openArgv.add(id);
      paint();
      return;
    }

    if (act === "start" || act === "restart" || act === "stop") {
      // Lifecycle actions run through the shared runner: the store's
      // pending overlay shows the transition on this card and on every
      // other surface at once, polling continues meanwhile (the server
      // reports the transition with its origin), and the result lands
      // in the card note.
      const row = lastRows.find((r) => r.id === id);
      if (act !== "stop" && row?.fit?.state === "wddm_paged" && !armedWddmStart.has(id)) {
        // WDDM paged fits start only through the two-click confirm:
        // the first click arms and states the paged amount; the
        // second click carries the confirmation to the gate.
        armedWddmStart.add(id);
        msgs.set(id, { text: `${fmtGb(row.fit.paged_mib / 1024)} GiB will page to system RAM. Click ${act === "start" ? "Start" : "Restart"} again to confirm.`, err: false });
        paint();
        return;
      }
      const allow = (act !== "stop" && armedWddmStart.has(id)) || undefined;
      armedWddmStart.delete(id);
      msgs.delete(id);
      const r = await runLifecycle({ kind: "backend", id }, act, { origin: "ui", allowWddmOvercommit: allow });
      if (disposed) return;
      if (!r.ok) msgs.set(id, { text: r.message, err: true });
      else msgs.delete(id);
      await load();
      return;
    }

    inFlight.add(id);
    paint();
    try {
      if (act === "enable" || act === "disable") {
        const r = await applyBackendConfig(id, { enabled: act === "enable" }, "OnNextRestart");
        if (r.error) msgs.set(id, { text: `${act} failed: ${r.error}`, err: true });
        else if (r.data.persisted === false) {
          // Code-review 5.2: warn instead of silently succeeding.
          msgs.set(id, { text: `${act === "enable" ? "Enabled" : "Disabled"} for this session only — the change was not saved and will be lost when the app restarts.`, err: true });
        } else msgs.delete(id);
        await load();
        return;
      }
      if (act === "remove") {
        if (!confirmRemove.has(id)) {
          confirmRemove.add(id);
          msgs.set(id, { text: "Click the highlighted remove icon again to delete this backend and its group memberships.", err: false });
          paint();
          return;
        }
        confirmRemove.delete(id);
        const r = await deleteBackend(id);
        if (r.error) msgs.set(id, { text: `Remove failed: ${r.error}`, err: true });
        await load();
        return;
      }
      if (act === "toolcall") {
        // The probe persists its outcome on the backend (the card's
        // Tool calls line), so a reload shows the measured state;
        // the probe's own detail rides the card note meanwhile.
        const r = await testBackendToolCall(id);
        if (disposed) return;
        if (r.error) msgs.set(id, { text: `Tool-call test failed: ${r.error}`, err: true });
        else msgs.set(id, { text: `Tool calls: ${r.data.outcome} — ${r.data.detail}`, err: r.data.outcome !== "verified" });
        await load();
        return;
      }
      if (act === "test") {
        const r = await testBackend(id, "Reply with the single word: ready");
        if (disposed) return;
        if (r.error) {
          msgs.set(id, { text: `Test failed: ${r.error}`, err: true });
        } else if (r.data.content) {
          const reply = r.data.content.trim();
          const excerpt = reply.length > 120 ? `${reply.slice(0, 120)}…` : reply;
          msgs.set(id, { text: `Test reply (${r.data.latency_ms} ms): ${excerpt}`, err: false });
        } else {
          msgs.set(id, { text: `Test failed: HTTP ${r.data.status} — no reply content.`, err: true });
        }
        paint();
        return;
      }
    } finally {
      inFlight.delete(id);
      if (!disposed) paint();
    }
  };
  const onKeyDown = (ev: KeyboardEvent): void => {
    if (ev.key === "Escape" && openMenus.size) {
      openMenus.clear();
      paint();
    }
  };
  root.addEventListener("click", onClick);
  root.addEventListener("click", onOvGoto);
  root.addEventListener("keydown", onKeyDown);

  await load();

  // Keep the view's data fresh (a backend can die outside this view).
  // Polling continues during lifecycle actions: the shared store's
  // pending overlay and the server's own transition keep the cards
  // truthful meanwhile (UI-5).
  const timer = window.setInterval(() => {
    if (!disposed) void load();
  }, 5000);
  // Lifecycle state between loads comes from the shared store (fast
  // while anything is in transition, immediate on the backend's
  // lifecycle events): fold its rows' state into the wire rows and
  // repaint only when an effective state actually changed.
  const unsubscribeStore = serviceStore.subscribe((snap: ServiceSnapshot) => {
    if (disposed || !wireRows.length) return;
    const fresh = new Map(snap.backends.map((r) => [r.id, r]));
    wireRows = wireRows.map((r) => {
      const f = fresh.get(r.id);
      return f ? { ...r, state: f.state, lifecycle: f.lifecycle, transition: f.transition ?? null, pid: f.pid, failure_reason: f.failure_reason } : r;
    });
    const next = wireRows.map(effRow);
    if (lcSignature(next) === lcSignature(lastRows)) return;
    lastRows = next;
    const sliderFocused = (document.activeElement as HTMLElement | null)?.hasAttribute?.("data-offload-slider");
    const rawFocused = (document.activeElement as HTMLElement | null)?.hasAttribute?.("data-flags-raw");
    if (!sliderFocused && !rawFocused) paint();
  });

  return () => {
    disposed = true;
    unsubscribeStore();
    window.clearInterval(timer);
    stopPredictTicker();
    root.removeEventListener("click", onClick);
    root.removeEventListener("click", onOvGoto);
    root.removeEventListener("keydown", onKeyDown);
    root.removeEventListener("change", syncEditModel);
    root.removeEventListener("change", syncFlagsDraft);
    root.removeEventListener("input", syncFlagsDraft);
    root.removeEventListener("input", syncOffloadDraft);
    root.removeEventListener("change", commitOffload);
    root.removeEventListener("change", syncPlaceDraft);
    // `root` is the persistent #view container, so every listener
    // wireBackends adds must be removed here or it survives the visit
    // and fires (with stale closures) on every later visit (code
    // review v0.1.60 UI-5).
    root.removeEventListener("input", syncRawDraft);
    root.removeEventListener("change", changeMtp);
    root.removeEventListener("click", clickMoeCacheSwitch);
    root.removeEventListener("change", changeMoeCacheSize);
    root.removeEventListener("change", commitParallel);
    root.removeEventListener("change", changeCache);
    root.removeEventListener("click", clickCacheBest);
    refreshBtn?.removeEventListener("click", onRefresh);
  };
}

/* ---------------- Models ---------------- */

/** A hash counts as pinned exactly when the backend accepts it as
 *  one: 64 characters, all hex (the placeholder text the catalog ships
 *  fails both halves of the test). */
function hashPinned(m: ModelRow): boolean {
  return typeof m.sha256 === "string" && m.sha256.length === 64 && /^[0-9a-fA-F]+$/.test(m.sha256);
}

function modelShaBadge(m: ModelRow): string {
  // "Verified" asserts a file on disk matched the pin — a verified_at
  // stamp whose file is gone must not keep claiming it.
  if (m.verified_at && m.on_disk) {
    return `<span class="chip badge-ok">${icon("shield")} sha256 verified ${esc(fmtDate(m.verified_at))}</span>`;
  }
  if (!hashPinned(m)) {
    // A static configuration state, not a transition — gray, with
    // the warn icon and the explanation in the tooltip (principle 11).
    return `<span class="chip" title="No SHA-256 is pinned for this model — its downloads run unverified until a hash is recorded in the configuration.">${icon("warn")} hash not pinned</span>`;
  }
  return `<span class="chip">${icon("shield")} pinned, not verified</span>`;
}

type ModelNote = { text: string; err: boolean };

/** Overview list for the Models view: one line per model with column
 *  headings, divider lines, and a status button that scrolls to the
 *  model's card. The status reflects the file/download lifecycle:
 *  verified > on disk unverified > partial > downloading/verifying >
 *  disabled > not downloaded; a recorded download failure outranks
 *  all but an in-flight retry. */
function modelOverviewHtml(rows: ModelRow[], stats: Map<string, { backends: number; req?: number }>): string {
  if (!rows.length) return "";
  const body = rows.map((m) => {
    const st = stats.get(m.id);
    let btn: string;
    let stateTxt: string;
    if (m.download_error && !m.downloading) { btn = ovStatusBtn("err", "err", `card-model-${m.id}`, `${m.id}: download failed — go to card`); stateTxt = "download failed"; }
    else if (m.downloading) { btn = ovStatusBtn("warn", m.paused ? "pause" : "clock", `card-model-${m.id}`, `${m.id}: ${m.paused ? "download paused" : "downloading"} — go to card`); stateTxt = m.paused ? "download paused" : "downloading"; }
    else if (m.verifying) { btn = ovStatusBtn("warn", "shield", `card-model-${m.id}`, `${m.id}: verifying checksum — go to card`); stateTxt = "verifying checksum"; }
    else if (!m.enabled) { btn = ovStatusBtn("idle", "pause", `card-model-${m.id}`, `${m.id}: disabled — go to card`); stateTxt = "disabled"; }
    else if (m.on_disk && m.verified_at) { btn = ovStatusBtn("ok", "check", `card-model-${m.id}`, `${m.id}: downloaded and verified — go to card`); stateTxt = "verified"; }
    else if (m.on_disk) { btn = ovStatusBtn("warn", "shield", `card-model-${m.id}`, `${m.id}: on disk, checksum not verified — go to card`); stateTxt = "on disk, not verified"; }
    else if ((m.partial_bytes ?? 0) > 0) { btn = ovStatusBtn("warn", "warn", `card-model-${m.id}`, `${m.id}: partial download — go to card`); stateTxt = "partial download"; }
    else { btn = ovStatusBtn("idle", "download", `card-model-${m.id}`, `${m.id}: not downloaded — go to card`); stateTxt = "not downloaded"; }
    const size = m.on_disk ? fmtBytes(m.size_bytes) : (m.partial_bytes ? `${fmtBytes(m.partial_bytes)} partial` : "\u2014");
    return `<div class="ov-row">${btn}<span class="ov-name" title="${esc(m.id)}">${esc(modelShortName(m))}</span><span class="ov-dim">${esc(m.quant)}</span><span class="ov-dim">${esc(size)}</span><span class="ov-dim">${esc(stateTxt)}</span><span class="ov-dim">${st ? fmtCount(st.backends) : "0"}</span><span class="ov-dim">${st?.req !== undefined ? fmtCount(st.req) : "\u2014"}</span></div>`;
  }).join("");
  return `<div class="ovlist ov-m"><div class="ov-head"><span></span><span>Model</span><span>Quant</span><span>Size</span><span>State</span><span>Serving backends</span><span>Requests (1h)</span></div>${body}</div>`;
}

/** Recover (repo, file) from an HF resolve URL — the inverse
 *  of the source URL an Add Model pick constructs. The Edit
 *  form's re-fetch (P2-D8) is offered only when this succeeds;
 *  mirrors manager_app::hf_forms::parse_hf_source_url. */
function parseHfSourceUrl(url: string): { repo: string; file: string } | null {
  const schemeEnd = url.indexOf("://");
  if (schemeEnd < 0) return null;
  const rest = url.slice(schemeEnd + 3);
  if (!rest.startsWith("huggingface.co/")) return null;
  const path = rest.slice("huggingface.co/".length);
  const idx = path.indexOf("/resolve/");
  if (idx < 0) return null;
  const repo = path.slice(0, idx);
  const tail = path.slice(idx + "/resolve/".length);
  const slash = tail.indexOf("/");
  if (!repo.includes("/") || slash < 0) return null;
  const file = tail.slice(slash + 1);
  return file ? { repo, file } : null;
}

function modelCard(m: ModelRow, downloading: Set<string>, notes: Map<string, ModelNote>, stats?: { backends: number; req?: number; err?: number; toks?: number }, rename?: { draft: string } | null, detailsOpen = false, offload?: { layers: string; def: string; moe: boolean; mtp: boolean } | null, hfRefetch?: HfHeaderResult | null): string {
  const note = notes.get(m.id);
  const noteHtml = note
    ? `<div style="margin-top:8px;font-size:12px;color:${note.err ? "var(--err)" : "var(--text2)"}">${esc(note.text)}</div>` : "";
  // Backend-recorded terminal failure (transport error, or a hash
  // mismatch — which also removed the file). Shown until a new download
  // is started.
  const dlErrHtml = !m.downloading && m.download_error
    ? `<div style="margin-top:8px;font-size:12px;color:var(--err)">${esc(m.download_error)}</div>` : "";
  const partial = m.partial_bytes ?? 0;
  const inFlight = downloading.has(m.id) || m.downloading;
  // Spawn window: this view started the download but the spawn invoke
  // has not settled, so the backend does not track it yet — Pause and
  // Cancel would have nothing to act on and stay inert until it lands.
  const spawning = downloading.has(m.id) && !m.downloading;
  const spawnDis = spawning ? " disabled" : "";
  /* Lifecycle state as ONE chip, colored per principle 11:
   * downloading / paused / verifying are transitional (amber);
   * partial and a recorded download failure are light red. The
   * state sentences move into the chips' tooltips. */
  const stateChip = inFlight
    ? `<span class="chip badge-warn">${m.paused ? "Paused" : "Downloading"}</span>`
    : m.verifying
      ? `<span class="chip badge-warn" title="The download finished and the file is being checked against the pinned SHA-256.">Verifying checksum</span>`
      : partial > 0
        ? `<span class="chip badge-err" title="The file is incomplete and cannot be used yet.">Partial download</span>`
        : m.download_error
          ? `<span class="chip badge-err">Download failed</span>`
          : "";
  /* Download progress: one meter, fill = partial / total (on-disk
   * size when known, else the catalog estimate). The byte counts
   * appear once, as the value beside the bar — the Size tile no
   * longer repeats them. */
  const dlTotal = m.on_disk ? m.size_bytes : m.catalog_size_bytes;
  const progressHtml = (inFlight || partial > 0) && dlTotal !== null && dlTotal > 0 && partial > 0
    ? `<div style="display:flex;gap:10px;align-items:center;margin-top:8px">
        <span style="flex:1">${meter((partial / dlTotal) * 100, 300, 8, C.blue)}</span>
        <span style="font-size:12px;color:var(--text2)">Downloaded: ${esc(fmtBytes(partial))} of ${m.on_disk ? "" : "~"}${esc(fmtBytes(dlTotal))}${m.on_disk ? "" : " (estimate)"}</span>
      </div>`
    : "";
  let actions: string;
  if (inFlight && m.downloading && m.paused) {
    actions = `<button class="btn icon-only" data-model-act="resume" data-model-id="${esc(m.id)}" title="Resume download" aria-label="Resume download of ${esc(m.id)}">${icon("play")}</button>
      <button class="btn" data-model-act="cancel" data-model-id="${esc(m.id)}">${icon("x")} Cancel</button>`;
  } else if (inFlight) {
    actions = `<button class="btn icon-only" data-model-act="pause" data-model-id="${esc(m.id)}" title="Pause download" aria-label="Pause download of ${esc(m.id)}"${spawnDis}>${icon("pause")}</button>
      <button class="btn" data-model-act="cancel" data-model-id="${esc(m.id)}"${spawnDis}>${icon("x")} Cancel</button>`;
  } else if (m.verifying) {
    // The transfer finished; the backend watcher is hashing the file
    // against the pinned SHA-256. Not a plain complete state yet —
    // the state chip above carries it; no actions apply.
    actions = "";
  } else if (partial > 0) {
    actions = `<button class="btn icon-only" data-model-act="download" data-model-id="${esc(m.id)}" title="Resume download" aria-label="Resume download of ${esc(m.id)}">${icon("play")}</button>
      <button class="btn" data-model-act="delete" data-model-id="${esc(m.id)}">${icon("err")} Delete</button>`;
  } else if (!m.on_disk) {
    actions = `<button class="btn" data-model-act="download" data-model-id="${esc(m.id)}">${icon("download")} Download</button>`;
  } else {
    // Verify is only offered with a pinned hash — against an unpinned
    // placeholder it could never succeed (the badge explains the state).
    actions = `${hashPinned(m) ? `<button class="btn icon-only" data-model-act="verify" data-model-id="${esc(m.id)}" title="Verify checksum" aria-label="Verify checksum of ${esc(m.id)}">${icon("shield")}</button>` : ""}
      <button class="btn" data-model-act="delete" data-model-id="${esc(m.id)}">${icon("err")} Delete</button>`;
  }
  // Enable/disable is always offered: it controls whether the setup
  // wizard's download step includes this model. The control is the
  // shared state switch (rule 16).
  actions += ` ${enableSwitch({ on: m.enabled, name: `Model ${m.id}`, attrs: `data-model-act="toggle-enabled" data-model-id="${esc(m.id)}"` })}`;
  // Rename is always offered: the short name is the operator's own
  // label for the model, independent of its download state.
  actions += ` <button class="btn icon-only" data-model-act="rename" data-model-id="${esc(m.id)}" title="Rename short name" aria-label="Rename short name of ${esc(m.id)}">${icon("pencil")}</button>`;
  // The capability declaration editor is always offered too: it
  // is how a model outside the curated catalog acquires
  // expert-offload and MTP draft controls for its backends.
  actions += ` <button class="btn icon-only" data-model-act="offload" data-model-id="${esc(m.id)}" title="Capability declarations" aria-label="Capability declarations of ${esc(m.id)}">${icon("layers")}</button>`;
  const renameHtml = rename ? `<div class="backend-form-panel flags-panel">
      <div style="font-size:12px;font-weight:600">Rename short name</div>
      <div style="display:flex;gap:6px;margin-top:6px;min-width:0">
        <input data-model-rename-input="${esc(m.id)}" type="text" value="${esc(rename.draft)}" maxlength="64" style="${SELECT_STYLE};flex:1;min-width:0">
      </div>
      <div class="card-actions"><button class="btn" data-model-act="rename-save" data-model-id="${esc(m.id)}">${icon("check")} Save name</button>
        <button class="btn icon-only" data-model-act="rename-cancel" data-model-id="${esc(m.id)}" title="Cancel" aria-label="Cancel">${icon("x")}</button></div>
    </div>` : "";
  /* The effective offload figures: a catalog pin wins over the
   * record's declaration wherever the catalog carries one. */
  const effMoeLayers = m.catalog_moe_layers ?? m.moe_layers ?? null;
  const effMoeDefault = m.catalog_default_offload_layers ?? m.default_offload_layers ?? null;
  const catalogPinned = m.catalog_moe_layers != null && m.catalog_default_offload_layers != null;
  /* The effective MTP capability: the same precedence — the
   * catalog's pin (where it knows the model) over the record's
   * declaration. */
  const effMtp = m.catalog_mtp ?? m.mtp ?? false;
  const mtpPinned = m.catalog_mtp != null;
  /* Re-fetch proposals (P2-D8), rendered from the last
   * Refresh result held by wireModels: per-field, applied
   * explicitly, and never over a catalog pin. The changed
   * hash is a warning line only — a re-fetch never re-pins. */
  const hfSource = parseHfSourceUrl(m.source_url);
  const hfRefetchHtml = ((): string => {
    if (!offload || !hfRefetch) return "";
    const parts: string[] = [];
    if (hfRefetch.sha256 && hashPinned(m) && hfRefetch.sha256 !== m.sha256) {
      parts.push(`<div style="font-size:12px;color:var(--err);margin-top:6px" title="Stored: ${esc(m.sha256)} · Fetched: ${esc(hfRefetch.sha256)}">SHA-256 changed in the repository — Stored: ${esc(m.sha256.slice(0, 16))}… · Fetched: ${esc(hfRefetch.sha256.slice(0, 16))}…</div>`);
    }
    const hdr = hfRefetch.header;
    if (!hdr) {
      parts.push(`<div style="font-size:12px;color:var(--text2);margin-top:6px">File header could not be read.</div>`);
    } else if (!hfRefetch.catalog) {
      if (hdr.moe_layers != null && !catalogPinned && hdr.moe_layers !== (m.moe_layers ?? null)) {
        parts.push(`<div style="font-size:12px;color:var(--text2);margin-top:6px">MoE layers — Declared: ${m.moe_layers ?? "—"} · File: ${hdr.moe_layers} <button class="btn" data-model-act="offload-use-file" data-model-id="${esc(m.id)}" data-file-layers="${hdr.moe_layers}" style="padding:2px 8px;font-size:11px">Use file value</button></div>`);
      }
      if (hdr.mtp && !mtpPinned && !effMtp) {
        parts.push(`<div style="font-size:12px;color:var(--text2);margin-top:6px">MTP — File: head present <button class="btn" data-model-act="mtp-use-file" data-model-id="${esc(m.id)}" style="padding:2px 8px;font-size:11px">Declare MTP</button></div>`);
      }
    }
    return parts.join("");
  })();
  const offloadHtml = offload ? `<div class="backend-form-panel flags-panel">
      <div style="font-size:12px;font-weight:600">Capability declarations</div>
      <div style="margin-top:6px"><label style="font-size:12px;color:var(--text2);display:flex;gap:6px;align-items:center">
        <input data-model-mtp-check="${esc(m.id)}" type="checkbox" ${offload.mtp ? "checked" : ""}${mtpPinned ? ` disabled title="Pinned by the curated catalog"` : ""}> MTP capable</label></div>
      ${catalogPinned
        ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">This model's offload figures come from the curated catalog — ${effMoeDefault} of ${effMoeLayers} MoE layers by default — and cannot be changed here.</div>`
        : `<div style="margin-top:8px"><label style="font-size:12px;color:var(--text2);display:flex;gap:6px;align-items:center">
            <input data-model-moe-check="${esc(m.id)}" type="checkbox" ${offload.moe ? "checked" : ""}> MoE</label></div>
          <div style="font-size:12px;color:var(--text2);margin-top:6px">Declare this model as mixture-of-experts (MoE) to give its backends expert-offload controls: a new backend starts at the default offload, and the Edit Flags slider ranges over the MoE layers. Uncheck MoE and save to remove the declaration.</div>
          <div style="display:flex;gap:10px;margin-top:6px;flex-wrap:wrap">
            <label style="font-size:12px;color:var(--text2)">MoE layers<br>
              <input data-model-offload-layers="${esc(m.id)}" type="text" inputmode="numeric" value="${esc(offload.layers)}" placeholder="48" style="${SELECT_STYLE};width:110px"${offload.moe ? "" : " disabled"}></label>
            <label style="font-size:12px;color:var(--text2)">Default offload layers<br>
              <input data-model-offload-default="${esc(m.id)}" type="text" inputmode="numeric" value="${esc(offload.def)}" placeholder="16" style="${SELECT_STYLE};width:110px"${offload.moe ? "" : " disabled"}></label>
          </div>`}
      ${m.moe_reconcile ? `<div style="font-size:12px;color:var(--text2);margin-top:8px">MoE layers — Declared: ${m.moe_reconcile.declared} · File: ${m.moe_reconcile.file} <button class="btn" data-model-act="offload-use-file" data-model-id="${esc(m.id)}" data-file-layers="${m.moe_reconcile.file}" style="padding:2px 8px;font-size:11px">Use file value</button></div>` : ""}
      ${m.file_nextn && !mtpPinned && !effMtp ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">MTP — File: head present <button class="btn" data-model-act="mtp-use-file" data-model-id="${esc(m.id)}" style="padding:2px 8px;font-size:11px">Declare MTP</button></div>` : ""}
      ${hfSource && !(catalogPinned && mtpPinned) ? `<div style="margin-top:8px"><button class="btn" data-model-act="hf-refresh" data-model-id="${esc(m.id)}">${icon("search")} Refresh from Hugging Face</button></div>` : ""}
      ${hfRefetchHtml}
      ${catalogPinned && mtpPinned
        ? `<div class="card-actions"><button class="btn icon-only" data-model-act="offload-cancel" data-model-id="${esc(m.id)}" title="Close" aria-label="Close">${icon("x")}</button></div>`
        : `<div class="card-actions"><button class="btn" data-model-act="offload-save" data-model-id="${esc(m.id)}">${icon("check")} Save declaration</button>
          <button class="btn icon-only" data-model-act="offload-cancel" data-model-id="${esc(m.id)}" title="Cancel" aria-label="Cancel">${icon("x")}</button></div>`}
    </div>` : "";
  // Size carries the final size only when the file is on disk; in
  // download states the progress value carries the partial figure,
  // and before any download the catalog estimate is the size.
  const sizeText = m.on_disk
    ? esc(fmtBytes(m.size_bytes))
    : m.catalog_size_bytes !== null ? `~${esc(fmtBytes(m.catalog_size_bytes))} (estimate)` : "not downloaded";
  /* Stats row: only when a metrics window exists (req defined).
   * Each field is its own tile; a missing field omits its tile —
   * never a coerced 0 (principle 12). The error-share bar encodes
   * the proportion; the counts live in the tiles. No window: no
   * stats row and no placeholder sentence. */
  const statsHtml = stats && stats.backends > 0 && stats.req !== undefined
    ? `<div style="display:grid;grid-template-columns:repeat(3,1fr);gap:12px;margin:0 0 8px;align-items:center">
        <div class="kv"><span class="k">Requests (1h)</span><span class="v" style="font-size:12px">${fmtCount(stats.req)}</span></div>
        ${stats.err !== undefined ? `<div class="kv"><span class="k">Errors (1h)</span><span class="v" style="font-size:12px">${fmtCount(stats.err)}</span></div>` : ""}
        ${stats.toks !== undefined ? `<div class="kv"><span class="k">Throughput</span><span class="v" style="font-size:12px">${trim1(stats.toks)} tok/s</span></div>` : ""}
      </div>
      ${stats.err !== undefined && stats.req > 0 ? stackedBar([
        { pct: (stats.err / stats.req) * 100, color: C.red, label: `Errors: ${fmtCount(stats.err)} of ${fmtCount(stats.req)}` },
        { pct: ((stats.req - stats.err) / stats.req) * 100, color: "#3a4350", label: `Succeeded: ${fmtCount(stats.req - stats.err)} of ${fmtCount(stats.req)}` },
      ], 600, 6) : ""}`
    : "";
  /* Details expandable: the full hash (with copy), verified date,
   * full path, and model id. The 16-char fragment line is gone —
   * the hash is printed once, here, in full. No hash: no SHA-256
   * row. */
  const detailsHtml = `<div style="margin-top:8px">
      <button class="btn" data-model-act="details" data-model-id="${esc(m.id)}" aria-expanded="${detailsOpen}" title="Details" aria-label="Details" style="padding:2px 8px;font-size:11px">${icon(detailsOpen ? "chevD" : "chevR")}</button>
      ${detailsOpen ? `<div style="display:grid;gap:4px;margin-top:8px;font-size:12px">
        ${m.sha256 ? `<div style="display:flex;gap:8px;align-items:center;min-width:0"><span style="color:var(--text2)">SHA-256:</span><span style="font-family:var(--mono);font-size:11px;word-break:break-all">${esc(m.sha256)}</span><button class="btn" data-model-act="copy-hash" data-model-id="${esc(m.id)}" title="Copy SHA-256" aria-label="Copy SHA-256" style="padding:1px 7px;font-size:11px">${icon("copy")}</button></div>` : ""}
        ${m.verified_at ? `<div><span style="color:var(--text2)">Verified:</span> ${esc(fmtDate(m.verified_at))}</div>` : ""}
        <div style="min-width:0"><span style="color:var(--text2)">File:</span> <span style="font-family:var(--mono);font-size:11px;word-break:break-all">${esc(m.gguf_path)}</span></div>
        <div><span style="color:var(--text2)">Model ID:</span> <span style="font-family:var(--mono)">${esc(m.id)}</span></div>
      </div>` : ""}
    </div>`;
  return `<div class="card" id="card-model-${esc(m.id)}">
    <h3>${icon("box")} ${esc(modelShortName(m))} <span class="chip">${esc(m.quant)}</span>${m.enabled ? "" : ` <span class="chip">disabled</span>`}
      <span class="spacer" style="flex:1"></span>
      ${modelShaBadge(m)}${hashPinned(m) ? ` <button class="btn" data-model-act="copy-hash" data-model-id="${esc(m.id)}" title="Copy the full SHA-256" aria-label="Copy SHA-256" style="padding:2px 8px;font-size:11px">${icon("copy")}</button>` : ""}</h3>
    ${modelShortName(m) !== m.id ? `<div style="font-size:11px;color:var(--text3);font-family:var(--mono);margin-top:2px">${esc(m.id)}</div>` : ""}
    ${stateChip || progressHtml ? `<div style="margin-top:8px">${stateChip}${progressHtml}</div>` : ""}
    <div style="display:grid;grid-template-columns:1fr 1fr 1fr;gap:12px;margin:12px 0;align-items:center">
      <div class="kv"><span class="k">File</span><span class="v" style="font-size:12px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap" title="${esc(m.gguf_path)}">${esc(m.gguf_path)}</span> <button class="btn" data-model-act="copy-path" data-model-id="${esc(m.id)}" title="Copy file path" aria-label="Copy file path" style="padding:1px 7px;font-size:11px">${icon("copy")}</button></div>
      <div class="kv"><span class="k">Size</span><span class="v" style="font-size:12px">${sizeText}</span></div>
      ${m.params_b > 0 ? `<div class="kv"><span class="k">Parameters</span><span class="v" style="font-size:12px">${trim1(m.params_b)}B</span></div>` : ""}
      ${m.ctx_max != null ? `<div class="kv"><span class="k">Context</span><span class="v" style="font-size:12px">${fmtCount(m.ctx_max)}</span></div>` : ""}
      ${effMoeLayers != null && effMoeDefault != null ? `<div class="kv"><span class="k">Expert offload</span><span class="v" style="font-size:12px">Default: ${effMoeDefault} of ${effMoeLayers} layers</span></div>` : ""}
      ${effMtp ? `<div class="kv"><span class="k">MTP</span><span class="v" style="font-size:12px">Capable</span></div>` : ""}
    </div>
    ${statsHtml}
    <div class="card-actions">${actions}</div>
    ${detailsHtml}
    ${renameHtml}
    ${offloadHtml}
    ${dlErrHtml}
    ${noteHtml}
  </div>`;
}

/** The Add Model form card, painted into #modelAddForm by
 *  wireModels when the topbar's Add model button opens it (the
 *  Add Backend form's host pattern). Closed = the host is empty,
 *  so a successful submit clears and hides the form by emptying
 *  the host; the page's pre-open state is the topbar button. */
function modelAddFormHtml(): string {
  return `<div class="card"><h3>${icon("box")} Add a model<span class="info-tip" title="The model is added enabled. A model picked from Hugging Face starts pinned to the picked file's SHA-256; a manually entered model has no pinned SHA-256 — its downloads run unverified until a hash is recorded in the configuration.">${icon("info")}</span></h3>
    <div style="display:grid;grid-template-columns:1fr auto;gap:10px;margin-top:10px;align-items:end">
      <label style="font-size:12px;color:var(--text2)">Model name<br>
        <span style="position:relative;display:block">
          <input id="addModelName" type="text" placeholder="qwen3 coder" autocomplete="off" style="${SELECT_STYLE};width:100%">
          <span id="addModelSuggest" style="display:none;position:absolute;left:0;right:0;top:100%;z-index:30;max-height:264px;overflow-y:auto;background:var(--panel);border:1px solid var(--border);border-radius:6px"></span>
        </span></label>
      <button class="btn" id="addModelLookupBtn">${icon("search")} Look up</button>
    </div>
    <div id="addModelPick"></div>
    <div style="display:grid;grid-template-columns:1fr 2fr;gap:10px;margin-top:10px">
      <label style="font-size:12px;color:var(--text2)">Model id <span id="addModelIdSrc"></span><br>
        <input id="addModelId" type="text" placeholder="my-model-7b" style="${SELECT_STYLE};width:100%"></label>
      <label style="font-size:12px;color:var(--text2)">Source URL <span id="addModelUrlSrc"></span><br>
        <input id="addModelUrl" type="text" placeholder="Model download URL (.gguf)" style="${SELECT_STYLE};width:100%"></label>
    </div>
    <div style="margin-top:10px"><button class="btn" id="addModelOptToggle" aria-expanded="false" title="Optional fields" aria-label="Optional fields" style="padding:2px 8px;font-size:11px">${icon("chevR")}</button></div>
    <div id="addModelOptional" style="display:none">
      <div style="display:grid;grid-template-columns:1fr 1fr 1fr;gap:10px;margin-top:10px">
        <label style="font-size:12px;color:var(--text2)">Quant <span id="addModelQuantSrc"></span><br>
          <input id="addModelQuant" type="text" placeholder="Q4_K_M" style="${SELECT_STYLE};width:100%"></label>
        <label style="font-size:12px;color:var(--text2)">File name <span id="addModelFileSrc"></span><span class="info-tip" title="Defaults to the URL's file name.">${icon("info")}</span><br>
          <input id="addModelFile" type="text" placeholder="model.gguf" style="${SELECT_STYLE};width:100%"></label>
        <label style="font-size:12px;color:var(--text2)">Parameters (B) <span id="addModelParamsSrc"></span><span class="info-tip" title="Total parameter count in billions (e.g. 80 for an 80B model — for a mixture-of-experts model, the total, not the active count). Required for models outside the curated catalog: the count feeds memory-fit estimation and is never re-derived later.">${icon("info")}</span><br>
          <input id="addModelParams" type="text" inputmode="decimal" placeholder="80" style="${SELECT_STYLE};width:100%"></label>
      </div>
      <div style="display:flex;gap:16px;margin-top:10px">
        <label style="font-size:12px;color:var(--text2);display:flex;gap:6px;align-items:center">
          <input id="addModelMtp" type="checkbox"> MTP capable <span id="addModelMtpSrc"></span><span class="info-tip" title="The model's GGUF carries an MTP (multi-token prediction) head. Declaring it gives the model's backends the MTP draft control.">${icon("info")}</span></label>
        <label style="font-size:12px;color:var(--text2);display:flex;gap:6px;align-items:center">
          <input id="addModelMoe" type="checkbox"> MoE <span id="addModelMoeSrc"></span></label>
      </div>
      <div style="display:grid;grid-template-columns:1fr 1fr 1fr;gap:10px;margin-top:10px">
        <label style="font-size:12px;color:var(--text2)">MoE layers <span id="addModelMoeLayersSrc"></span><span class="info-tip" title="For a mixture-of-experts (MoE) model: the number of layers carrying routed experts. Declaring this and the default offload gives the model's backends expert-offload controls — a new backend starts at the default offload and the Edit Flags slider ranges over these layers.">${icon("info")}</span><br>
          <input id="addModelMoeLayers" type="text" inputmode="numeric" placeholder="48" style="${SELECT_STYLE};width:100%" disabled></label>
        <label style="font-size:12px;color:var(--text2)">Default offload layers <span id="addModelMoeDefaultSrc"></span><span class="info-tip" title="How many MoE layers a new backend for this model offloads to system RAM by default (0 to the MoE layer count; the full count is full expert offload).">${icon("info")}</span><br>
          <input id="addModelMoeDefault" type="text" inputmode="numeric" placeholder="16" style="${SELECT_STYLE};width:100%" disabled></label>
      </div>
    </div>
    <div class="card-actions"><button class="btn" id="addModelBtn">${icon("check")} Create model</button>
      <button class="btn icon-only" id="addModelCancel" title="Cancel" aria-label="Cancel">${icon("x")}</button></div>
    <div id="addModelMsg" style="font-size:12px;margin-top:6px;display:none"></div>
  </div>`;
}

export function renderModels(): string {
  return `${topbar("Models", `<button class="btn" id="modelAdd" title="Add model">${icon("plus")} Add model</button>`)}
  <div class="view"><div id="modelBanner"></div>
  <div id="modelOverview"></div>
  <div id="modelBody">${loadingHtml("Loading models…")}</div>
  <div id="modelAddForm"></div></div>`;
}

export async function wireModels(root: HTMLElement): Promise<() => void> {
  /** Downloads started from this view whose spawn invoke has not settled
   *  yet; the backend's `downloading` flag carries the state after that. */
  const downloading = new Set<string>();
  const notes = new Map<string, ModelNote>();
  let lastRows: ModelRow[] = [];
  /** Rename form state: the model whose inline rename form is open,
   *  and its draft text. Kept outside the repainted DOM (synced on
   *  input) so poll repaints never lose typed text. */
  let renameFor: string | null = null;
  let renameDraft = "";
  /** Capability-declaration form state: the model whose inline
   *  declaration form is open, and its draft fields (the MoE pair
   *  kept as text, like the rename draft, so poll repaints never
   *  lose typed text; the two capability checkboxes as booleans). */
  let offloadFor: string | null = null;
  let offloadDraftLayers = "";
  let offloadDraftDefault = "";
  let offloadDraftMoe = false;
  let offloadDraftMtp = false;
  /** Last Hugging Face re-fetch result per model (P2-D8):
   *  the declaration editor renders its proposals from this
   *  until the view is left. */
  const hfRefetchResults = new Map<string, HfHeaderResult>();
  const syncOffloadDraft = (ev: Event): void => {
    const t = ev.target as HTMLElement;
    if (t.hasAttribute("data-model-offload-layers") && t.getAttribute("data-model-offload-layers") === offloadFor) {
      offloadDraftLayers = (t as HTMLInputElement).value;
    }
    if (t.hasAttribute("data-model-offload-default") && t.getAttribute("data-model-offload-default") === offloadFor) {
      offloadDraftDefault = (t as HTMLInputElement).value;
    }
  };
  root.addEventListener("input", syncOffloadDraft);
  /** Checkbox drafts sync on change; toggling MoE also enables or
   *  disables the pair's inputs in place (no repaint, so the
   *  toggle never loses focus). */
  const syncOffloadChecks = (ev: Event): void => {
    const t = ev.target as HTMLElement;
    if (t.hasAttribute("data-model-mtp-check") && t.getAttribute("data-model-mtp-check") === offloadFor) {
      offloadDraftMtp = (t as HTMLInputElement).checked;
    }
    if (t.hasAttribute("data-model-moe-check") && t.getAttribute("data-model-moe-check") === offloadFor) {
      offloadDraftMoe = (t as HTMLInputElement).checked;
      const card = t.closest(".card");
      card?.querySelectorAll("[data-model-offload-layers],[data-model-offload-default]").forEach((el) => {
        (el as HTMLInputElement).disabled = !offloadDraftMoe;
      });
    }
  };
  root.addEventListener("change", syncOffloadChecks);
  /** Models whose Details expandable is open (wire state outside
   *  the repainted DOM, so poll repaints preserve it). */
  const openModelDetails = new Set<string>();
  const syncRenameDraft = (ev: Event): void => {
    const t = ev.target as HTMLElement;
    if (!t.hasAttribute("data-model-rename-input")) return;
    if (t.getAttribute("data-model-rename-input") !== renameFor) return;
    renameDraft = (t as HTMLInputElement).value;
  };
  root.addEventListener("input", syncRenameDraft);
  /** Enter in the rename input saves, like a form submit. */
  const onRenameKey = (ev: KeyboardEvent): void => {
    const t = ev.target as HTMLElement;
    if (!t.hasAttribute("data-model-rename-input") || ev.key !== "Enter") return;
    ev.preventDefault();
    const btn = root.querySelector(`[data-model-act="rename-save"][data-model-id="${CSS.escape(renameFor ?? "")}"]`) as HTMLButtonElement | null;
    btn?.click();
  };
  root.addEventListener("keydown", onRenameKey);
  /** Per-model gateway statistics, aggregated over the backends whose
   *  model_file matches the model's gguf_path. */
  let modelStats = new Map<string, { backends: number; req?: number; err?: number; toks?: number }>();
  let pollTimer: number | null = null;
  let disposed = false;

  /** Poll every 2s while any download is in flight (or verifying) so
   *  progress and the terminal state (complete, verified, failed)
   *  reach the view; stop when nothing is in flight. */
  const syncPoll = (): void => {
    // A load that resolves after the view was left must not start a
    // poll the (already-run) cleanup can never clear (UI-8).
    if (disposed) return;
    const any = downloading.size > 0 || lastRows.some((m) => m.downloading || m.verifying);
    if (any && pollTimer === null) {
      pollTimer = window.setInterval(() => { void load(); }, 2000);
    } else if (!any && pollTimer !== null) {
      window.clearInterval(pollTimer);
      pollTimer = null;
    }
  };

  const load = async (): Promise<void> => {
    const r = await getModels();
    if (disposed) return;
    const br = await getBackends();
    // Re-check after the second await too: a navigation away and back
    // while list_backends was in flight left this disposed instance
    // painting into the new view and starting a poll interval nothing
    // cleared (code review v0.1.60 UI-8).
    if (disposed) return;
    if (!br.error) {
      const m = new Map<string, { backends: number; req?: number; err?: number; toks?: number }>();
      for (const model of r.data) {
        const serving = br.data.filter((b) => b.configured && sameModelFile(b.model_file, model.gguf_path));
        if (!serving.length) continue;
        const withWin = serving.filter((b) => b.requests_1h !== undefined);
        // Throughput sums only backends whose live rate is
        // measured; absent when none is (unknown is not zero).
        const withTput = serving.filter((b) => b.throughput_toks !== undefined);
        m.set(model.id, withWin.length
          ? {
              backends: serving.length,
              req: withWin.reduce((a, b) => a + (b.requests_1h ?? 0), 0),
              err: withWin.reduce((a, b) => a + (b.errors_1h ?? 0), 0),
              toks: withTput.length
                ? withTput.reduce((a, b) => a + (b.throughput_toks ?? 0), 0)
                : undefined,
            }
          : { backends: serving.length });
      }
      modelStats = m;
    }
    setBanner(root, "modelBanner", bannerHtml(r));
    const b = root.querySelector("#modelBody");
    if (!b) return;
    const mov = root.querySelector("#modelOverview");
    if (r.error) { if (mov) mov.innerHTML = ""; b.innerHTML = emptyHtml(`Models unavailable: ${r.error}`); return; }
    if (!r.data.length) {
      lastRows = [];
      if (mov) mov.innerHTML = "";
      b.innerHTML = emptyHtml(r.live
        ? "No models are configured. Add one with the Add model button."
        : "No model data in sample mode.");
      syncPoll();
      return;
    }
    lastRows = r.data;
    // While a rename or offload input has focus, a poll repaint
    // would drop the caret mid-word; the drafts are synced to
    // wire state, so skipping one repaint loses nothing.
    const active = document.activeElement as HTMLElement | null;
    const formFocused = (renameFor !== null && !!active?.hasAttribute?.("data-model-rename-input"))
      || (offloadFor !== null && (!!active?.hasAttribute?.("data-model-offload-layers") || !!active?.hasAttribute?.("data-model-offload-default")));
    if (!formFocused) {
      if (mov) mov.innerHTML = modelOverviewHtml(lastRows, modelStats);
      b.innerHTML = `<div class="cards-fluid">${lastRows.map((m) => modelCard(m, downloading, notes, modelStats.get(m.id), renameFor === m.id ? { draft: renameDraft } : null, openModelDetails.has(m.id), offloadFor === m.id ? { layers: offloadDraftLayers, def: offloadDraftDefault, moe: offloadDraftMoe, mtp: offloadDraftMtp } : null, hfRefetchResults.get(m.id) ?? null)).join("")}</div>`;
    }
    syncPoll();
  };

  const onClick = async (ev: Event): Promise<void> => {
    const t = (ev.target as HTMLElement).closest("[data-model-act]") as HTMLElement | null;
    if (!t) return;
    const id = t.getAttribute("data-model-id") ?? "";
    const act = t.getAttribute("data-model-act") ?? "";

    if (act === "details") {
      if (openModelDetails.has(id)) openModelDetails.delete(id); else openModelDetails.add(id);
      await load();
      return;
    }

    if (act === "copy-hash" || act === "copy-path") {
      const row = lastRows.find((m) => m.id === id);
      const value = act === "copy-hash" ? row?.sha256 : row?.gguf_path;
      if (!value) return;
      try {
        await navigator.clipboard.writeText(value);
        notes.set(id, { text: act === "copy-hash" ? "SHA-256 copied to the clipboard." : "File path copied to the clipboard.", err: false });
      } catch {
        notes.set(id, { text: "Copy failed — copy the value manually.", err: true });
      }
      await load();
      return;
    }

    if (act === "download") {
      // Disable synchronously so a second click cannot fire while the
      // first spawn is in flight (the repaint also replaces the button).
      (t as HTMLButtonElement).disabled = true;
      downloading.add(id);
      // A fresh attempt clears any stale note (the backend clears the
      // recorded download_error when a new download starts).
      notes.delete(id);
      await load();
      const r = await downloadModel(id);
      downloading.delete(id);
      if (r.error) {
        notes.set(id, { text: `Download failed: ${r.error}`, err: true });
      }
      await load();
      return;
    }

    if (act === "pause") {
      (t as HTMLButtonElement).disabled = true;
      const r = await pauseModelDownload(id);
      if (r.error) notes.set(id, { text: `Pause failed: ${r.error}`, err: true });
      await load();
      return;
    }

    if (act === "resume") {
      (t as HTMLButtonElement).disabled = true;
      const r = await resumeModelDownload(id);
      if (r.error) notes.set(id, { text: `Resume failed: ${r.error}`, err: true });
      await load();
      return;
    }

    if (act === "cancel") {
      if (!confirm(`Cancel the download of ${id}? The downloaded progress will be discarded — the partial file is deleted.`)) return;
      const r = await cancelDownload(id);
      if (r.error) {
        notes.set(id, { text: `Cancel failed: ${r.error}`, err: true });
      } else {
        downloading.delete(id);
        notes.set(id, { text: "Download cancelled.", err: false });
      }
      await load();
      return;
    }

    if (act === "toggle-enabled") {
      (t as HTMLButtonElement).disabled = true;
      const row = lastRows.find((m) => m.id === id);
      const target = !(row?.enabled ?? true);
      const r = await modelSetEnabled(id, target);
      if (r.error) {
        notes.set(id, { text: `${target ? "Enable" : "Disable"} failed: ${r.error}`, err: true });
      } else {
        notes.set(id, {
          text: target
            ? "Enabled — the setup wizard's download step will include this model."
            : "Disabled — the setup wizard's download step will skip this model. The file and configuration are kept.",
          err: false,
        });
      }
      await load();
      return;
    }

    if (act === "rename") {
      if (renameFor === id) { renameFor = null; await load(); return; }
      const row = lastRows.find((m) => m.id === id);
      renameFor = id;
      renameDraft = row ? modelShortName(row) : id;
      await load();
      const input = root.querySelector(`[data-model-rename-input="${CSS.escape(id)}"]`) as HTMLInputElement | null;
      input?.focus();
      input?.select();
      return;
    }

    if (act === "rename-cancel") {
      renameFor = null;
      await load();
      return;
    }

    if (act === "rename-save") {
      (t as HTMLButtonElement).disabled = true;
      const r = await modelSetShortName(id, renameDraft);
      if (r.error) {
        notes.set(id, { text: `Rename failed: ${r.error}`, err: true });
      } else {
        renameFor = null;
        notes.set(id, { text: `Short name saved: ${r.data.name}`, err: false });
      }
      await load();
      return;
    }

    if (act === "offload") {
      if (offloadFor === id) { offloadFor = null; await load(); return; }
      const row = lastRows.find((m) => m.id === id);
      offloadFor = id;
      offloadDraftLayers = row?.moe_layers != null ? String(row.moe_layers) : "";
      offloadDraftDefault = row?.default_offload_layers != null ? String(row.default_offload_layers) : "";
      // The checkboxes start from the EFFECTIVE capabilities
      // (catalog pin over the record's declaration).
      offloadDraftMoe = (row?.catalog_moe_layers ?? row?.moe_layers) != null
        && (row?.catalog_default_offload_layers ?? row?.default_offload_layers) != null;
      offloadDraftMtp = row?.catalog_mtp ?? row?.mtp ?? false;
      await load();
      return;
    }

    if (act === "offload-cancel") {
      offloadFor = null;
      await load();
      return;
    }

    if (act === "offload-use-file") {
      // "Use file value" (P2-D7): the file's parsed MoE layer
      // count fills the editor's layers field — a proposal in
      // the form, persisted by Save declaration through
      // model_set_offload_spec like any other edit. If the
      // stored default exceeds the corrected count, Save's
      // validation refuses and the operator adjusts the
      // default in this same editor; nothing clamps silently.
      const fileLayers = t.getAttribute("data-file-layers");
      if (fileLayers) {
        offloadDraftLayers = fileLayers;
        offloadDraftMoe = true;
      }
      await load();
      return;
    }

    if (act === "mtp-use-file") {
      (t as HTMLButtonElement).disabled = true;
      const r = await modelSetMtp(id, true);
      if (r.error) {
        notes.set(id, { text: `Declaration failed: ${r.error}`, err: true });
      } else {
        notes.set(id, { text: "MTP declared capable from the file's header: this model's backends gain the MTP draft control.", err: false });
      }
      await load();
      return;
    }

    if (act === "hf-refresh") {
      // Re-fetch (P2-D8): re-read this model's repo record
      // and file header from Hugging Face and hold the
      // result for the editor's proposal lines. Nothing is
      // written here — proposals apply per field.
      const row = lastRows.find((m) => m.id === id);
      const src = row ? parseHfSourceUrl(row.source_url) : null;
      if (!row || !src) return;
      (t as HTMLButtonElement).disabled = true;
      const r = await modelHfHeader(src.repo, src.file);
      if (r.error || !r.live) {
        notes.set(id, { text: `Refresh failed: ${r.error ?? "the re-fetch runs in the installed app."}`, err: true });
      } else {
        hfRefetchResults.set(id, r.data);
      }
      await load();
      return;
    }

    if (act === "offload-save") {
      (t as HTMLButtonElement).disabled = true;
      const row = lastRows.find((m) => m.id === id);
      const moePinned = row != null && row.catalog_moe_layers != null && row.catalog_default_offload_layers != null;
      const mtpPinned = row?.catalog_mtp != null;
      // MoE pair, validated before anything persists: checked
      // requires both fields; unchecked clears the declaration.
      let layers: number | null = null;
      let def: number | null = null;
      if (!moePinned && offloadDraftMoe) {
        const layersRaw = offloadDraftLayers.trim();
        const defRaw = offloadDraftDefault.trim();
        const l = Number(layersRaw);
        const d = Number(defRaw);
        if (!layersRaw || !defRaw || !Number.isInteger(l) || !Number.isInteger(d) || l < 1 || d < 0 || d > l) {
          notes.set(id, { text: "Declaration not saved: MoE is checked — give both fields, MoE layers of 1 or more and a default offload between 0 and the layer count (or uncheck MoE to remove the declaration).", err: true });
          await load();
          return;
        }
        layers = l;
        def = d;
      }
      const savedParts: string[] = [];
      if (!moePinned) {
        const r = await modelSetOffloadSpec(id, layers, def);
        if (r.error) {
          notes.set(id, { text: `Declaration failed: ${r.error}`, err: true });
          await load();
          return;
        }
        savedParts.push(layers === null
          ? "MoE offload declaration cleared."
          : `MoE offload declared: default ${def} of ${layers} layers. New backends start at the default, and this model's backends gain the Edit Flags offload slider.`);
      }
      if (!mtpPinned) {
        // Checked stores the declaration; unchecked clears it —
        // either way the effective capability follows the box.
        const r = await modelSetMtp(id, offloadDraftMtp ? true : null);
        if (r.error) {
          notes.set(id, { text: `Declaration failed: ${r.error}`, err: true });
          await load();
          return;
        }
        savedParts.push(offloadDraftMtp
          ? "MTP declared capable: this model's backends gain the MTP draft control."
          : "MTP declaration cleared.");
      }
      offloadFor = null;
      if (savedParts.length) notes.set(id, { text: savedParts.join(" "), err: false });
      await load();
      return;
    }

    if (act === "verify") {
      const btn = t as HTMLButtonElement;
      btn.disabled = true;
      const r = await verifyModel(id);
      if (r.error) {
        notes.set(id, { text: `Verify failed: ${r.error}`, err: true });
      } else {
        notes.set(id, r.data.ok
          ? { text: "Checksum verified — the file on disk matches the pinned SHA-256.", err: false }
          : { text: "Checksum mismatch — the file on disk does not match the pinned SHA-256.", err: true });
      }
      await load();
      return;
    }

    if (act === "delete") {
      if (!confirm(`Delete model ${id}? This removes the downloaded file and its config entry. The delete is refused while a backend references the model.`)) return;
      const r = await deleteModel(id);
      if (r.error) {
        notes.set(id, { text: `Delete failed: ${r.error}`, err: true });
      } else {
        notes.delete(id);
      }
      await load();
    }
  };
  root.addEventListener("click", onClick);
  root.addEventListener("click", onOvGoto);

  /* ---------------- Add model form ----------------
   * Lives in #modelAddForm, outside the repainted #modelBody, so
   * the poll never clobbers typed input (the Add Backend form's
   * host pattern). The topbar's Add model button opens it; a
   * successful create empties the host — the form clears and
   * hides, returning the page to its pre-open state — while a
   * failed create keeps the form open with the input intact. */
  const addModelHost = root.querySelector("#modelAddForm");
  const closeAddModelForm = (): void => { if (addModelHost) addModelHost.innerHTML = ""; };
  const openAddModelForm = (): void => {
    if (!addModelHost) return;
    if (addModelHost.innerHTML) { closeAddModelForm(); return; }
    addModelHost.innerHTML = modelAddFormHtml();
    const optToggle = addModelHost.querySelector("#addModelOptToggle") as HTMLButtonElement | null;
    optToggle?.addEventListener("click", () => {
      const box = addModelHost.querySelector("#addModelOptional") as HTMLElement | null;
      if (!box) return;
      const open = box.style.display === "none";
      box.style.display = open ? "" : "none";
      optToggle.setAttribute("aria-expanded", String(open));
      optToggle.innerHTML = icon(open ? "chevD" : "chevR");
    });
    addModelHost.querySelector("#addModelCancel")?.addEventListener("click", closeAddModelForm);
    // The MoE checkbox gates the declaration pair's inputs.
    const addMoeCheck = addModelHost.querySelector("#addModelMoe") as HTMLInputElement | null;
    addMoeCheck?.addEventListener("change", () => {
      const on = addMoeCheck.checked;
      (addModelHost.querySelector("#addModelMoeLayers") as HTMLInputElement | null)?.toggleAttribute("disabled", !on);
      (addModelHost.querySelector("#addModelMoeDefault") as HTMLInputElement | null)?.toggleAttribute("disabled", !on);
    });

    /* ---- Hugging Face lookup, two-step pick, and prefill
     * (deep-discovery design Part 2). The lookup is a form
     * aid only: every field stays manually editable, and a
     * failed or empty lookup leaves manual entry untouched.
     * Prefill writes only into empty fields and fields still
     * holding an unedited prefill (P2.8 rule 2); each
     * prefilled field carries its source chip beside the
     * label (principle 11), cleared when the operator edits
     * the field. Nothing is stored until Create model. ---- */
    const pickHost = addModelHost.querySelector("#addModelPick") as HTMLElement | null;
    const nameInput = addModelHost.querySelector("#addModelName") as HTMLInputElement | null;
    const suggestHost = addModelHost.querySelector("#addModelSuggest") as HTMLElement | null;
    interface HfPickState { repoId: string; file: string; url: string; fileName: string; sha256: string | null }
    let hfPick: HfPickState | null = null;
    let lastLookup: HfLookupRepo[] = [];
    let expandedRepo: string | null = null;
    const prefilled = new Set<string>();
    const fieldOf = (sel: string): HTMLInputElement | null => addModelHost.querySelector(sel) as HTMLInputElement | null;
    const setChip = (key: string, text: string | null): void => {
      const slot = addModelHost.querySelector(`#addModel${key}Src`) as HTMLElement | null;
      if (slot) slot.innerHTML = text ? `<span class="chip">${esc(text)}</span>` : "";
    };
    const setPrefill = (sel: string, key: string, value: string, chip: string): void => {
      const el = fieldOf(sel);
      if (!el) return;
      if (el.value.trim() !== "" && !prefilled.has(key)) return;
      el.value = value;
      prefilled.add(key);
      setChip(key, chip);
    };
    const watchEdit = (sel: string, key: string): void => {
      fieldOf(sel)?.addEventListener("input", () => { prefilled.delete(key); setChip(key, null); });
    };
    watchEdit("#addModelId", "Id");
    watchEdit("#addModelUrl", "Url");
    watchEdit("#addModelQuant", "Quant");
    watchEdit("#addModelFile", "File");
    watchEdit("#addModelParams", "Params");
    watchEdit("#addModelMoeLayers", "MoeLayers");
    watchEdit("#addModelMoeDefault", "MoeDefault");
    fieldOf("#addModelMtp")?.addEventListener("change", () => setChip("Mtp", null));
    addMoeCheck?.addEventListener("change", () => setChip("Moe", null));
    const setOptionalOpen = (open: boolean): void => {
      const box = addModelHost.querySelector("#addModelOptional") as HTMLElement | null;
      if (!box) return;
      box.style.display = open ? "" : "none";
      optToggle?.setAttribute("aria-expanded", String(open));
      if (optToggle) optToggle.innerHTML = icon(open ? "chevD" : "chevR");
    };
    const fitLabel = (fit: string): string => fit === "fits" ? "Fits" : fit === "fits_with_split" ? "Fits with split" : fit === "does_not_fit" ? "Does not fit" : "";
    const quantRowHtml = (r: HfLookupRepo, q: HfLookupQuant): string => {
      const fit = fitLabel(q.fit);
      return `<div style="display:flex;gap:8px;align-items:center;margin-top:6px;flex-wrap:wrap">
        <button class="btn" data-hf-quant="${esc(q.file)}" data-hf-repo="${esc(r.repo_id)}">${icon("check")} Pick</button>
        <span style="font-size:12px;color:var(--text2)">File: ${esc(q.file)} · Size: ${esc(fmtBytes(q.size_bytes))}${fit ? ` · Fit: ${fit}` : ""}</span>
      </div>`;
    };
    const renderPickList = (): void => {
      if (!pickHost) return;
      pickHost.innerHTML = lastLookup.map((r) => {
        const facts = [
          r.params_b != null ? `Params: ${trim1(r.params_b)}B` : "",
          r.architecture ? `Architecture: ${esc(r.architecture)}` : "",
          r.ctx_max != null ? `Rated context: ${fmtCount(r.ctx_max)}` : "",
          `Downloads: ${fmtCount(r.downloads)}`,
        ].filter(Boolean).join(" · ");
        const open = expandedRepo === r.repo_id;
        return `<div style="border:1px solid var(--border);border-radius:6px;padding:8px;margin-top:8px">
          <div style="display:flex;gap:8px;align-items:center;flex-wrap:wrap">
            <span style="font-size:12px;color:var(--text2)">Repo:</span>
            <button class="btn" data-hf-repo="${esc(r.repo_id)}" style="font-family:var(--mono);font-size:12px">${icon(open ? "chevD" : "chevR")} ${esc(r.repo_id)}</button>
            ${r.catalog_model_id ? `<span class="chip">Catalog</span>` : ""}
            <span style="font-size:12px;color:var(--text2)">${facts}</span>
          </div>
          ${open ? r.quants.map((q) => quantRowHtml(r, q)).join("") : ""}
        </div>`;
      }).join("");
    };
    const renderPickSummary = (note: string): void => {
      if (!pickHost || !hfPick) return;
      pickHost.innerHTML = `<div style="display:flex;gap:8px;align-items:center;margin-top:8px;flex-wrap:wrap">
          <span style="font-size:12px;color:var(--text2)">Repo: ${esc(hfPick.repoId)} · File: ${esc(hfPick.file)}</span>
          <button class="btn" id="addModelPickChange" style="padding:2px 8px;font-size:11px">Change</button>
        </div>
        ${note ? `<div style="font-size:12px;color:var(--text2);margin-top:4px">${esc(note)}</div>` : ""}`;
      pickHost.querySelector("#addModelPickChange")?.addEventListener("click", () => {
        expandedRepo = hfPick?.repoId ?? null;
        renderPickList();
      });
    };
    const applyPick = async (repo: HfLookupRepo, q: HfLookupQuant): Promise<void> => {
      const url = q.source_url;
      const fileName = q.file.split("/").pop() ?? q.file;
      hfPick = { repoId: repo.repo_id, file: q.file, url, fileName, sha256: q.shard_count === 1 ? q.sha256 : null };
      setPrefill("#addModelId", "Id", q.id_slug, "Hugging Face");
      setPrefill("#addModelUrl", "Url", url, "Hugging Face");
      setOptionalOpen(true);
      setPrefill("#addModelFile", "File", fileName, "Hugging Face");
      if (q.quant) setPrefill("#addModelQuant", "Quant", q.quant, "Hugging Face");
      if (repo.params_b != null) setPrefill("#addModelParams", "Params", trim1(repo.params_b), "Hugging Face");
      renderPickSummary("");
      // The pick-time header read (P2-D4): geometry and the
      // MTP signal prefill the capability declarations as
      // proposals. Any failure leaves the form as the record
      // prefill left it — the add is never blocked by it.
      const hr = await modelHfHeader(repo.repo_id, q.file);
      if (!hr.live || hr.error || !hr.data.header) {
        renderPickSummary("File header not read — MoE/MTP not prefilled.");
        return;
      }
      if (hr.data.catalog) {
        // A catalog file's pins govern; no fetched value is
        // prefilled over them (§P2.8 rule 1).
        renderPickSummary("Catalog model — its figures come from the curated catalog.");
        return;
      }
      const hdr = hr.data.header;
      let note = "";
      if (hdr.moe_layers != null) {
        const moeBox = fieldOf("#addModelMoe");
        if (moeBox && !moeBox.checked) {
          moeBox.checked = true;
          setChip("Moe", "File header");
        }
        fieldOf("#addModelMoeLayers")?.toggleAttribute("disabled", false);
        fieldOf("#addModelMoeDefault")?.toggleAttribute("disabled", false);
        setPrefill("#addModelMoeLayers", "MoeLayers", String(hdr.moe_layers), "File header");
        if (hdr.suggested_default_offload_layers != null) {
          setPrefill("#addModelMoeDefault", "MoeDefault", String(hdr.suggested_default_offload_layers), "Suggested");
        }
        if (hdr.needs_split) note = "Needs a tensor split to fit one GPU.";
      }
      if (hdr.mtp) {
        const mtpBox = fieldOf("#addModelMtp");
        if (mtpBox && !mtpBox.checked) {
          mtpBox.checked = true;
          setChip("Mtp", "File header");
        }
      }
      renderPickSummary(note);
    };
    const runLookup = async (query: string, expandRepo?: string): Promise<void> => {
      if (!pickHost) return;
      if (!isTauri()) {
        pickHost.innerHTML = `<div style="font-size:12px;color:var(--text2);margin-top:8px">Sample mode: the Hugging Face lookup runs in the installed app.</div>`;
        return;
      }
      pickHost.innerHTML = `<div style="font-size:12px;color:var(--text2);margin-top:8px">Looking up…</div>`;
      const r = await modelHfLookup(query);
      if (r.error) {
        pickHost.innerHTML = `<div style="font-size:12px;color:var(--err);margin-top:8px">Lookup failed: ${esc(r.error)}</div>`;
        return;
      }
      lastLookup = r.data.repos;
      expandedRepo = expandRepo ?? (lastLookup.length === 1 ? lastLookup[0].repo_id : null);
      if (!lastLookup.length) {
        pickHost.innerHTML = `<div style="font-size:12px;color:var(--text2);margin-top:8px">No repositories matched.</div>`;
        return;
      }
      renderPickList();
    };
    addModelHost.querySelector("#addModelLookupBtn")?.addEventListener("click", () => {
      const q = nameInput?.value.trim() ?? "";
      if (!q) {
        if (pickHost) pickHost.innerHTML = `<div style="font-size:12px;color:var(--err);margin-top:8px">Enter a model name to look up.</div>`;
        return;
      }
      hideSuggest();
      void runLookup(q);
    });
    pickHost?.addEventListener("click", (ev) => {
      const t = ev.target as HTMLElement;
      const quantBtn = t.closest("[data-hf-quant]") as HTMLElement | null;
      if (quantBtn) {
        const repo = lastLookup.find((r) => r.repo_id === quantBtn.getAttribute("data-hf-repo"));
        const q = repo?.quants.find((x) => x.file === quantBtn.getAttribute("data-hf-quant"));
        if (repo && q) void applyPick(repo, q);
        return;
      }
      const repoBtn = t.closest("[data-hf-repo]") as HTMLElement | null;
      if (repoBtn) {
        const rid = repoBtn.getAttribute("data-hf-repo");
        expandedRepo = expandedRepo === rid ? null : rid;
        renderPickList();
      }
    });
    /* Type-ahead on the name field: repo-name suggestions
     * from the same shallow search the lookup runs (P2-D2
     * depth), one search per 300 ms debounce window.
     * Selecting a suggestion — pointer or keyboard — runs
     * the full lookup for that repo. No results or a failed
     * search simply show no dropdown; typing is never
     * interrupted. */
    let suggestTimer: number | null = null;
    let suggestSeq = 0;
    let suggestActive = -1;
    let suggestRepos: { repo_id: string; downloads: number }[] = [];
    const hideSuggest = (): void => {
      if (suggestHost) suggestHost.style.display = "none";
      suggestActive = -1;
    };
    const renderSuggest = (): void => {
      if (!suggestHost) return;
      if (!suggestRepos.length) { hideSuggest(); return; }
      suggestHost.innerHTML = suggestRepos.map((r, i) => `<div data-hf-suggest="${esc(r.repo_id)}" style="padding:6px 8px;font-size:12px;cursor:pointer;${i === suggestActive ? "background:var(--border)" : ""}"><span style="font-family:var(--mono)">${esc(r.repo_id)}</span> <span style="color:var(--text2)">· Downloads: ${fmtCount(r.downloads)}</span></div>`).join("");
      suggestHost.style.display = "";
    };
    const chooseSuggest = (repoId: string): void => {
      if (nameInput) nameInput.value = repoId;
      hideSuggest();
      void runLookup(repoId, repoId);
    };
    nameInput?.addEventListener("input", () => {
      if (suggestTimer !== null) window.clearTimeout(suggestTimer);
      const q = nameInput.value.trim();
      if (q.length < 2 || !isTauri()) { suggestRepos = []; hideSuggest(); return; }
      suggestTimer = window.setTimeout(async () => {
        const seq = ++suggestSeq;
        const r = await modelHfSuggest(q);
        if (seq !== suggestSeq) return;
        if (r.error || !r.live) { suggestRepos = []; hideSuggest(); return; }
        suggestRepos = r.data.repos;
        suggestActive = -1;
        renderSuggest();
      }, 300);
    });
    nameInput?.addEventListener("keydown", (ev) => {
      if (!suggestRepos.length || (suggestHost && suggestHost.style.display === "none")) return;
      if (ev.key === "ArrowDown") {
        ev.preventDefault();
        suggestActive = Math.min(suggestActive + 1, suggestRepos.length - 1);
        renderSuggest();
      } else if (ev.key === "ArrowUp") {
        ev.preventDefault();
        suggestActive = Math.max(suggestActive - 1, 0);
        renderSuggest();
      } else if (ev.key === "Enter" && suggestActive >= 0) {
        ev.preventDefault();
        chooseSuggest(suggestRepos[suggestActive].repo_id);
      } else if (ev.key === "Escape") {
        hideSuggest();
      }
    });
    suggestHost?.addEventListener("mousedown", (ev) => {
      const row = (ev.target as HTMLElement).closest("[data-hf-suggest]") as HTMLElement | null;
      if (row) {
        ev.preventDefault();
        chooseSuggest(row.getAttribute("data-hf-suggest") ?? "");
      }
    });
    nameInput?.addEventListener("blur", () => { window.setTimeout(hideSuggest, 150); });
    const addBtn = addModelHost.querySelector("#addModelBtn") as HTMLButtonElement | null;
    addBtn?.addEventListener("click", async () => {
      const msg = addModelHost.querySelector("#addModelMsg") as HTMLElement | null;
      // The message row stays hidden until its first write.
      if (msg) msg.style.display = "";
      const val = (sel: string): string => (addModelHost.querySelector(sel) as HTMLInputElement | null)?.value.trim() ?? "";
      const id = val("#addModelId");
      const url = val("#addModelUrl");
      if (!isTauri()) { if (msg) msg.textContent = "Sample mode: adding models runs in the installed app."; return; }
      if (!id || !url) {
        if (msg) { msg.style.color = "var(--err)"; msg.textContent = "Model id and source URL are required."; }
        return;
      }
      const paramsRaw = val("#addModelParams");
      let paramsB: number | undefined;
      if (paramsRaw) {
        const n = Number(paramsRaw);
        if (!Number.isFinite(n) || n <= 0) {
          if (msg) { msg.style.color = "var(--err)"; msg.textContent = "Parameters (B) must be a positive number (the model's total parameter count, e.g. 80)."; }
          return;
        }
        paramsB = n;
      }
      // The MoE pair is read only when its checkbox is checked;
      // checked requires both fields (the declaration is atomic).
      const moeChecked = (addModelHost.querySelector("#addModelMoe") as HTMLInputElement | null)?.checked ?? false;
      let moeLayers: number | undefined;
      let moeDefault: number | undefined;
      if (moeChecked) {
        const moeLayersRaw = val("#addModelMoeLayers");
        const moeDefRaw = val("#addModelMoeDefault");
        const l = Number(moeLayersRaw);
        const d = Number(moeDefRaw);
        if (!moeLayersRaw || !moeDefRaw || !Number.isInteger(l) || !Number.isInteger(d) || l < 1 || d < 0 || d > l) {
          if (msg) { msg.style.color = "var(--err)"; msg.textContent = "MoE is checked: give both fields — MoE layers of 1 or more and a default offload between 0 and the layer count."; }
          return;
        }
        moeLayers = l;
        moeDefault = d;
      }
      const mtpChecked = (addModelHost.querySelector("#addModelMtp") as HTMLInputElement | null)?.checked ?? false;
      // A pick's hash pins the add only while the form still
      // names the picked file at the picked URL (P2-D6): an
      // operator who redirected the form at another model
      // must not inherit the pick's pin.
      const fileVal = val("#addModelFile");
      let pickSha: string | undefined;
      if (hfPick && hfPick.sha256 && url === hfPick.url && (!fileVal || fileVal === hfPick.fileName)) {
        pickSha = hfPick.sha256;
      }
      addBtn.disabled = true;
      const r = await modelAdd(id, url, fileVal || undefined, val("#addModelQuant") || undefined, paramsB, moeLayers, moeDefault, mtpChecked ? true : undefined, pickSha);
      addBtn.disabled = false;
      if (r.error) {
        // Failure: the form stays open with the operator's input
        // intact and the error in the form's message row.
        if (msg) { msg.style.color = "var(--err)"; msg.textContent = `Add failed: ${r.error}`; }
        return;
      }
      // Success: clear and hide the form (emptying the host drops
      // every field with it), then repaint so the new model shows.
      closeAddModelForm();
      await load();
    });
  };
  root.querySelector("#modelAdd")?.addEventListener("click", openAddModelForm);

  await load();

  return () => {
    disposed = true;
    if (pollTimer !== null) window.clearInterval(pollTimer);
    root.removeEventListener("click", onClick);
    root.removeEventListener("click", onOvGoto);
    root.removeEventListener("input", syncRenameDraft);
    root.removeEventListener("input", syncOffloadDraft);
    root.removeEventListener("change", syncOffloadChecks);
    root.removeEventListener("keydown", onRenameKey);
  };
}
/* ---------------- Gateway ---------------- */

/** Routing-rules card form state (routing-rules design §7), held
 *  in the wireGateway closure so the 5-second repaint preserves an
 *  open form. `editingId` null = adding a new rule; `optimalTokens`
 *  is the raw input text — empty means the fraction default. */
interface RuleFormState {
  open: boolean;
  editingId: string | null;
  target: string;
  optimalTokens: string;
  advisory: boolean;
}

/** The freshest gateway status: the shared store's snapshot when it
 *  has one (it polls fast during transitions and refreshes on the
 *  backend's lifecycle events), else the view's own load. */
function freshGatewayStatus(status: GatewayStatusData): GatewayStatusData {
  return serviceStore.get().gateway ?? status;
}

/** The gateway's effective lifecycle state and transition (the store's
 *  pending overlay applied: a Restart click reads "restarting…" at
 *  once, and a restart from any other surface reads the same). */
function gatewayLc(status: GatewayStatusData): { state: LifecycleState; transition: GatewayStatusData["transition"] } {
  const g = freshGatewayStatus(status);
  return {
    state: serviceStore.effective("gateway", g.state),
    transition: serviceStore.transitionOf("gateway", g.transition),
  };
}

/** The Gateway view's summary title: lifecycle dot + label (with the
 *  transition's origin and age), plus a "service" chip when the
 *  gateway runs as a Windows service. */
function gatewayLcTitleHtml(status: GatewayStatusData): string {
  const g = freshGatewayStatus(status);
  const { state, transition } = gatewayLc(status);
  const fail = state === "failed" && g.failure ? ` title="${esc(g.failure.reason)}"` : "";
  return `${lifecycleDot(state)} <span${fail}>Gateway \u2014 LiteLLM \u00b7 ${esc(lifecycleLabel(state, transition))}</span>`
    + (g.service_managed ? ` <span class="chip status-tag status-off" title="The gateway runs as a Windows service (SCM)">service</span>` : "")
    + (g.pid !== null && META[state].isLive ? ` <span style="font-size:11px;color:var(--text3);font-weight:400">pid ${g.pid}</span>` : "");
}

/** Start / Stop / Restart for the gateway, from the shared rule. */
function gatewayLcActionsHtml(status: GatewayStatusData): string {
  const { state } = gatewayLc(status);
  return renderLifecycleActions({ kind: "gateway" }, state, {}, {
    labels: { start: "Start gateway", stop: "Stop gateway", restart: "Restart gateway" },
  });
}

/** The app-managed PostgreSQL sub-row under the gateway title: its
 *  lifecycle chip, pid and port, a port-mismatch warning (the next
 *  gateway start restarts it on the configured port), and "Stop
 *  database" — enabled only while the gateway is stopped, since the
 *  gateway uses the database. PostgreSQL starts with the gateway. */
function gatewayPgRowHtml(status: GatewayStatusData): string {
  const pg = freshGatewayStatus(status).postgres;
  if (!pg) return "";
  if (!pg.installed && pg.state === "not_installed") {
    return `<div style="font-size:12px;color:var(--text2);margin:-4px 0 10px">PostgreSQL: not installed<span class="info-tip" title="The LiteLLM admin UI needs a database — run the setup wizard's Install PostgreSQL step.">${icon("info")}</span></div>`;
  }
  const ps = serviceStore.effective("postgres", pg.state);
  const pt = serviceStore.transitionOf("postgres", pg.transition);
  const gs = gatewayLc(status).state;
  const gatewayDown = gs === "stopped" || gs === "failed" || gs === "unknown" || gs === "not_installed";
  const parts = [
    pg.pid !== null && META[ps].isLive ? `pid ${pg.pid}` : "",
    pg.port !== null ? `port ${pg.port}` : "",
  ].filter(Boolean).join(" \u00b7 ");
  const mismatch = pg.port_mismatch
    ? ` <span style="color:var(--warn)" title="The running server listens on a different port than configured. The next gateway start restarts PostgreSQL on the configured port.">${icon("warn")} port differs from the configuration</span>`
    : "";
  const stopBtn = renderLifecycleActions({ kind: "postgres" }, ps, {
    blockedReason: gatewayDown ? undefined : "Stop the gateway first — it uses the database",
  }, { className: "btn", labels: { stop: "Stop database" } });
  return `<div style="display:flex;align-items:center;gap:8px;flex-wrap:wrap;font-size:12px;color:var(--text2);margin:-4px 0 10px">PostgreSQL ${lifecycleChip(ps, { transition: pt })}${parts ? ` <span>${esc(parts)}</span>` : ""}${mismatch}<span class="spacer" style="flex:1"></span>${stopBtn}</div>`;
}

function gatewayPaint(
  status: GatewayStatusData,
  cfg: GatewayConfigData,
  backends: BackendRow[],
  models: ModelRow[],
  yamlOpen: boolean,
  yamlText: string | null,
  yamlErr: string,
  backendsError: string | null,
  masterKey: string | null,
  keyRevealed: boolean,
  plan: ActivationPlanData | null,
  planUi: PlanUiState,
  routingTab: "flow" | "groups",
  orModelsOpen: boolean,
  openFragments: Set<string>,
  ruleForm: RuleFormState,
  confirmRuleDelete: Set<string>,
  ruleRawFlags: Map<string, string | null>,
  genTab: "yaml" | "routing",
  routingPyText: string | null,
): string {
  const port = status.port || cfg.port;
  const backendById = new Map(backends.map((b) => [b.id, b]));
  /** Is this member dropped at generation time, and why? A backend
   *  member is dropped when disabled; the openrouter member when its
   *  tier is disabled. A member naming neither (an unknown name)
   *  cannot be verified as excluded and is treated as active.
   *  Exclusion is a settled state, so it renders gray (principle
   *  11) with the reason in a tooltip — never amber. */
  const memberState = (name: string): { excluded: boolean; reason: string } => {
    if (name === "openrouter") {
      return cfg.openrouter && !cfg.openrouter.enabled
        ? { excluded: true, reason: "tier disabled" }
        : { excluded: false, reason: "" };
    }
    const b = backendById.get(name);
    return b && !b.enabled
      ? { excluded: true, reason: "backend disabled" }
      : { excluded: false, reason: "" };
  };
  const memberExcluded = (name: string): boolean => memberState(name).excluded;
  const groupByName = new Map(cfg.groups.map((g) => [g.name, g]));
  /** A fallback naming a group whose members are ALL excluded (or
   *  which has no members) produces no deployments — the generated
   *  config drops the fallback reference itself. */
  const fallbackState = (name: string): { excluded: boolean; reason: string } => {
    const g = groupByName.get(name);
    if (g && name !== "openrouter") {
      return g.members.length === 0 || g.members.every(memberExcluded)
        ? { excluded: true, reason: "no active members" }
        : { excluded: false, reason: "" };
    }
    return memberState(name);
  };

  // The exclusion chips above are computed from the backend list; when
  // that list failed to load, say so instead of silently rendering
  // members as included.
  const backendsNote = backendsError
    ? `<div class="banner" style="margin-bottom:12px">${icon("warn")} Backend list unavailable (${esc(backendsError)}) \u2014 exclusion states in this view may be incomplete.</div>`
    : "";

  const withWin = backends.filter((b) => b.requests_1h !== undefined);
  // The traffic block renders only when at least one backend has a
  // metrics window (no placeholder line otherwise). Requests are a
  // per-backend distribution, so they render as a stacked bar whose
  // segments are the reporting backends (the largest segment IS the
  // hottest backend — no separate tile); the totals live once, as
  // the rows' Label: Value. Totals sum reporting backends only, and
  // partial coverage is stated in the Requests row's info-tip —
  // never claimed as "all backends" (principle 12). Errors render
  // as their share of requests in a thin proportion bar.
  const gwStats = withWin.length
    ? (() => {
        const totalReq = withWin.reduce((a, b) => a + (b.requests_1h ?? 0), 0);
        const totalErr = withWin.reduce((a, b) => a + (b.errors_1h ?? 0), 0);
        // Throughput sums only backends whose live rate is
        // measured; the row is omitted when none is (unknown is
        // not zero).
        const tputRows = withWin.filter((b) => b.throughput_toks !== undefined);
        const totalToks = tputRows.length
          ? tputRows.reduce((a, b) => a + (b.throughput_toks ?? 0), 0)
          : undefined;
        const palette = [C.blue, C.purple, C.teal, C.amber, C.green];
        const segs = withWin.map((b, i) => ({
          pct: totalReq > 0 ? ((b.requests_1h ?? 0) / totalReq) * 100 : 0,
          color: palette[i % palette.length],
          label: `${b.id} — ${fmtCount(b.requests_1h ?? 0)} req (1h)`,
        }));
        const keys = withWin.map((b, i) => `<span><span class="sw" style="background:${palette[i % palette.length]}"></span>${esc(b.id)} ${fmtCount(b.requests_1h ?? 0)}</span>`).join("");
        const coverageTip = withWin.length < backends.length
          ? `<span class="info-tip" title="${withWin.length} of ${backends.length} backends reporting — totals cover reporting backends only">${icon("info")}</span>`
          : "";
        const errSegs = totalReq > 0 && totalErr > 0
          ? [{ pct: (totalErr / totalReq) * 100, color: C.red, label: `Errors: ${fmtCount(totalErr)} of ${fmtCount(totalReq)} requests (1h)` }]
          : [];
        return `<div style="margin-top:12px;border-top:1px solid var(--border);padding-top:10px">
        <div style="font-size:12px;color:var(--text2)">Requests (1h): <b style="color:var(--text)">${fmtCount(totalReq)}</b>${coverageTip}</div>
        <div style="margin-top:4px">${stackedBar(segs, 600, 10)}</div>
        <div class="legend" style="margin-top:4px">${keys}</div>
        <div style="font-size:12px;color:var(--text2);margin-top:8px">Errors (1h): <b style="color:var(--text)">${fmtCount(totalErr)}</b></div>
        <div style="margin-top:4px">${stackedBar(errSegs, 600, 4)}</div>
        ${totalToks !== undefined ? `<div class="kv" style="margin-top:8px"><span class="k">Throughput</span><span class="v" style="font-size:13px">${trim1(totalToks)} tok/s</span></div>` : ""}
      </div>`;
      })()
    : "";
  const summary = `<div class="card" style="margin-bottom:12px"><div class="sec-title" id="gwLcTitle">${gatewayLcTitleHtml(status)}</div>
    <div id="gwPgRow">${gatewayPgRowHtml(status)}</div>
    <div style="display:grid;grid-template-columns:repeat(5,1fr);gap:10px">
      ${([["Port", String(port)], ["Groups", fmtCount(status.groups), "Enabled groups — a group counts when at least one of its member backends is enabled; the gateway serves enabled groups only."], ["Routing rules", fmtCount(status.routing_rules)], ["Routing test traces", fmtCount(status.routing_log_len), "Routing test traces are dry-run decisions recorded from routing tests this session — live requests are not logged."], ["LiteLLM", status.litellm_version ?? "not measured", "The LiteLLM version the start-gateway step probed in the gateway venv at the last spawn (MCP design §4.3). Not measured until a spawn records one."]] as [string, string, string?][])

        .map(([k, v, tip]) => `<div class="kv"><span class="k">${k}${tip ? `<span class="info-tip" title="${esc(tip)}">${icon("info")}</span>` : ""}</span><span class="v" style="font-size:13px">${esc(v)}</span></div>`).join("")}
    </div>
    ${gwStats}</div>`;

  // Bind explanations ride an info-tip on the Network access row —
  // the LAN row itself renders only when a LAN URL exists.
  const bindTip = status.bind_host === "127.0.0.1"
    ? "Reachable from this machine only."
    : status.lan_ip
      ? null
      : "Bound to all interfaces; this machine's LAN address could not be determined.";
  const masterKeyTip = `Clients authenticate with the master key — LiteLLM requires one. The VS Code LiteLLM extension takes the base URL without /v1 (http://127.0.0.1:${port}); it appends /v1 itself.${masterKey === null ? " Reveal loads it." : ""}`;
  // The gateway lifecycle controls answer "how do I control the
  // gateway", not the endpoint card's question ("what do clients
  // connect to") — the row renders directly under the summary card
  // instead (client-endpoint audit Q7). Ids and handlers unchanged.
  const gwActions = `<div class="card-actions" style="margin:0 0 12px">
      <span id="gwLcActions" style="display:contents">${gatewayLcActionsHtml(status)}</span><span class="info-tip" title="Start runs the Setup Wizard's start-gateway step: LiteLLM binds the configured address, and the step reconciles the Windows firewall rule with it (one elevation prompt when the rule state changes). When the OpenRouter tier is enabled, the fallback is armed from Windows Credential Manager. Restart replaces the running gateway in one server-side operation (PostgreSQL keeps running unless its port changed); Stop stops the gateway only.">${icon("info")}</span>
      <a class="btn" href="#/wizard">${icon("wrench")} Setup Wizard</a>
      <button class="btn icon-only" id="gwAdminUiBtn" title="LiteLLM admin UI" aria-label="LiteLLM admin UI">${icon("gateway")}</button><span class="info-tip" title="Sign in as admin with the master key as the password. The admin UI requires a database — run the setup wizard's Install PostgreSQL step if sign-in reports &quot;Not connected to DB&quot;.">${icon("info")}</span>
      <a class="btn icon-only" href="#/logs?scope=gateway&follow=1" title="Logs" aria-label="Gateway logs">${icon("logs")}</a>
    </div>`;
  const endpoint = `<div class="card" style="margin-bottom:12px"><div class="sec-title">${icon("gateway")} Client endpoint</div>
    <div style="font-size:12px;color:var(--text2);display:grid;gap:4px">
      <div>Base URL: <span style="font-family:var(--mono);color:var(--text)">http://127.0.0.1:${port}/v1</span>
        <button class="btn icon-only" id="gwBaseUrlCopyBtn" title="Copy base URL" aria-label="Copy base URL">${icon("copy")}</button></div>
      <div>Port:<span class="info-tip" title="The gateway's listen port, saved to the configuration through update_gateway_config and validated against every other claimed port. Applies when the gateway next starts; the activation plan tracks the restart.">${icon("info")}</span> <input id="gwPortInput" type="number" min="1" max="65535" value="${port}" style="width:90px;font-size:12px">
        <button class="btn" id="gwPortApply" style="padding:1px 7px;font-size:11px">Apply</button></div>
      <div>Network access:${bindTip ? `<span class="info-tip" title="${esc(bindTip)}">${icon("info")}</span>` : ""} <select id="gwBindSelect" class="input" style="width:auto;display:inline-block;padding:2px 6px">
          <option value="0.0.0.0"${status.bind_host !== "127.0.0.1" ? " selected" : ""}>Local network (all interfaces)</option>
          <option value="127.0.0.1"${status.bind_host === "127.0.0.1" ? " selected" : ""}>This computer only (127.0.0.1)</option>
        </select></div>
      ${status.bind_host !== "127.0.0.1" && status.lan_ip
        ? `<div>LAN base URL: <span style="font-family:var(--mono);color:var(--text)">http://${esc(status.lan_ip)}:${port}/v1</span><span class="info-tip" title="Other machines use this address with the master key. The VS Code LiteLLM extension takes it without /v1.">${icon("info")}</span>
        <button class="btn icon-only" id="gwLanUrlCopyBtn" title="Copy LAN base URL" aria-label="Copy LAN base URL">${icon("copy")}</button></div>`
        : ""}
      <div>Master key:<span class="info-tip" title="${esc(masterKeyTip)}">${icon("info")}</span> <span style="font-family:var(--mono);color:var(--text)">${masterKey === null ? "not loaded" : keyRevealed ? esc(masterKey) : "\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022"}</span>
        <button class="btn icon-only" id="gwKeyRevealBtn" title="${keyRevealed ? "Hide master key" : "Reveal master key"}" aria-label="${keyRevealed ? "Hide master key" : "Reveal master key"}">${icon(keyRevealed ? "eyeOff" : "eye")}</button>
        <button class="btn icon-only" id="gwKeyCopyBtn" title="Copy master key" aria-label="Copy master key">${icon("copy")}</button></div>
    </div></div>`;

  const or = cfg.openrouter;
  // The tier card is omitted entirely when the tier is not
  // configured (principle 13). State renders as header chips, the
  // credential's probed presence as a chip beside its label
  // (present / not found / not checked — the probe's null is an
  // explicit unknown, never prose), and every explanation rides a
  // tooltip verbatim. The reference is an address (of the secret in
  // Windows Credential Manager), so it carries a Copy button.
  const openrouter = or === null ? "" : (() => {
    const present = status.openrouter_cred_present;
    const credChip = or.cred_ref
      ? (present === true
        ? `<span class="chip status-tag status-on">present</span>`
        : present === false
          ? `<span class="chip status-tag status-bad" title="The fallback cannot authenticate.">not found</span>`
          : `<span class="chip">not checked</span>`)
      : or.enabled
        ? `<span class="chip status-tag status-bad" title="The fallback cannot authenticate until one is configured.">no credential reference</span>`
        : "";
    const credStatusRow = or.cred_ref || or.enabled
      ? `<div style="display:flex;justify-content:space-between;gap:16px"><span class="k">Credential status</span><span>${credChip}</span></div>`
      : "";
    const allowRow = `<div style="display:flex;justify-content:space-between;gap:16px"><span class="k">Model allowlist</span><span class="v" style="font-size:12px;text-align:right">${or.model_allowlist.length ? `${or.model_allowlist.length} model${or.model_allowlist.length === 1 ? "" : "s"}` : "none"} <button class="btn" data-or-models aria-expanded="${orModelsOpen}" title="Model allowlist" aria-label="Model allowlist" style="padding:1px 7px;font-size:11px">${icon(orModelsOpen ? "chevD" : "chevR")}</button></span></div>
      ${orModelsOpen ? `${or.model_allowlist.length ? `<div style="display:flex;gap:6px;flex-wrap:wrap;margin-top:4px">${or.model_allowlist.map((m) => `<span class="chip">${esc(m)}</span>`).join("")}</div>` : ""}
      <div style="margin-top:6px"><label style="font-size:12px;color:var(--text2)">Models, one per line
        <textarea id="gwOrAllowInput" rows="3" style="display:block;width:100%;font-size:12px;margin-top:4px">${esc(or.model_allowlist.join("\n"))}</textarea></label>
      <div class="card-actions" style="margin-top:6px"><button class="btn" id="gwOrAllowSave" style="padding:1px 7px;font-size:11px">Save allowlist</button></div></div>` : ""}`;
    // Spent-vs-cap (openrouter-tier audit's designated slot, now that
    // gateway_status carries the spend ledger): "Spent today: $X"
    // with a thin meter whose fill is spent / cap — the cap itself
    // lives once, in the Daily cap field above. Light red once the
    // cap is reached (no further OpenRouter traffic). Omitted when
    // the payload carries no spend or the cap is zero (no share).
    const spend = status.openrouter_spend ?? null;
    const spendRow = spend && spend.fraction !== null && spend.cap_usd > 0
      ? `<div data-or-spend><div style="display:flex;justify-content:space-between;gap:16px"><span class="k">Spent today<span class="info-tip" title="${esc(`The app's spend ledger for OpenRouter traffic today; the bar fills toward the daily cap ($${trim1(spend.cap_usd)}). The cap itself is enforced inside the gateway (LiteLLM provider budget, 1-day window).`)}">${icon("info")}</span></span><span class="v" style="font-size:12px">$${trim1(spend.spent_today_usd)}${spend.capped ? ` <span class="chip status-tag status-bad" title="The daily cap is reached: no OpenRouter traffic is sent until the window resets.">cap reached</span>` : ""}</span></div>
      <div style="margin-top:4px" title="${esc(`Spent: ${trim1(Math.min(1, spend.fraction) * 100)}% of the daily cap`)}">${meter(Math.min(100, spend.fraction * 100), 300, 6, spend.capped ? C.red : C.blue)}</div></div>`
      : "";
    return `<div class="card" style="margin-bottom:12px${or.enabled ? "" : ";opacity:.65"}"><div class="sec-title">${icon("gateway")} OpenRouter fallback tier
      <span class="spacer" style="flex:1"></span>
      ${or.enabled
        ? `<span class="chip status-tag status-on">enabled</span>`
        : `<span class="chip status-tag status-off" title="The tier is disabled — fallback entries naming openrouter are dropped when the gateway config is generated, and no traffic is sent to OpenRouter.">disabled</span>`}
    </div>
    <div class="kv" style="gap:10px">
      <div style="display:flex;justify-content:space-between;gap:16px;align-items:center"><span class="k">Credential<span class="info-tip" title="The Credential Manager target holding the OpenRouter key. When the gateway starts, the key is read from Windows Credential Manager (this reference) and passed to LiteLLM as OPENROUTER_API_KEY. The key itself is never stored in the configuration. Clear the field and save to remove the reference.">${icon("info")}</span></span><span style="display:flex;gap:6px;align-items:center"><input id="gwOrCredInput" type="text" value="${esc(or.cred_ref ?? "")}" placeholder="Credential Manager target" style="width:220px;font-size:12px"><button class="btn" id="gwOrCredSave" style="padding:1px 7px;font-size:11px">Save</button>${or.cred_ref ? `<button class="btn" id="gwOrCredCopyBtn" title="Copy credential reference" aria-label="Copy credential reference" style="padding:1px 7px;font-size:11px">${icon("copy")}</button>` : ""}</span></div>
      ${credStatusRow}
      <div style="display:flex;justify-content:space-between;gap:16px;align-items:center"><span class="k">Daily cap<span class="info-tip" title="Spending cap per day.">${icon("info")}</span></span><span style="display:flex;gap:6px;align-items:center"><span style="font-size:12px">$</span><input id="gwOrCapInput" type="number" min="0" step="0.5" value="${or.daily_cap_usd}" style="width:90px;font-size:12px"><button class="btn" id="gwOrCapSave" style="padding:1px 7px;font-size:11px">Save</button></span></div>
      ${spendRow}
      ${allowRow}
    </div>
    <div class="card-actions">${enableSwitch({ on: or.enabled, name: "OpenRouter fallback tier", attrs: `id="gwOrToggle"` })}</div>
  </div>`;
  })();

  // Computed by the backend (get_gateway_config.effective_strategy)
  // with the config generator's own rule; never re-derived here.
  const effStrategy = cfg.effective_strategy;

  /* ---- Routing section: one section, two views (Flow | Groups).
   * The section top carries the one answer-level fact — the
   * effective global strategy as Label: Value, its methodology in
   * the label's info-tip. ---- */
  const flowGroupHtml = (g: GatewayGroup): string => {
    // Member traffic shares: real only when every member is a
    // backend with a metrics window and the group total is
    // positive. Otherwise the bar encodes configured membership at
    // equal widths, and its info-tip states that basis — equal
    // width never invents traffic.
    const reqs = g.members.map((name) => backendById.get(name)?.requests_1h);
    const totalReq = reqs.reduce<number>((a, r) => a + (r ?? 0), 0);
    const traffic = g.members.length > 0 && reqs.every((r) => r !== undefined) && totalReq > 0;
    const palette = [C.blue, C.purple, C.teal, C.amber, C.green];
    const segs = g.members.map((name, i) => ({
      pct: traffic ? ((reqs[i] ?? 0) / totalReq) * 100 : 100 / g.members.length,
      color: traffic ? palette[i % palette.length] : "#3a4350",
      label: traffic ? `${name} — ${fmtCount(reqs[i] ?? 0)} req (1h)` : name,
    }));
    const keys = g.members.map((name) => {
      const st = memberState(name);
      return st.excluded
        ? `<span style="color:var(--text3)">${esc(name)} <span class="chip status-tag status-off" title="${esc(st.reason)}">excluded</span></span>`
        : `<span>${esc(name)}</span>`;
    }).join(" · ");
    const fallbackLines = g.fallbacks.map((name) => {
      const st = fallbackState(name);
      // An excluded fallback target gets NO connector — a connector
      // must never assert a path the generated config drops.
      return st.excluded
        ? `<div style="font-size:12px;margin-top:2px"><span class="chip status-tag status-off" title="${esc(st.reason)}">${esc(name)}</span></div>`
        : `<div style="font-size:12px;margin-top:2px"><span style="color:var(--text3)">fallback - - \u2192</span> ${esc(name)}</div>`;
    }).join("");
    const sub = g.members.length
      ? `Members enabled: ${g.enabled_members} of ${g.members.length}`
      : `<span title="Omitted from the generated gateway config.">Members: 0</span>`;
    return `<div style="display:flex;gap:10px;align-items:flex-start;padding:8px 0;border-bottom:1px solid var(--border)${g.enabled ? "" : ";opacity:.65"}">
      <div style="flex:0 0 170px;border:1px solid var(--border);border-radius:8px;padding:8px 10px;background:var(--panel2)">
        <div style="font-size:12px;font-weight:600">${esc(g.name)}${g.enabled ? "" : ` <span class="chip status-tag status-off">disabled</span>`}</div>
        <div style="font-size:11px;color:var(--text2);margin-top:2px">${sub}</div>
      </div>
      <span style="color:var(--text3);padding-top:10px">\u2192</span>
      <div style="flex:1;min-width:0">
        ${g.members.length ? `${stackedBar(segs, 600, 10)}${traffic ? "" : `<span class="info-tip" title="Configured membership — no metrics window yet.">${icon("info")}</span>`}
        <div style="font-size:12px;margin-top:4px">${keys}</div>` : ""}
        ${fallbackLines}
      </div>
    </div>`;
  };

  const groupCardHtml = (g: GatewayGroup): string => {
    const m = g.members.length;
    const enabledCount = g.members.filter((name) => !memberExcluded(name)).length;
    // One segment per member, equal width — segments encode
    // membership and state, not a share. Green reaches the
    // generated config; gray is excluded. The fraction lives only
    // in the bar's tooltip; member names are the bar's keys, each
    // in its segment's color.
    const bar = m
      ? `<div style="margin-top:10px"><span title="Enabled: ${enabledCount} of ${m}" style="display:block">${stackedBar(g.members.map((name) => ({
          pct: 100 / m,
          color: memberExcluded(name) ? "#6d7686" : C.green,
          label: name,
        })), 300, 10)}</span>
        <div style="font-size:12px;margin-top:4px;display:flex;gap:10px;flex-wrap:wrap">${g.members.map((name) => {
          const ex = memberExcluded(name);
          return `<span style="color:${ex ? "var(--text3)" : C.green}"${ex ? ` title="Disabled — dropped from the generated gateway config"` : ""}>${esc(name)}</span>`;
        }).join("")}</div></div>`
      : "";
    const fallbacks = g.fallbacks.length
      ? `<div style="font-size:12px;margin-top:8px"><span style="color:var(--text2)">Fallbacks:</span> ${g.fallbacks.map((name) => {
          const st = fallbackState(name);
          const tip = st.reason === "no active members"
            ? "No active members — dropped from the generated gateway config"
            : "Disabled — dropped from the generated gateway config";
          return `<span style="color:${st.excluded ? "var(--text3)" : C.green}"${st.excluded ? ` title="${tip}"` : ""}>${esc(name)}</span>`;
        }).join(" · ")}</div>`
      : "";
    return `<div class="card"${g.enabled ? "" : ` style="opacity:.65"`}>
      <h3>${esc(g.name)}${g.enabled ? "" : ` <span class="chip status-tag status-off" title="No enabled backends — omitted from the generated gateway config">disabled</span>`}
        <span class="spacer" style="flex:1"></span><span class="chip">${esc(g.strategy)}</span></h3>
      ${bar}
      ${fallbacks}
      <div style="display:flex;gap:6px;align-items:center;margin-top:8px;flex-wrap:wrap">
        <label style="font-size:12px;color:var(--text2)">Fallbacks <input type="text" data-group-fallbacks-input="${esc(g.name)}" value="${esc(g.fallbacks.join(", "))}" style="width:240px;font-size:12px"></label>
        <button class="btn" data-group-fallbacks-save="${esc(g.name)}" style="padding:1px 7px;font-size:11px">Save</button>
        <span class="info-tip" title="The ordered fallback chain for this group: backend ids, or openrouter for the cloud tier, comma-separated. Saved through update_gateway_config and validated — an unknown name is refused. Applies when the gateway next starts.">${icon("info")}</span>
      </div>
    </div>`;
  };

  const routing = cfg.groups.length
    ? `<div class="card" style="margin-bottom:12px"><div class="sec-title">${icon("gateway")} Routing<span class="spacer" style="flex:1"></span><a class="btn" href="#/groups">${icon("pencil")} Edit membership</a></div>
      ${effStrategy ? `<div style="font-size:12px;color:var(--text2)">Effective strategy: <b style="color:var(--text)">${esc(effStrategy)}</b><span class="info-tip" title="LiteLLM applies ONE global routing strategy — the strategy chips on the groups are labels, not per-group settings.">${icon("info")}</span></div>` : ""}
      <div class="viewsel" role="tablist" style="margin-top:10px">
        <button data-routing-tab="flow"${routingTab === "flow" ? ` class="active"` : ""}>Flow</button>
        <button data-routing-tab="groups"${routingTab === "groups" ? ` class="active"` : ""}>Groups</button>
      </div>
      ${routingTab === "flow"
        ? `<div>${cfg.groups.map(flowGroupHtml).join("")}</div>`
        : `<div class="grid2" style="margin-top:12px">${cfg.groups.map(groupCardHtml).join("")}</div>`}
    </div>`
    : emptyHtml("No model groups are configured for the gateway.");

  /* ---- Routing rules (routing-rules design §7): the management
   * surface for typed rules. The header renders always — it is the
   * creation surface, not a data block; the body renders when rules
   * exist or the form is open. Typed rules render as Label: Value
   * rows with enable/disable (icon-only), edit (icon-only), and
   * Delete (words + icon, destructive — principle 15); legacy
   * fragment-only rules stay read-only (design §6.1) with their
   * fragment behind a per-row expandable. The source composition
   * bar survives from the read-only card for mixed rule lists. ---- */
  /** A group's hard window, mirroring manager-gateway's baking:
   *  the minimum over its enabled member backends of
   *  min(configured context, the model's rated context). Null when
   *  no enabled backend member bounds the group. */
  const groupHardWindow = (g: GatewayGroup): number | null => {
    let hard: number | null = null;
    for (const member of g.members) {
      const b = backendById.get(member);
      if (!b || !b.enabled || b.n_ctx === undefined) continue;
      const mdl = models.find((m) => sameModelFile(b.model_file, m.gguf_path));
      const cap = mdl?.ctx_max ? mdl.ctx_max : null;
      const w = cap !== null ? Math.min(b.n_ctx, cap) : b.n_ctx;
      hard = hard === null ? w : Math.min(hard, w);
    }
    return hard;
  };
  /** The §6.2 warnings for a rule draft targeting `target`,
   *  computed from the same data the generator bakes from. */
  const ruleWarnings = (target: string): string[] => {
    const warnings: string[] = [];
    const tg = cfg.groups.find((g) => g.name === target);
    if (!tg) return warnings;
    if (tg.enabled_members === 0) {
      warnings.push(`The target group ${target} has no enabled members — rerouted requests will fail until a member backend is enabled.`);
    }
    const targetHard = groupHardWindow(tg);
    if (targetHard !== null) {
      for (const g of cfg.groups) {
        if (g.name === target) continue;
        const h = groupHardWindow(g);
        if (h !== null && targetHard <= h) {
          warnings.push(`The target's window (${fmtCount(targetHard)} tokens) is not larger than ${g.name}'s (${fmtCount(h)} tokens) — the rule cannot help ${g.name}.`);
        }
      }
    }
    for (const g of cfg.groups) {
      for (const member of g.members) {
        const raw = ruleRawFlags.get(member);
        if (!raw) continue;
        const toks = raw.split(/\s+/);
        for (let i = 0; i + 1 < toks.length; i++) {
          if ((toks[i] === "--parallel" || toks[i] === "-np") && Number(toks[i + 1]) > 1) {
            warnings.push(`${member} runs with ${toks[i]} ${toks[i + 1]} — its per-slot context is smaller than the window the rule assumes, so rerouting can trigger later than the rule intends.`);
          }
        }
      }
    }
    return warnings;
  };
  const ruleOptimalText = (r: GatewayRule): string => {
    const k = r.kind as GatewayRuleKind;
    if (k.optimal_tokens !== null) return `${fmtCount(k.optimal_tokens)} tokens`;
    const tg = cfg.groups.find((g) => g.name === k.target_group);
    const hard = tg ? groupHardWindow(tg) : null;
    const pct = `${Math.round(k.optimal_fraction * 100)}%`;
    return hard !== null
      ? `${fmtCount(Math.floor(k.optimal_fraction * hard))} tokens (${pct} of the target window)`
      : `${pct} of the serving window`;
  };
  const typedRuleRow = (r: GatewayRule): string => {
    const k = r.kind as GatewayRuleKind;
    return `<div style="display:flex;gap:14px;align-items:center;padding:6px 0;border-bottom:1px solid var(--border);flex-wrap:wrap">
      <span style="font-size:12px">Type: <b>Context overflow</b></span>
      <span style="font-size:12px">Target: <b>${esc(k.target_group)}</b></span>
      <span style="font-size:12px">Optimal: <b>${esc(ruleOptimalText(r))}</b></span>
      <span style="font-size:12px">Advisory: <b>${k.advisory ? "On" : "Off"}</b></span>
      ${r.description ? `<span class="info-tip" title="${esc(r.description)}">${icon("info")}</span>` : ""}
      ${r.enabled ? "" : `<span class="chip status-tag status-off">Disabled</span>`}
      <span class="spacer" style="flex:1"></span>
      ${enableSwitch({ on: r.enabled, name: `Routing rule ${r.id}`, attrs: `data-rule-toggle="${esc(r.id)}"` })}
      <button class="btn icon-only" data-rule-edit="${esc(r.id)}" title="Edit rule" aria-label="Edit rule ${esc(r.id)}">${icon("pencil")}</button>
      <button class="btn danger${confirmRuleDelete.has(r.id) ? " armed" : ""}" data-rule-delete="${esc(r.id)}">${icon("trash")} ${confirmRuleDelete.has(r.id) ? "Confirm delete" : "Delete"}</button>
    </div>`;
  };
  const legacyRuleRow = (r: GatewayRule): string => {
    const open = openFragments.has(r.id);
    return `<div style="display:flex;gap:8px;align-items:center;padding:6px 0;border-bottom:1px solid var(--border)">
      <span style="font-size:12px">Rule: <span style="font-family:var(--mono)">${esc(r.id)}</span></span>
      <span class="info-tip" title="${esc(r.description)}">${icon("info")}</span>
      <span class="spacer" style="flex:1"></span>
      ${r.litellm_fragment ? `<button class="btn" data-fragment="${esc(r.id)}" aria-expanded="${open}" title="Fragment" aria-label="Fragment" style="padding:2px 8px;font-size:11px">${icon(open ? "chevD" : "chevR")}</button>` : ""}
      <span class="chip">${esc(r.source)}</span></div>
      ${open && r.litellm_fragment ? `<pre style="margin:6px 0;padding:10px 12px;background:var(--panel2);border:1px solid var(--border);border-radius:8px;font-family:var(--mono);font-size:11px;white-space:pre-wrap;word-break:break-word">${esc(r.litellm_fragment)}</pre>` : ""}`;
  };
  const ruleFormHtml = (): string => {
    if (!ruleForm.open) return "";
    const warnings = ruleWarnings(ruleForm.target);
    const targetOptions = cfg.groups
      .map((g) => `<option value="${esc(g.name)}"${g.name === ruleForm.target ? " selected" : ""}>${esc(g.name)}</option>`)
      .join("");
    return `<div style="margin-top:10px;border-top:1px solid var(--border);padding-top:10px">
      <div style="display:flex;gap:16px;align-items:flex-end;flex-wrap:wrap">
        <span style="font-size:12px">Type: <b>Context overflow</b></span>
        <label style="font-size:12px">Target group
          <select id="gwRuleTarget" class="input" style="display:block;margin-top:4px">${targetOptions}</select></label>
        <label style="font-size:12px">Optimal tokens<span class="info-tip" title="Past this conversation size the serving model adds the compaction note. Empty = the default: half the serving model's window.">${icon("info")}</span>
          <input id="gwRuleOptimal" class="input" type="number" min="1" step="1" placeholder="Default (half the window)" value="${esc(ruleForm.optimalTokens)}" style="display:block;margin-top:4px"></label>
        <label style="font-size:12px;display:flex;gap:6px;align-items:center;padding-bottom:6px"><input type="checkbox" id="gwRuleAdvisory"${ruleForm.advisory ? " checked" : ""}> Advisory<span class="info-tip" title="When on, the serving model appends the compaction note past the optimal window. Off = reroute silently.">${icon("info")}</span></label>
      </div>
      ${warnings.map((w) => `<div style="color:var(--amber);font-size:12px;margin-top:6px">${icon("warn")} ${esc(w)}</div>`).join("")}
      <div class="card-actions" style="margin-top:10px">
        <button class="btn" id="gwRuleSaveBtn">${icon("check")} Save rule</button>
        <button class="btn" id="gwRuleCancelBtn">Cancel</button>
      </div>
    </div>`;
  };
  const rules = (() => {
    const bySource = new Map<string, number>();
    for (const r of cfg.routing_rules) bySource.set(r.source, (bySource.get(r.source) ?? 0) + 1);
    const tones = ["#6d7686", "#3a4350", "#a7b0bf", "#545e6e"];
    const showBar = cfg.routing_rules.length >= 2 && bySource.size >= 2;
    const bar = showBar
      ? `<div style="margin-bottom:6px">${stackedBar([...bySource.entries()].map(([src, cnt], i) => ({
          pct: (cnt / cfg.routing_rules.length) * 100,
          color: tones[i % tones.length],
          label: `${src}: ${cnt}`,
        })), 600, 8)}
        <div class="legend" style="margin-top:4px">${[...bySource.entries()].map(([src], i) => `<span><span class="sw" style="background:${tones[i % tones.length]}"></span>${esc(src)}</span>`).join("")}</div></div>`
      : "";
    const rows = cfg.routing_rules.map((r) => (r.kind ? typedRuleRow(r) : legacyRuleRow(r))).join("");
    return `<div class="card" style="margin-top:12px"><div class="sec-title">${icon("gateway")} Routing rules<span class="info-tip" title="A context-overflow rule reroutes a request too large for its model group to the target group instead of failing it, and past the optimal window the serving model adds a one-line note advising a compacted or fresh conversation. Changes take effect when the gateway restarts — save, then apply the Activation Plan.">${icon("info")}</span>
      <span class="spacer" style="flex:1"></span>
      <button class="btn" id="gwRuleAddBtn">${icon("plus")} Add rule</button></div>
      ${bar}${rows}${ruleFormHtml()}</div>`;
  })();

  /* ---- Generated config: one section, two views (design §7) —
   * config.yaml, plus the llmdil_routing.py callback as a second
   * tab when a typed routing rule is enabled. One collapsed row
   * (title, summary values once fetched, Copy / Download / toggle
   * acting on the active tab); the methodology sentences ride
   * info-tips verbatim; a fetch in flight renders "Loading…" in
   * the pre region, never an empty pre; a load failure is a header
   * value with the raw error in its tooltip. ---- */
  const hasRoutingCallback = cfg.routing_rules.some((r) => r.kind !== null && r.enabled);
  const activeGenText = genTab === "routing" ? routingPyText : yamlText;
  const genSummary = activeGenText !== null && !yamlErr
    ? (() => {
        const lines = activeGenText.replace(/\n$/, "").split("\n").length;
        const models = genTab === "yaml" ? (activeGenText.match(/model_name:/g) ?? []).length : 0;
        return `<span style="font-size:12px;color:var(--text2)">Lines: <b style="color:var(--text)">${lines}</b></span>
        ${genTab === "yaml" ? `<span style="font-size:12px;color:var(--text2)">Models: <b style="color:var(--text)">${models}</b></span>` : ""}
        <span style="font-size:12px;color:var(--text2)">Source: current configuration<span class="info-tip" title="The running gateway reads the config file written when it was last (re)started from the Setup Wizard. This preview is regenerated from the current configuration — after edits, the two can differ until the gateway is restarted.">${icon("info")}</span></span>`;
      })()
    : "";
  const genTabs = hasRoutingCallback
    ? `<div class="viewsel" role="tablist" style="margin-top:10px">
        <button data-gen-tab="yaml"${genTab === "yaml" ? ` class="active"` : ""}>config.yaml</button>
        <button data-gen-tab="routing"${genTab === "routing" ? ` class="active"` : ""}>llmdil_routing.py</button>
      </div>`
    : "";
  const yaml = `<div class="card" style="margin-top:12px">
    <div style="display:flex;gap:10px;align-items:center;flex-wrap:wrap">
      <span class="sec-title" style="margin:0">Generated config (preview)</span>
      ${yamlErr ? `<span style="font-size:12px;color:var(--err)" title="${esc(yamlErr)}">Preview: failed to load</span>` : genSummary}
      <span class="spacer" style="flex:1"></span>
      <button class="btn icon-only" id="gwYamlCopyBtn" title="Copy generated config" aria-label="Copy generated config">${icon("copy")}</button>
      <button class="btn icon-only" id="gwYamlDownloadBtn" title="Download ${genTab === "routing" ? "llmdil_routing.py" : "config.yaml"}" aria-label="Download ${genTab === "routing" ? "llmdil_routing.py" : "config.yaml"}">${icon("download")}</button>
      <button class="btn icon-only" id="gwYamlBtn" title="${yamlOpen ? "Hide preview" : "Preview generated config"}" aria-label="${yamlOpen ? "Hide preview" : "Preview generated config"}">${icon(yamlOpen ? "eyeOff" : "eye")}</button>
    </div>
    ${genTabs}
    ${yamlOpen && !yamlErr ? (activeGenText === null
      ? `<pre style="margin-top:10px;padding:12px;background:var(--panel2);border:1px solid var(--border);border-radius:8px;font-family:var(--mono);font-size:11px">Loading\u2026</pre>`
      : `<div style="display:flex;gap:8px;align-items:center;margin-top:10px"><span style="font-size:11px;color:var(--text3);font-family:var(--mono)">${genTab === "routing" ? "llmdil_routing.py" : "config.yaml"}</span>${genTab === "yaml" ? `<span class="info-tip" title="\`api_key: local\` in the generated config is LiteLLM's required non-empty placeholder — it is not a real key.">${icon("info")}</span>` : ""}</div>
         <pre style="margin-top:6px;padding:12px;background:var(--panel2);border:1px solid var(--border);border-radius:8px;font-family:var(--mono);font-size:11px;white-space:pre-wrap;word-break:break-word;max-height:420px;overflow:auto">${esc(activeGenText)}</pre>`) : ""}
  </div>`;

  return activationPlanHtml(plan, planUi, "summary") + backendsNote + summary + gwActions + endpoint + openrouter + routing + rules + yaml;
}

export function renderGateway(): string {
  return `${topbar("Gateway")}
  <div class="view"><div id="gwBanner"></div>
  <div id="gwMsg" style="font-size:12px;margin-bottom:8px"></div>
  <div id="gwBody">${loadingHtml("Loading gateway configuration\u2026")}</div></div>`;
}

export async function wireGateway(root: HTMLElement): Promise<() => void> {
  let yamlOpen = false;
  let yamlText: string | null = null;
  let yamlErr = "";
  let disposed = false;
  /** The gateway status from the latest load: the lifecycle title,
   *  actions and PostgreSQL row repaint in place from it (merged
   *  with the shared store's fresher snapshot) without a full body
   *  repaint. */
  let lastStatus: GatewayStatusData | null = null;
  /** Last loaded config (the toggle handler patches against it). */
  let lastCfg: GatewayConfigData | null = null;
  /** Gateway port from the latest load (the admin UI link target). */
  let lastPort = 4000;
  /** LAN address from the latest load (the LAN base URL copy target). */
  let lastLanIp: string | null = null;
  /** Master key, fetched on first Reveal/Copy; kept in the closure
   *  so repaints do not re-read the credential store. */
  let masterKey: string | null = null;
  let keyRevealed = false;
  /** Gateway activation plan, painted at the top of the view. */
  let lastPlan: ActivationPlanData | null = null;
  const planUi = newPlanUi();
  /** Routing section tab (Flow | Groups), the OpenRouter allowlist
   * expandable, and the routing-rule Fragment expandables — view
   * state held outside the repainted DOM. */
  let routingTab: "flow" | "groups" = "flow";
  let orModelsOpen = false;
  const openFragments = new Set<string>();
  /** Models from the latest load (the routing-rule windows and
   *  form warnings resolve each backend's rated context from
   *  them, mirroring the generator). */
  let lastModels: ModelRow[] = [];
  /** Routing-rules form draft + Delete arming + the lazily fetched
   *  per-backend raw flags behind the form's --parallel warning. */
  const ruleForm: RuleFormState = { open: false, editingId: null, target: "", optimalTokens: "", advisory: true };
  const confirmRuleDelete = new Set<string>();
  const ruleRawFlags = new Map<string, string | null>();
  /** Generated config card: active tab + the routing callback
   *  preview text (fetched with the YAML). */
  let genTab: "yaml" | "routing" = "yaml";
  let routingPyText: string | null = null;

  const gwMsg = (text: string, isErr = false): void => {
    const m = root.querySelector("#gwMsg");
    if (m) m.innerHTML = text ? `<span style="color:${isErr ? "var(--err)" : "var(--text2)"}">${esc(text)}</span>` : "";
  };

  /** Append a manual-command block under the gateway message (a
   * failed start-gateway outcome carries one: the exact commands
   * to start the gateway by hand). #gwMsg lives outside #gwBody,
   * so the block survives the load() repaint that follows. The
   * command is also recorded on the plan UI state so the block's
   * Copy button (data-plan-act="copy-manual") copies this text. */
  const gwShowManual = (cmd: string): void => {
    planUi.manualCommand = cmd;
    const m = root.querySelector("#gwMsg");
    if (m) m.insertAdjacentHTML("beforeend", manualCommandHtml(cmd));
  };

  /** Fetch the master key on first use and cache it in the closure. */
  const ensureMasterKey = async (): Promise<string | null> => {
    if (masterKey !== null) return masterKey;
    const r = await getGatewayMasterKey();
    if (r.error || !r.data.master_key) {
      gwMsg(`Master key unavailable: ${r.error ?? "empty response"}`, true);
      return null;
    }
    masterKey = r.data.master_key;
    return masterKey;
  };

  const load = async (): Promise<void> => {
    const [st, cfg, backs, pr, mdls] = await Promise.all([getGatewayStatus(), getGatewayConfig(), getBackends(), getActivationPlan(), getModels()]);
    if (disposed) return;
    if (!pr.error && pr.data) lastPlan = pr.data;
    if (!mdls.error) lastModels = mdls.data;
    const err = st.error ?? cfg.error;
    // The OpenRouter spend-cap banner (spec §7) rides with the status
    // banner: cap reached, or spend at ≥80% of it.
    const spendBanner = st.error ? "" : openRouterSpendBannerHtml(st.data.openrouter_spend, icon("warn"));
    setBanner(root, "gwBanner", bannerHtml({ live: st.live && cfg.live, error: err })
      + (spendBanner ? `<div style="margin-bottom:12px">${spendBanner}</div>` : ""));
    const b = root.querySelector("#gwBody");
    if (!b) return;
    if (err) { b.innerHTML = emptyHtml(`Gateway configuration unavailable: ${err}`); return; }
    if (!cfg.data) {
      b.innerHTML = emptyHtml(st.live
        ? "The gateway returned no configuration."
        : "No gateway data in sample mode.");
      return;
    }
    lastCfg = cfg.data;
    lastStatus = st.data;
    lastPort = st.data.port || cfg.data.port;
    lastLanIp = st.data.lan_ip ?? null;
    b.innerHTML = gatewayPaint(st.data, cfg.data, backs.data, lastModels, yamlOpen, yamlText, yamlErr, backs.error ?? null, masterKey, keyRevealed, lastPlan, planUi, routingTab, orModelsOpen, openFragments, ruleForm, confirmRuleDelete, ruleRawFlags, genTab, routingPyText);
  };

  /** Fetch the generated config when a Copy/Download click needs
   * it and none is loaded (or the last fetch failed) — the buttons
   * never act on an empty string. Returns the ACTIVE TAB's text
   * (the routing callback tab when selected), or null. */
  const ensureGen = async (): Promise<string | null> => {
    const have = genTab === "routing" ? routingPyText : yamlText;
    if (have !== null && !yamlErr) return have;
    const r = await getGatewayYaml();
    if (disposed) return null;
    if (r.error) { yamlErr = r.error; await load(); return null; }
    yamlText = r.data.yaml;
    routingPyText = r.data.routing_py;
    yamlErr = "";
    return genTab === "routing" ? routingPyText : yamlText;
  };

  /** Serialize a rule back to the stored config shape for the
   *  update_gateway_config merge-patch. The fragment rides its
   *  untouched raw value, so saving an unrelated rule never
   *  rewrites a legacy rule's stored fragment. */
  const serializeRule = (r: GatewayRule): Record<string, unknown> => ({
    id: r.id,
    description: r.description,
    enabled: r.enabled,
    kind: r.kind,
    litellm_fragment: r.fragment_raw === undefined ? null : r.fragment_raw,
    source: r.source,
  });

  /** Save the full rules array through update_gateway_config (the
   *  generic gateway merge-patch — no dedicated command, design
   *  §7) and repaint with the outcome. */
  /** Returns true only when the change was saved (persisted):
   *  callers with an open form close it only on a true result
   *  (design rule 17 — a failed or non-persisted save keeps the
   *  form open with the operator's input intact). */
  const patchRules = async (rules: GatewayRule[], okMsg: string): Promise<boolean> => {
    const r = await updateGatewayConfig({ routing_rules: rules.map(serializeRule) });
    if (disposed) return false;
    let saved = false;
    if (r.error) {
      gwMsg(`Routing rule change failed: ${r.error}`, true);
    } else if (!r.data.persisted) {
      gwMsg("The change was applied to the in-memory configuration only — it was not saved and will be lost when the app restarts.", true);
    } else {
      gwMsg(okMsg);
      saved = true;
    }
    await load();
    return saved;
  };

  /** Open the rule form (add when `editing` is null, else edit that
   *  typed rule). Member backends' raw flags are fetched lazily
   *  for the form's --parallel warning (best-effort). */
  const openRuleForm = async (editing: GatewayRule | null): Promise<void> => {
    if (editing?.kind) {
      ruleForm.editingId = editing.id;
      ruleForm.target = editing.kind.target_group;
      ruleForm.optimalTokens = editing.kind.optimal_tokens !== null ? String(editing.kind.optimal_tokens) : "";
      ruleForm.advisory = editing.kind.advisory;
    } else {
      ruleForm.editingId = null;
      ruleForm.target = lastCfg?.groups[0]?.name ?? "";
      ruleForm.optimalTokens = "";
      ruleForm.advisory = true;
    }
    ruleForm.open = true;
    const ids = new Set((lastCfg?.groups ?? []).flatMap((g) => g.members));
    await Promise.all([...ids].map(async (id) => {
      if (ruleRawFlags.has(id)) return;
      const r = await getBackendConfig(id);
      if (r.data) ruleRawFlags.set(id, r.data.raw_flags);
    }));
    if (!disposed) await load();
  };

  const onClick = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLElement;

    const pt = t.closest("[data-plan-act]") as HTMLElement | null;
    if (pt) {
      await handlePlanAction(pt.getAttribute("data-plan-act") ?? "", planUi, () => { void load(); }, load, lastPlan);
      return;
    }

    const rt = t.closest("[data-routing-tab]") as HTMLElement | null;
    if (rt) {
      routingTab = rt.getAttribute("data-routing-tab") === "groups" ? "groups" : "flow";
      await load();
      return;
    }

    const fr = t.closest("[data-fragment]") as HTMLElement | null;
    if (fr) {
      const id = fr.getAttribute("data-fragment") ?? "";
      if (openFragments.has(id)) openFragments.delete(id); else openFragments.add(id);
      await load();
      return;
    }

    if (t.closest("[data-or-models]")) {
      orModelsOpen = !orModelsOpen;
      await load();
      return;
    }

    if (t.closest("#gwOrCredCopyBtn")) {
      const ref = lastCfg?.openrouter?.cred_ref;
      if (!ref) return;
      try {
        await navigator.clipboard.writeText(ref);
        gwMsg("Credential reference copied to the clipboard.");
      } catch {
        gwMsg("Copy failed — copy the credential reference manually.", true);
      }
      return;
    }

    if (t.closest("#gwYamlCopyBtn")) {
      const text = await ensureGen();
      if (text === null || disposed) return;
      try {
        await navigator.clipboard.writeText(text);
        gwMsg("Generated config copied to the clipboard.");
      } catch {
        gwMsg("Copy failed — open the preview and copy it manually.", true);
      }
      await load();
      return;
    }

    if (t.closest("#gwYamlDownloadBtn")) {
      const text = await ensureGen();
      if (text === null || disposed) return;
      const url = URL.createObjectURL(new Blob([text], { type: "text/yaml" }));
      const a = document.createElement("a");
      a.href = url;
      a.download = genTab === "routing" ? "llmdil_routing.py" : "config.yaml";
      a.click();
      URL.revokeObjectURL(url);
      await load();
      return;
    }

    const gt = t.closest("[data-gen-tab]") as HTMLElement | null;
    if (gt) {
      genTab = gt.getAttribute("data-gen-tab") === "routing" ? "routing" : "yaml";
      await load();
      return;
    }

    if (t.closest("#gwRuleAddBtn")) {
      await openRuleForm(null);
      return;
    }

    const rtog = t.closest("[data-rule-toggle]") as HTMLElement | null;
    if (rtog && lastCfg) {
      const id = rtog.getAttribute("data-rule-toggle") ?? "";
      const rules = lastCfg.routing_rules.map((r) => (r.id === id ? { ...r, enabled: !r.enabled } : r));
      const rule = rules.find((r) => r.id === id);
      await patchRules(rules, rule?.enabled ? `Routing rule ${id} enabled and saved.` : `Routing rule ${id} disabled and saved.`);
      return;
    }

    const redit = t.closest("[data-rule-edit]") as HTMLElement | null;
    if (redit && lastCfg) {
      const rule = lastCfg.routing_rules.find((r) => r.id === (redit.getAttribute("data-rule-edit") ?? ""));
      if (rule) await openRuleForm(rule);
      return;
    }

    const rdel = t.closest("[data-rule-delete]") as HTMLElement | null;
    if (rdel && lastCfg) {
      const id = rdel.getAttribute("data-rule-delete") ?? "";
      if (!confirmRuleDelete.has(id)) {
        confirmRuleDelete.add(id);
        await load();
        return;
      }
      confirmRuleDelete.delete(id);
      await patchRules(lastCfg.routing_rules.filter((r) => r.id !== id), `Routing rule ${id} deleted and saved.`);
      return;
    }

    if (t.closest("#gwRuleCancelBtn")) {
      ruleForm.open = false;
      await load();
      return;
    }

    if (t.closest("#gwRuleSaveBtn")) {
      if (!lastCfg) return;
      const optimalText = ruleForm.optimalTokens.trim();
      const optimal = optimalText === "" ? null : Number(optimalText);
      if (optimal !== null && (!Number.isFinite(optimal) || optimal <= 0 || Math.floor(optimal) !== optimal)) {
        gwMsg("Optimal tokens must be a positive whole number, or empty for the default (half the serving window).", true);
        return;
      }
      const editing = ruleForm.editingId !== null
        ? lastCfg.routing_rules.find((r) => r.id === ruleForm.editingId)
        : undefined;
      const kind: GatewayRuleKind = {
        type: "context-overflow",
        target_group: ruleForm.target,
        optimal_fraction: editing?.kind?.optimal_fraction ?? 0.5,
        optimal_tokens: optimal,
        output_reserve_tokens: editing?.kind?.output_reserve_tokens ?? 1024,
        advisory: ruleForm.advisory,
      };
      const description = `Requests too large for their group are rerouted to ${ruleForm.target}; past the optimal window the serving model advises compacting the conversation`;
      let rules: GatewayRule[];
      if (editing) {
        rules = lastCfg.routing_rules.map((r) => (r.id === editing.id ? { ...r, kind, description } : r));
      } else {
        let id = "context-overflow";
        for (let n = 2; lastCfg.routing_rules.some((r) => r.id === id); n++) id = `context-overflow-${n}`;
        rules = [...lastCfg.routing_rules, { id, description, source: "user", enabled: true, kind, litellm_fragment: undefined, fragment_raw: undefined }];
      }
      // Rule 17: the form closes only when the save persisted.
      // patchRules repaints from the ruleForm draft, so a failed
      // or non-persisted save leaves the form open with the
      // operator's input intact and the error in #gwMsg.
      const saved = await patchRules(rules, editing ? `Routing rule ${editing.id} saved.` : "Routing rule added and saved.");
      if (saved && !disposed) {
        ruleForm.open = false;
        await load();
      }
      return;
    }

    if (t.closest("#gwYamlBtn")) {
      if (yamlOpen) {
        yamlOpen = false;
        await load();
        return;
      }
      // Refetch on every Preview click: a cached copy (or a cached
      // failure) would hide config edits made since the last preview.
      yamlOpen = true;
      yamlText = null;
      routingPyText = null;
      yamlErr = "";
      await load();
      const r = await getGatewayYaml();
      if (disposed) return;
      if (r.error) yamlErr = r.error;
      else {
        yamlText = r.data.yaml;
        routingPyText = r.data.routing_py;
      }
      await load();
      return;
    }

    if (t.closest("#gwKeyRevealBtn")) {
      if (keyRevealed) {
        keyRevealed = false;
        await load();
        return;
      }
      if ((await ensureMasterKey()) === null) return;
      if (disposed) return;
      keyRevealed = true;
      await load();
      return;
    }

    if (t.closest("#gwKeyCopyBtn")) {
      const key = await ensureMasterKey();
      if (key === null || disposed) return;
      try {
        await navigator.clipboard.writeText(key);
        gwMsg("Master key copied to the clipboard.");
      } catch {
        gwMsg("Copy failed \u2014 Reveal the key and copy it manually.", true);
      }
      return;
    }

    if (t.closest("#gwBaseUrlCopyBtn")) {
      try {
        await navigator.clipboard.writeText(`http://127.0.0.1:${lastPort}/v1`);
        gwMsg("Base URL copied to the clipboard.");
      } catch {
        gwMsg("Copy failed \u2014 copy the base URL manually.", true);
      }
      return;
    }

    if (t.closest("#gwLanUrlCopyBtn")) {
      if (!lastLanIp) return;
      try {
        await navigator.clipboard.writeText(`http://${lastLanIp}:${lastPort}/v1`);
        gwMsg("LAN base URL copied to the clipboard.");
      } catch {
        gwMsg("Copy failed \u2014 copy the LAN base URL manually.", true);
      }
      return;
    }

    if (t.closest("#gwAdminUiBtn")) {
      const r = await openUrl(`http://127.0.0.1:${lastPort}/ui`);
      if (disposed) return;
      if (r.error) gwMsg(`Could not open the admin UI: ${r.error}`, true);
      return;
    }

    if (t.closest("#gwOrToggle")) {
      const or = lastCfg?.openrouter;
      if (!or) return;
      const btn = t.closest("#gwOrToggle") as HTMLButtonElement;
      btn.disabled = true;
      const r = await updateGatewayConfig({ openrouter: { enabled: !or.enabled } });
      if (disposed) return;
      if (r.error) {
        gwMsg(`OpenRouter tier change failed: ${r.error}`, true);
      } else if (!r.data.persisted) {
        gwMsg("The change was applied to the in-memory configuration only \u2014 it was not saved and will be lost when the app restarts.", true);
      } else {
        gwMsg(or.enabled ? "OpenRouter tier disabled and saved." : "OpenRouter tier enabled and saved.");
      }
      await load();
    }

    /** Shared save for the Gateway view's permanent fields
     *  (port, OpenRouter credential/cap/allowlist, per-group
     *  fallbacks). Rule 17: on success the repaint displays the
     *  saved value in its new state; on failure — or a
     *  non-persisted apply — there is NO repaint, so the
     *  operator's input stays intact in the field and the error
     *  shows in #gwMsg (which lives outside the repainted body).
     *  A repaint on failure would discard the typed value and
     *  redisplay the old saved one. */
    const gwSave = async (patch: Record<string, unknown>, okText: string): Promise<void> => {
      const r = await updateGatewayConfig(patch);
      if (disposed) return;
      if (r.error) { gwMsg(`Change not saved: ${r.error}`, true); return; }
      if (!r.data.persisted) { gwMsg("The change was applied to the in-memory configuration only — it was not saved and will be lost when the app restarts.", true); return; }
      gwMsg(okText);
      await load();
    };

    if (t.closest("#gwPortApply")) {
      const port = Math.round(Number((root.querySelector("#gwPortInput") as HTMLInputElement | null)?.value));
      if (!Number.isFinite(port) || port < 1 || port > 65535) {
        gwMsg("Gateway port must be between 1 and 65535.", true);
        return;
      }
      await gwSave({ port }, "Gateway port saved — applies at the next gateway start.");
      return;
    }

    if (t.closest("#gwOrCredSave")) {
      const v = ((root.querySelector("#gwOrCredInput") as HTMLInputElement | null)?.value ?? "").trim();
      await gwSave({ openrouter: { cred_ref: v === "" ? null : v } },
        v === "" ? "Credential reference cleared and saved." : "Credential reference saved.");
      return;
    }

    if (t.closest("#gwOrCapSave")) {
      const cap = Number((root.querySelector("#gwOrCapInput") as HTMLInputElement | null)?.value);
      if (!Number.isFinite(cap) || cap < 0) {
        gwMsg("Daily cap must be a number of dollars, 0 or more.", true);
        return;
      }
      await gwSave({ openrouter: { daily_cap_usd: cap } }, "OpenRouter daily cap saved.");
      return;
    }

    if (t.closest("#gwOrAllowSave")) {
      const raw = (root.querySelector("#gwOrAllowInput") as HTMLTextAreaElement | null)?.value ?? "";
      const list = [...new Set(raw.split(/[\n,]/).map((s) => s.trim()).filter(Boolean))];
      await gwSave({ openrouter: { model_allowlist: list } }, "OpenRouter model allowlist saved.");
      return;
    }

    const fbSave = t.closest("[data-group-fallbacks-save]") as HTMLElement | null;
    if (fbSave && lastCfg) {
      const name = fbSave.getAttribute("data-group-fallbacks-save") ?? "";
      const input = root.querySelector(`[data-group-fallbacks-input="${name}"]`) as HTMLInputElement | null;
      const list = [...new Set((input?.value ?? "").split(",").map((s) => s.trim()).filter(Boolean))];
      // Patch the full groups array with only this group's chain
      // changed; the stored fields ride through verbatim (the
      // derived enabled/enabled_members are not stored fields).
      const groups = lastCfg.groups.map((g) => ({
        name: g.name, members: g.members, strategy: g.strategy, purpose: g.purpose,
        fallbacks: g.name === name ? list : g.fallbacks,
      }));
      await gwSave({ groups }, `Fallbacks for ${name} saved — applies at the next gateway start.`);
      return;
    }
  };
  const onBindChange = async (ev: Event): Promise<void> => {
    const sel = ev.target as HTMLElement;
    if (!sel.closest("#gwBindSelect")) return;
    const host = (sel as HTMLSelectElement).value;
    const r = await setGatewayBindHost(host);
    if (r.error) {
      gwMsg(`Network access not saved: ${r.error}`, true);
      return;
    }
    gwMsg("Network access saved — applies at the next gateway restart; the start-gateway step also reconciles the Windows firewall rule.");
    await load();
  };
  /** Rule-form field changes update the draft in the closure (the
   *  form re-renders from it); Target/Optimal changes repaint so
   *  the §6.2 warnings track the draft. */
  const onRuleFormChange = async (ev: Event): Promise<void> => {
    const el = ev.target as HTMLElement;
    if (el.closest("#gwRuleTarget")) {
      ruleForm.target = (el as HTMLSelectElement).value;
      await load();
    } else if (el.closest("#gwRuleOptimal")) {
      ruleForm.optimalTokens = (el as HTMLInputElement).value;
      await load();
    } else if (el.closest("#gwRuleAdvisory")) {
      ruleForm.advisory = (el as HTMLInputElement).checked;
    }
  };
  root.addEventListener("click", onClick);
  root.addEventListener("change", onBindChange);
  root.addEventListener("change", onRuleFormChange);

  await load();

  // Lifecycle buttons (gateway Start/Stop/Restart, PostgreSQL Stop)
  // run through the shared runner: Restart is ONE server-side
  // operation (no client-side stop, no client activationComplete —
  // the server clears the plan), Stop stops the gateway only, and
  // Start/Restart offer the MCP credential prompt first.
  const unwireLc = wireLifecycleActions(root, (r: LifecycleResult) => {
    if (disposed) return;
    gwMsg(r.message, !r.ok && !r.cancelled);
    if (r.manualCommand) gwShowManual(r.manualCommand);
    void load();
  }, { origin: "ui" });
  // The lifecycle title, actions and PostgreSQL row repaint in place
  // from the shared store (every restart, from any surface, reads
  // "restarting…" here), without the full body repaint the hover
  // rule below guards.
  const paintLc = (): void => {
    if (!lastStatus) return;
    const t = root.querySelector("#gwLcTitle");
    if (t) setHtml(t, gatewayLcTitleHtml(lastStatus));
    const a = root.querySelector("#gwLcActions");
    if (a) setHtml(a, gatewayLcActionsHtml(lastStatus));
    const p = root.querySelector("#gwPgRow");
    if (p) setHtml(p, gatewayPgRowHtml(lastStatus));
  };
  const unsubscribeStore = serviceStore.subscribe(() => { if (!disposed) paintLc(); });

  // Status poll: a gateway that dies while the view is open must show
  // as down without a manual refresh. The repaint replaces #gwBody's
  // innerHTML wholesale, which destroys the node under the pointer —
  // a native title tooltip on an info-tip inside the body is
  // cancelled before it shows or dismissed while it is read (the
  // Routing test traces tip was reported exactly this way: cursor
  // changes, no tooltip). While the pointer hovers the body the
  // user is reading it, so the periodic repaint waits for the next
  // cycle; click-driven loads are unaffected.
  const timer = window.setInterval(() => {
    if (!disposed && !root.querySelector("#gwBody:hover")) void load();
  }, 5000);

  return () => {
    disposed = true;
    unwireLc();
    unsubscribeStore();
    window.clearInterval(timer);
    root.removeEventListener("click", onClick);
    root.removeEventListener("change", onBindChange);
    root.removeEventListener("change", onRuleFormChange);
  };
}

export {
  sameModelFile,
} from "./common";
export {
  renderGpus,
  wireGpus,
} from "./gpus";
export type {
  GpuModelRow,
} from "./gpus";
export {
  renderLogs,
  wireLogs,
} from "./logs";
export {
  renderSettings,
  wireSettings,
} from "./settings";
export {
  renderWizard,
  wireWizard,
} from "./wizard";
