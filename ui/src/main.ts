/* Local LLM Service Manager — UI shell (plain TypeScript, no framework).
   Hash router over 11 views; the Tauri command boundary stays in api.ts so a
   framework can replace this file later without changing command contracts. */

import "./styles.css";
import { appUpdateRestart, getActivationPlan, getAppConfig, getAppUpdateStatus, getBackends, getGatewayStatus, getSetupStatus, getStatus, getWizardSteps, isTauri } from "./api";
import type { ActivationPlanData, AppUpdateInfo } from "./api";
import { META, lifecycleDot, lifecycleLabel, serviceStore } from "./lifecycle";
import type { ServiceSnapshot } from "./lifecycle";
import { stackedBar } from "./charts";
import { esc, setHtml } from "./html";
import { icon, type IconName } from "./icons";
import { setupNeedsWizard } from "./setupGate";
import { onStartAllClick, paintStartAll } from "./startAll";
import { renderBench, wireBench } from "./views/bench";
import { mountDashboard } from "./views/dashboard";
import { renderGroups, wireGroups } from "./views/groups";
import { renderMcp, wireMcp } from "./views/mcp";
import { renderOptimize, wireOptimize } from "./views/optimize";
import { renderServiceMap, wireServiceMap } from "./views/serviceMap";
import {
  applyActivationPlan, planApplyLine,
  renderActivationPlan, renderBackends, renderGateway, renderGpus, renderLogs,
  renderModels, renderSettings, renderWizard,
  wireActivationPlan, wireBackends, wireGateway, wireGpus, wireLogs, wireModels, wireSettings, wireWizard,
} from "./views/static";

export {}; // module marker

interface Route { hash: string; label: string; icon: IconName; title: string }

const ROUTES: Route[] = [
  { hash: "#/dashboard", label: "Dashboard", icon: "dashboard", title: "Dashboard" },
  { hash: "#/map", label: "Service Map", icon: "map", title: "Service Map" },
  { hash: "#/gpus", label: "GPUs", icon: "gpu", title: "GPUs" },
  { hash: "#/backends", label: "Backends", icon: "server", title: "Backends" },
  { hash: "#/models", label: "Models", icon: "box", title: "Models" },
  { hash: "#/logs", label: "Logs", icon: "logs", title: "Logs" },
  { hash: "#/gateway", label: "Gateway", icon: "gateway", title: "Gateway" },
  { hash: "#/groups", label: "Groups", icon: "groups", title: "Groups" },
  { hash: "#/mcp", label: "MCP Servers", icon: "layers", title: "MCP Servers" },
  { hash: "#/optimize", label: "Optimize", icon: "target", title: "Optimize" },
  { hash: "#/bench", label: "Bench", icon: "gauge", title: "Bench" },
  { hash: "#/activation-plan", label: "Activation Plan", icon: "warn", title: "Activation Plan" },
  { hash: "#/settings", label: "Settings", icon: "gear", title: "Settings" },
  { hash: "#/wizard", label: "Setup Wizard", icon: "wrench", title: "Setup Wizard" },
];

let cleanup: (() => void) | null = null;
/** Incremented per render; async mounts that lose the race clean up after themselves. */
let renderGen = 0;

function shell(): string {
  return `<aside class="sidebar">
      <div class="brand"><div class="name">Local LLM Service</div><div class="sub">Dell Precision 7865</div></div>
      <nav class="nav">${ROUTES.map((r) => `<a href="${r.hash}" data-route="${r.hash}">${icon(r.icon)}<span class="lbl">${esc(r.label)}</span></a>`).join("")}</nav>
      <div id="sideServices"></div>
      <div class="side-foot" id="sideFoot"><div class="row"><span class="dot idle"></span> status…</div></div>
    </aside>
    <div class="main">
      <div id="view" style="display:flex;flex-direction:column;flex:1;min-height:0"></div>
      <div id="updateBanner"></div>
      <div id="activationBanner"></div>
      <div class="taskbar" id="taskbar"></div>
    </div>`;
}

/* ---------------- Global activation-plan banner ----------------
 * A bar docked under the view, above the task bar, shown whenever an
 * activation plan is pending — across scopes (gateway routes, backend
 * flags/splits, model assignment, GPU placement). It is docked at the
 * bottom so its late arrival never moves what the pointer is on. Review navigates to the dedicated Activation
 * Plan view; Apply executes the plan directly through the same
 * routine the plan card uses. It polls independently of the
 * current view, and a view-level dismissal (the plan card's X) never
 * hides it: saved-but-not-live changes stay visible here until
 * they are applied or cancelled. */
let bannerPlan: ActivationPlanData | null = null;
/** This window's own Apply call is in flight (a click guard only: the
 *  Apply buttons' disabled state and the "Applying…" line come from
 *  the server's activation_applying, so an apply started on ANY
 *  surface — another view, REST, CLI — disables them here too). */
let bannerApplyInFlight = false;
let bannerStatus = "";
let bannerStatusErr = false;
/** Manual-command block from a failed start-gateway outcome during
 * a banner Apply, rendered under the banner. */
let bannerManual = "";

function paintActivationBanner(): void {
  const el = document.getElementById("activationBanner");
  if (!el) return;
  const plan = bannerPlan;
  // The server-side apply in progress (any surface). The banner stays
  // rendered while it runs, even when the plan already resolved, so
  // the operator sees "Applying… <step>" until it finishes (ACT-5).
  const applying = serviceStore.get().gateway?.activation_applying ?? null;
  if (!plan || (!plan.pending && !applying)) { setHtml(el, ""); return; }
  if (!plan.pending && applying) {
    setHtml(el, `<div class="activation-banner">
    <span class="ab-text">${icon("clock")} <b>Applying the activation plan…</b></span>
    <span class="ab-status">${esc(applying.message)}</span></div>`);
    return;
  }
  // The banner renders the plan card's summary variant (one line:
  // warn icon, the pending count as Label: Value, the scope
  // segmented bar, Review + Apply) with the shell's own state; the
  // execution path is the shared applyActivationPlan.
  const n = plan.changes.length;
  const backend = plan.changes.filter((c) => c.scope === "backend").length;
  const gateway = n - backend;
  const bar = !n
    ? `${stackedBar([{ pct: 100, color: "#6d7686", label: "Gateway drift" }], 260, 8)}<span style="font-size:11px;color:var(--text3);white-space:nowrap">Gateway drift</span>`
    : `${stackedBar([
        ...(backend ? [{ pct: (backend / n) * 100, color: "#4a9eff", label: `Backend: ${backend}` }] : []),
        ...(gateway ? [{ pct: (gateway / n) * 100, color: "#a371f7", label: `Gateway: ${gateway}` }] : []),
      ], 340, 8)}<span style="font-size:11px;color:var(--text3);white-space:nowrap">${[backend ? "Backend" : "", gateway ? "Gateway" : ""].filter(Boolean).join(" · ")}</span>`;
  const dis = bannerApplyInFlight || applying ? " disabled" : "";
  // Drain/Apply key on the gateway's lifecycle state from the shared
  // store (running/not responding = restart it; anything else = the
  // single Apply, which starts it), the plan's probe as the fallback.
  const gwState = serviceStore.gatewayState();
  const gwLive = gwState !== null ? META[gwState].isLive : plan.gateway_up;
  const status = applying && !bannerApplyInFlight ? `Applying… ${applying.message}` : bannerStatus;
  // The same actions the plan card offers, under the same labels and
  // modes: drain-or-restart when the gateway is up with a gateway
  // change pending, otherwise the single Apply plan (start) action.
  // The composed consequence line rides the Apply buttons' tooltip
  // and renders as visible text on its own banner row: a gateway
  // restart (and its possible install repair) must be stated before
  // the operator clicks, not only on hover (2026-10-18).
  const tip = esc(planApplyLine(plan));
  const applyBtns = !plan.actionable
    ? ""
    : plan.gateway_pending && gwLive
      ? `<button class="btn" data-ab-act="drain" title="${tip}"${dis}>${icon("pause")} Drain Requests and Apply</button>
      <button class="btn" data-ab-act="apply-now" title="${tip}"${dis}>${icon("restart")} Apply now</button>`
      : `<button class="btn" data-ab-act="apply" title="${tip}"${dis}>${icon("play")} Apply plan</button>`;
  setHtml(el, `<div class="activation-banner">
    <span class="ab-text">${icon("warn")} <b>Activation Plan pending</b> — Pending changes: ${n}</span>
    <span style="display:inline-flex;align-items:center;gap:8px;flex:0 1 340px;min-width:160px">${bar}</span>
    ${status ? `<span class="ab-status${bannerStatusErr && status === bannerStatus ? " err" : ""}">${esc(status)}</span>` : ""}
    <span class="ab-actions">
      <button class="btn" data-ab-act="review" title="Review activation plan"${dis}>${icon("eye")} Review</button>
      ${applyBtns}
    </span>
    ${plan.actionable ? `<span style="flex-basis:100%">${esc(planApplyLine(plan))}</span>` : ""}</div>`
    + (bannerManual ? `<pre style="margin:0 12px 8px;font-family:var(--mono);font-size:11px;white-space:pre-wrap;word-break:break-all;background:var(--panel);border:1px solid var(--border);border-radius:6px;padding:8px;text-align:left">${esc(bannerManual)}</pre><div style="padding:0 12px 8px"><button class="btn icon-only" data-ab-act="copy-manual" title="Copy commands" aria-label="Copy commands">${icon("copy")}</button></div>` : ""));
}

async function refreshActivationBanner(): Promise<void> {
  if (!isTauri()) { bannerPlan = null; paintActivationBanner(); return; }
  const r = await getActivationPlan();
  if (r.error) return; // keep the last known state on a failed poll
  bannerPlan = r.data;
  if (!r.data.pending) { bannerStatus = ""; bannerStatusErr = false; bannerManual = ""; }
  paintActivationBanner();
}

async function onBannerClick(ev: Event): Promise<void> {
  const t = (ev.target as HTMLElement).closest("[data-ab-act]") as HTMLElement | null;
  if (!t) return;
  const act = t.getAttribute("data-ab-act");
  if (act === "review") { location.hash = "#/activation-plan"; return; }
  if (act === "copy-manual") {
    if (!bannerManual) return;
    try {
      await navigator.clipboard.writeText(bannerManual);
      bannerStatus = "Manual commands copied to the clipboard.";
      bannerStatusErr = false;
    } catch {
      bannerStatus = "Copy failed — select the commands in the block and copy them manually.";
      bannerStatusErr = true;
    }
    paintActivationBanner();
    return;
  }
  // Banner action -> the plan card's execution mode for the same
  // label: drain = quiesce wait then restart, Apply now = restart,
  // Apply plan = start (no stop; starts a down gateway).
  const modes: Record<string, "quiesce" | "restart" | "start"> = {
    drain: "quiesce", "apply-now": "restart", apply: "start",
  };
  const mode = act ? modes[act] : undefined;
  if (!mode || bannerApplyInFlight || serviceStore.get().gateway?.activation_applying || !bannerPlan) return;
  bannerApplyInFlight = true;
  const setStatus = (text: string, err = false): void => {
    bannerStatus = text;
    bannerStatusErr = err;
    paintActivationBanner();
  };
  const setManual = (cmd: string | null): void => {
    bannerManual = cmd ?? "";
    paintActivationBanner();
  };
  setStatus("Applying the activation plan…");
  const plan = bannerPlan;
  const ok = await applyActivationPlan(plan, mode, setStatus, setManual);
  bannerApplyInFlight = false;
  // The server's result message is already in bannerStatus (setStatus).
  void ok;
  paintActivationBanner();
  await refreshActivationBanner();
  void render();
}

/* Scroll position per route: kept when a page is left and returned to,
 * and across a re-render of the same page (an apply, a save). */
const scrollByRoute = new Map<string, number>();
let shownRoute: string | null = null;

function rememberScroll(): void {
  const el = document.querySelector("#view > .view");
  if (shownRoute !== null && el) scrollByRoute.set(shownRoute, el.scrollTop);
}

function restoreScroll(route: string): void {
  const el = document.querySelector("#view > .view");
  const top = scrollByRoute.get(route) ?? 0;
  if (el && top > 0) el.scrollTop = top;
}

/* ---------------- Global app-update banner ----------------
 * Shown whenever the app-update state carries a staged update
 * newer than the running version — the banner is derived from
 * that state on every poll, never from a one-time event, so it
 * survives navigation and app restarts until the update is
 * applied or superseded. Restart hands off to the backend's
 * apply helper and the app exits; a failure recorded by a
 * previous apply for this same version rides the banner so the
 * operator sees why it is still here. */
let updateInfo: AppUpdateInfo | null = null;
let updateRestarting = false;
let updateStatus = "";
let updateStatusErr = false;

function paintUpdateBanner(): void {
  const el = document.getElementById("updateBanner");
  if (!el) return;
  const banner = updateInfo?.banner;
  if (!banner) { el.innerHTML = ""; return; }
  const fail = banner.last_apply_error;
  el.innerHTML = `<div class="update-banner">
    <span class="ab-text">${icon("restart")} <b>Restart LLMdil to update</b> — Version: ${esc(banner.version)}</span>
    ${fail ? `<span class="ab-status err">Previous update attempt failed: ${esc(fail)}</span>` : ""}
    ${updateStatus ? `<span class="ab-status${updateStatusErr ? " err" : ""}">${esc(updateStatus)}</span>` : ""}
    <span class="ab-actions">
      <button class="btn" data-ub-act="restart"${updateRestarting ? " disabled" : ""}>${icon("restart")} Restart</button>
    </span>
  </div>`;
}

async function refreshUpdateBanner(): Promise<void> {
  if (!isTauri()) { updateInfo = null; paintUpdateBanner(); return; }
  const r = await getAppUpdateStatus();
  if (r.error) return; // keep the last known state on a failed poll
  updateInfo = r.data;
  if (!r.data.banner) { updateStatus = ""; updateStatusErr = false; }
  paintUpdateBanner();
}

async function onUpdateBannerClick(ev: Event): Promise<void> {
  const t = (ev.target as HTMLElement).closest("[data-ub-act]") as HTMLElement | null;
  if (!t || t.getAttribute("data-ub-act") !== "restart" || updateRestarting) return;
  updateRestarting = true;
  updateStatus = "Restarting to apply the update…";
  updateStatusErr = false;
  paintUpdateBanner();
  const r = await appUpdateRestart();
  if (r.error) {
    updateRestarting = false;
    updateStatus = `Restart failed: ${r.error}`;
    updateStatusErr = true;
    paintUpdateBanner();
  }
  // On success the process exits; nothing further renders.
}

/** Sidebar footer data that is not lifecycle (config-load outcome,
 *  MXC mode): fetched on navigation, not on every store refresh. */
let sideFootStatic: { reachable: boolean; cfgRow: string; mxc: string | null; mxcInstalled: boolean | null } | null = null;

async function refreshSideFootStatic(): Promise<void> {
  if (!isTauri()) return;
  const [s, cfg] = await Promise.all([getStatus(), getAppConfig()]);
  if (s.error) { sideFootStatic = { reachable: false, cfgRow: "", mxc: null, mxcInstalled: null }; paintSideFoot(serviceStore.get()); return; }
  // Abnormal-condition row only (design rule 5): the config came from
  // the .bak copy or from built-in defaults (code review v0.1.60 F2).
  // Details (error, preserved copies) live in the tooltip.
  const cl = s.data.config_load;
  const cfgRow = cl && (cl.source === "backup" || cl.source === "defaults")
    ? `<div class="row" title="${esc([cl.error ?? "", ...cl.preserved.map((p) => `Preserved: ${p}`)].filter(Boolean).join("\n"))}"><span class="dot err"></span> Config: ${cl.source === "backup" ? "restored from .bak" : "defaults (load failed)"}</div>`
    : "";
  const mxc = (cfg.data?.mxc as { mode?: string } | undefined)?.mode ?? null;
  sideFootStatic = { reachable: true, cfgRow, mxc, mxcInstalled: s.data.mxc_runtime_installed ?? null };
  paintSideFoot(serviceStore.get());
}

/** Sidebar status footer, painted from the shared service store on
 *  every refresh: the gateway by its lifecycle state (a restart reads
 *  "restarting…", an operator stop "stopped" — never a bare "down"),
 *  the backend count by the shared rule (enabled + configured
 *  backends, the same counts as the Dashboard and the Service Map). */
function paintSideFoot(snap: ServiceSnapshot): void {
  const el = document.getElementById("sideFoot");
  if (!el) return;
  if (!snap.live) {
    el.innerHTML = `<div class="row"><span class="dot idle"></span> disconnected · sample data</div>`;
    return;
  }
  if (snap.updatedAt === 0) return; // first refresh still in flight
  if ((sideFootStatic && !sideFootStatic.reachable) || (snap.backendsError && !snap.backends.length)) {
    el.innerHTML = `<div class="row"><span class="dot err"></span> backend unreachable</div>`;
    return;
  }
  const gwState = serviceStore.gatewayState();
  // An errored status call paints no port and no green dot (the state
  // is unknown, not up).
  const gwPort = snap.gateway ? snap.gateway.running_port ?? snap.gateway.port : 0;
  const portPending = Boolean(snap.gateway) && gwPort !== snap.gateway?.port;
  const gwRow = !snap.gateway || gwState === null
    ? `<div class="row"><span class="dot idle"></span> LiteLLM status unknown</div>`
    : `<div class="row" title="${esc(lifecycleLabel(gwState, serviceStore.transitionOf("gateway", snap.gateway.transition)))}">${lifecycleDot(gwState)} LiteLLM :${gwPort}${portPending ? ` · saved :${snap.gateway.port}` : ""}${gwState === "running" ? "" : ` · ${esc(lifecycleLabel(gwState))}`}</div>`;
  const sum = serviceStore.summary();
  const backDot = sum.backendsFailed ? "err"
    : sum.backendsTotal && sum.backendsRunning === sum.backendsTotal ? "ok"
    : sum.backendsTransitioning ? "warn"
    : sum.backendsTotal ? "warn" : "idle";
  const changing = sum.backendsTransitioning ? ` · ${sum.backendsTransitioning} changing` : "";
  const mxc = sideFootStatic?.mxc ?? null;
  el.innerHTML = `
    ${sideFootStatic?.cfgRow ?? ""}
    ${gwRow}
    <div class="row"><span class="dot ${backDot}"></span> ${sum.backendsRunning}/${sum.backendsTotal} backends up${changing}</div>
    ${sideFootStatic?.mxcInstalled === false
      ? `<div class="row"><span class="dot idle"></span> MXC · not installed</div>`
      : `<div class="row"><span class="dot ${mxc ? "warn" : "idle"}"></span> MXC · ${mxc ? esc(mxc) : "unknown"}</div>`}`;
}

/** Repaint the sidebar and the activation banner from every store
 *  refresh; reload the plan when a server-side apply starts or ends
 *  (the banner's pending count changes then). Subscribed once for the
 *  app's lifetime — this subscription also keeps the shared store
 *  polling on every page (UI-10). */
let lastApplying = false;
function onStoreSnapshot(snap: ServiceSnapshot): void {
  paintSideFoot(snap);
  paintStartAll(snap);
  const applying = Boolean(snap.gateway?.activation_applying);
  if (applying !== lastApplying) {
    lastApplying = applying;
    void refreshActivationBanner();
  } else {
    paintActivationBanner();
  }
}

function renderTaskbar(): void {
  const el = document.getElementById("taskbar");
  if (!el) return;
  // No background-task feed yet — render idle instead of a fabricated download.
  el.innerHTML = `<span class="quiet">No background tasks</span>
    <span class="grow"></span><span class="quiet">Task drawer</span>`;
}

/** Query params in the hash (e.g. #/logs?scope=coder&follow=1 — the
 *  Logs deep link carried by the "go to logs" buttons on entity cards:
 *  the entity's scope preselected, Follow on). Params are applied on
 *  entry and become the Logs view's session state (tracker #59): a
 *  later plain visit restores the session state as left instead of
 *  starting from defaults; defaults return only on an app restart. */
function hashParams(): URLSearchParams {
  return new URLSearchParams((location.hash.split("?")[1] ?? ""));
}

async function render(): Promise<void> {
  const gen = ++renderGen;
  rememberScroll();
  if (cleanup) { cleanup(); cleanup = null; }
  const hash = location.hash || "#/dashboard";
  // Routes match on the path part only; the query carries view params.
  const path = hash.split("?")[0];
  const route = ROUTES.find((r) => r.hash === path) ?? ROUTES[0];
  document.querySelectorAll(".nav a").forEach((a) =>
    a.classList.toggle("active", (a as HTMLElement).dataset.route === route.hash));
  const view = document.getElementById("view");
  if (!view) return;

  switch (route.hash) {
    case "#/dashboard":
      view.innerHTML = "";
      {
        const c = await mountDashboard();
        if (gen !== renderGen) { c(); return; } // superseded by a newer navigation
        cleanup = c;
      }
      break;
    case "#/map":
      view.innerHTML = renderServiceMap();
      {
        const c = await wireServiceMap(view);
        if (gen !== renderGen) { c(); return; }
        cleanup = c;
      }
      break;
    case "#/gpus":
      view.innerHTML = renderGpus();
      {
        const c = await wireGpus(view);
        if (gen !== renderGen) { c(); return; } // superseded by a newer navigation
        cleanup = c;
      }
      break;
    case "#/backends":
      view.innerHTML = renderBackends();
      {
        const c = await wireBackends(view);
        if (gen !== renderGen) { c(); return; }
        cleanup = c;
      }
      break;
    case "#/models":
      view.innerHTML = renderModels();
      {
        const c = await wireModels(view);
        if (gen !== renderGen) { c(); return; }
        cleanup = c;
      }
      break;
    case "#/logs":
      view.innerHTML = renderLogs();
      {
        const c = await wireLogs(view, hashParams().get("scope") ?? undefined, hashParams().get("follow") === "1");
        if (gen !== renderGen) { c(); return; }
        cleanup = c;
      }
      break;
    case "#/gateway":
      view.innerHTML = renderGateway();
      {
        const c = await wireGateway(view);
        if (gen !== renderGen) { c(); return; }
        cleanup = c;
      }
      break;
    case "#/groups":
      view.innerHTML = renderGroups();
      {
        const c = await wireGroups(view);
        if (gen !== renderGen) { c(); return; }
        cleanup = c;
      }
      break;
    case "#/mcp":
      view.innerHTML = renderMcp();
      {
        const c = await wireMcp(view);
        if (gen !== renderGen) { c(); return; }
        cleanup = c;
      }
      break;
    case "#/optimize":
      view.innerHTML = renderOptimize();
      {
        const c = await wireOptimize(view);
        if (gen !== renderGen) { c(); return; }
        cleanup = c;
      }
      break;
    case "#/bench":
      view.innerHTML = renderBench();
      {
        const c = await wireBench(view);
        if (gen !== renderGen) { c(); return; }
        cleanup = c;
      }
      break;
    case "#/activation-plan":
      view.innerHTML = renderActivationPlan();
      {
        const c = await wireActivationPlan(view);
        if (gen !== renderGen) { c(); return; }
        cleanup = c;
      }
      break;
    case "#/settings":
      view.innerHTML = renderSettings();
      {
        const c = await wireSettings(view);
        if (gen !== renderGen) { c(); return; }
        cleanup = c;
      }
      break;
    case "#/wizard":
      view.innerHTML = renderWizard();
      cleanup = wireWizard(view);
      break;
    default:
      view.innerHTML = "";
  }
  if (gen !== renderGen) return; // a newer render took over mid-flight
  shownRoute = route.hash;
  restoreScroll(route.hash);
  renderTaskbar();
  void refreshActivationBanner();
  void refreshUpdateBanner();
  void refreshSideFootStatic();
  // Navigation is a good moment to re-read lifecycle state too (cheap;
  // coalesced with any poll in flight).
  void serviceStore.refresh();
}

async function main(): Promise<void> {
  const app = document.getElementById("app");
  if (!app) return;
  app.innerHTML = shell();
  window.addEventListener("hashchange", () => void render());
  document.getElementById("activationBanner")?.addEventListener("click", (ev) => void onBannerClick(ev));
  document.getElementById("sideServices")?.addEventListener("click", onStartAllClick);
  document.getElementById("updateBanner")?.addEventListener("click", (ev) => void onUpdateBannerClick(ev));
  void refreshActivationBanner();
  void refreshUpdateBanner();
  // The sidebar and the banner's lifecycle parts follow the shared
  // store; the plan itself (pending changes) is re-read on a slower
  // cadence and whenever an apply starts or ends.
  serviceStore.subscribe(onStoreSnapshot);
  window.setInterval(() => { void refreshActivationBanner(); }, 5000);
  window.setInterval(() => { void refreshUpdateBanner(); }, 30000);
  // First launch: open the setup wizard while setup is not marked
  // complete AND a part the app cannot run without is missing. An
  // existing installation that never pressed Finish starts normally.
  if (isTauri() && !location.hash) {
    try {
      const s = await getSetupStatus();
      if (s.live && !s.data.setup_completed) {
        const [b, g, w] = await Promise.all([getBackends(), getGatewayStatus(), getWizardSteps()]);
        if (setupNeedsWizard({ backends: b.error ? null : b.data, gateway: g.error ? null : g.data.state, steps: w.data }) && !location.hash) location.hash = "#/wizard";
      }
    } catch { /* fall through to default route */ }
  }
  void render();
}

void main();
