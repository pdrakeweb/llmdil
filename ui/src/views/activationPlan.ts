/* Activation plan panel, shared by the Backends and Models views
   (split from static.ts). */

import { activationApply, activationCancelChanges, getActivationExecution, getActivationHistory, getActivationPlan, planNeedsElevation } from "../api";
import type { ActivationChange, ActivationExecution, ActivationExecutionSummary, ActivationPlanData } from "../api";
import { fmtCount, stackedBar } from "../charts";
import { esc } from "../html";
import { icon } from "../icons";
import { META, lifecycleChip, serviceStore } from "../lifecycle";
import type { LifecycleState } from "../lifecycle";
import { C, fmtDate, fmtTime, loadingHtml, topbar } from "./common";

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

export interface PlanUiState {
  dismissed: boolean;
  status: string;
  statusErr: boolean;
  confirmCancel: boolean;
  acting: boolean;
  /** Manual-command block from the last failed start-gateway
   * outcome, rendered under the status line. */
  manualCommand: string;
}

export function newPlanUi(): PlanUiState {
  return { dismissed: false, status: "", statusErr: false, confirmCancel: false, acting: false, manualCommand: "" };
}

/** The manual-command code block shared by the plan card surfaces:
 * the block's own first line says what it is, so it renders bare.
 * The commands are a value the operator must run by hand, so the
 * block carries a Copy button (principle 10) — word-break:break-all
 * makes manual selection error-prone. The button routes through the
 * plan surfaces' shared click handling (data-plan-act). */
export function manualCommandHtml(cmd: string): string {
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
  return planApplyCore(plan) + (planNeedsElevation(plan) ? " Windows will ask for administrator approval." : "");
}

function planApplyCore(plan: ActivationPlanData): string {
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
export function activationPlanHtml(plan: ActivationPlanData | null, ui: PlanUiState, variant: "summary" | "full" = "full"): string {
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
  const quiesce = r.data.quiesce;
  const drainNote = quiesce?.timed_out ? ` Drain timed out with ${fmtCount(quiesce.in_flight)} request${quiesce.in_flight === 1 ? "" : "s"} still in flight.` : "";
  const message = drainedMessage(r.data.message, drainNote)
    || (ok ? "Activation plan applied — the saved changes are live." : `Activation apply failed${r.error ? `: ${r.error}` : "."}`);
  setStatus(message, !ok);
  if (r.data.manual_command) setManual?.(r.data.manual_command);
  return ok;
}

/** The server's message with the drain note, unless it says so itself. */
function drainedMessage(message: string, note: string): string {
  return message && note && !/timed out/i.test(message) ? message + note : message;
}

/** Shared click handling for the plan card's buttons, used by the
 *  Backends and Gateway views. `repaint` re-renders the card in
 *  place; `reload` refreshes the whole view afterwards. */
export async function handlePlanAction(
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
 * coder" / "Gateway"), every backend named. */
function planTargetCell(c: ActivationChange): string {
  if (c.scope !== "backend") return "Gateway";
  const targets = c.targets ?? [];
  if (!targets.length) return "Backend";
  return `Backend: ${esc(targets.join(", "))}`;
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
    const meta = `<div style="font-size:12px;color:var(--text2);margin-top:6px">Mode: <b style="color:var(--text)">${esc(modeLabel)}</b> · Source: <b style="color:var(--text)">${esc(detail.source)}</b> · Gateway: ${lifecycleChip(detail.gateway_up ? "running" : "stopped", "The gateway's state when this apply started")}${detail.drift ? ` · Gateway config: <b style="color:var(--text)">drifted</b>` : ""}${detail.quiesce?.timed_out ? ` · Drain: <b style="color:#e3b341">timed out${typeof detail.quiesce.in_flight === "number" ? ` (${fmtCount(detail.quiesce.in_flight)} in flight)` : ""}</b>` : ""}</div>`;
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
