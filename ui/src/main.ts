/* Local LLM Service Manager — UI shell (plain TypeScript, no framework).
   Hash router over 11 views; the Tauri command boundary stays in api.ts so a
   framework can replace this file later without changing command contracts. */

import "./styles.css";
import { getActivationPlan, getAppConfig, getBackends, getGatewayStatus, getSetupStatus, getStatus, isTauri } from "./api";
import type { ActivationPlanData } from "./api";
import { stackedBar } from "./charts";
import { esc } from "./html";
import { icon, type IconName } from "./icons";
import { mountDashboard } from "./views/dashboard";
import { renderMcp, wireMcp } from "./views/mcp";
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
  { hash: "#/mcp", label: "MCP Servers", icon: "layers", title: "MCP Servers" },
  { hash: "#/activation-plan", label: "Activation Plan", icon: "warn", title: "Activation Plan" },
  { hash: "#/settings", label: "Settings", icon: "gear", title: "Settings" },
  { hash: "#/wizard", label: "Setup Wizard", icon: "wrench", title: "Setup Wizard" },
];

let cleanup: (() => void) | null = null;
/** Incremented per render; async mounts that lose the race clean up after themselves. */
let renderGen = 0;
/** The sidebar footer's own refresh timer. The footer otherwise only
 *  repaints on navigation, so a gateway that dies while the operator
 *  stays on one view would keep showing its last-probed state. Started
 *  once (guarded), never stacked by repeated render() calls. */
let sideFootTimer: number | null = null;

function shell(): string {
  return `<aside class="sidebar">
      <div class="brand"><div class="name">Local LLM Service</div><div class="sub">Dell Precision 7865</div></div>
      <nav class="nav">${ROUTES.map((r) => `<a href="${r.hash}" data-route="${r.hash}">${icon(r.icon)}<span class="lbl">${esc(r.label)}</span></a>`).join("")}</nav>
      <div class="side-foot" id="sideFoot"><div class="row"><span class="dot idle"></span> status…</div></div>
    </aside>
    <div class="main">
      <div id="activationBanner"></div>
      <div id="view" style="display:flex;flex-direction:column;flex:1;min-height:0"></div>
      <div class="taskbar" id="taskbar"></div>
    </div>`;
}

/* ---------------- Global activation-plan banner ----------------
 * A header at the very top of the main column, above every view's
 * title, shown whenever an activation plan is pending — across
 * scopes (gateway routes, backend flags/splits, model assignment,
 * GPU placement). Review navigates to the dedicated Activation
 * Plan view; Apply executes the plan directly through the same
 * routine the plan card uses. It polls independently of the
 * current view, and a view-level dismissal (the plan card's X) never
 * hides it: saved-but-not-live changes stay visible here until
 * they are applied or cancelled. */
let bannerPlan: ActivationPlanData | null = null;
let bannerActing = false;
let bannerStatus = "";
let bannerStatusErr = false;
/** Manual-command block from a failed start-gateway outcome during
 * a banner Apply, rendered under the banner. */
let bannerManual = "";

function paintActivationBanner(): void {
  const el = document.getElementById("activationBanner");
  if (!el) return;
  const plan = bannerPlan;
  if (!plan || !plan.pending) { el.innerHTML = ""; return; }
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
  const dis = bannerActing ? " disabled" : "";
  // The same actions the plan card offers, under the same labels and
  // modes: drain-or-restart when the gateway is up with a gateway
  // change pending, otherwise the single Apply plan (start) action.
  // The composed consequence line rides the Apply buttons' tooltip.
  const tip = esc(planApplyLine(plan));
  const applyBtns = !plan.actionable
    ? ""
    : plan.gateway_pending && plan.gateway_up
      ? `<button class="btn" data-ab-act="drain" title="${tip}"${dis}>${icon("pause")} Drain Requests and Apply</button>
      <button class="btn" data-ab-act="apply-now" title="${tip}"${dis}>${icon("restart")} Apply now</button>`
      : `<button class="btn" data-ab-act="apply" title="${tip}"${dis}>${icon("play")} Apply plan</button>`;
  el.innerHTML = `<div class="activation-banner">
    <span class="ab-text">${icon("warn")} <b>Activation Plan pending</b> — Pending changes: ${n}</span>
    <span style="display:inline-flex;align-items:center;gap:8px;flex:0 1 340px;min-width:160px">${bar}</span>
    ${bannerStatus ? `<span class="ab-status${bannerStatusErr ? " err" : ""}">${esc(bannerStatus)}</span>` : ""}
    <span class="ab-actions">
      <button class="btn" data-ab-act="review" title="Review activation plan"${dis}>${icon("eye")} Review</button>
      ${applyBtns}
    </span></div>`
    + (bannerManual ? `<pre style="margin:0 12px 8px;font-family:var(--mono);font-size:11px;white-space:pre-wrap;word-break:break-all;background:var(--panel);border:1px solid var(--border);border-radius:6px;padding:8px;text-align:left">${esc(bannerManual)}</pre><div style="padding:0 12px 8px"><button class="btn icon-only" data-ab-act="copy-manual" title="Copy commands" aria-label="Copy commands">${icon("copy")}</button></div>` : "");
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
  if (!mode || bannerActing || !bannerPlan) return;
  bannerActing = true;
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
  bannerActing = false;
  if (ok) bannerStatus = "Activation plan applied — the saved changes are live.";
  paintActivationBanner();
  await refreshActivationBanner();
  void render();
}

/** Sidebar status footer, driven by live data when available. */
async function renderSideFoot(): Promise<void> {
  const el = document.getElementById("sideFoot");
  if (!el) return;
  if (!isTauri()) {
    el.innerHTML = `<div class="row"><span class="dot idle"></span> disconnected · sample data</div>`;
    return;
  }
  const [s, b, gw, cfg] = await Promise.all([getStatus(), getBackends(), getGatewayStatus(), getAppConfig()]);
  if (s.error || b.error) {
    el.innerHTML = `<div class="row"><span class="dot err"></span> backend unreachable</div>`;
    return;
  }
  const up = b.data.filter((x) => x.state === "running").length;
  const mxc = (cfg.data?.mxc as { mode?: string } | undefined)?.mode;
  // The LiteLLM row keys to the gateway probe: an errored status call
  // paints no port and no green dot (the state is unknown, not up).
  const gwRow = gw.error
    ? `<div class="row"><span class="dot err"></span> LiteLLM status unknown</div>`
    : `<div class="row"><span class="dot ${gw.data.up ? "ok" : "err"}"></span> LiteLLM :${gw.data.port}${gw.data.up ? "" : " · down"}</div>`;
  el.innerHTML = `
    ${gwRow}
    <div class="row"><span class="dot ${b.data.length && up === b.data.length ? "ok" : "warn"}"></span> ${up}/${b.data.length} backends up</div>
    <div class="row"><span class="dot warn"></span> MXC · ${mxc ? esc(mxc) : "unknown"}</div>`;
}

function renderTaskbar(): void {
  const el = document.getElementById("taskbar");
  if (!el) return;
  // No background-task feed yet — render idle instead of a fabricated download.
  el.innerHTML = `<span class="quiet">No background tasks</span>
    <span class="grow"></span><span class="quiet">Task drawer</span>`;
}

/** Query params in the hash (e.g. #/logs?scope=llama-a4000 — the Logs
 *  deep link from the Backends view). */
function hashParams(): URLSearchParams {
  return new URLSearchParams((location.hash.split("?")[1] ?? ""));
}

async function render(): Promise<void> {
  const gen = ++renderGen;
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
        const c = await wireLogs(view, hashParams().get("scope") ?? undefined);
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
    case "#/mcp":
      view.innerHTML = renderMcp();
      {
        const c = await wireMcp(view);
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
  renderTaskbar();
  void refreshActivationBanner();
  void renderSideFoot();
  if (sideFootTimer === null) {
    sideFootTimer = window.setInterval(() => { void renderSideFoot(); }, 15000);
  }
}

async function main(): Promise<void> {
  const app = document.getElementById("app");
  if (!app) return;
  app.innerHTML = shell();
  window.addEventListener("hashchange", () => void render());
  document.getElementById("activationBanner")?.addEventListener("click", (ev) => void onBannerClick(ev));
  void refreshActivationBanner();
  window.setInterval(() => { void refreshActivationBanner(); }, 10000);
  // First launch: open the setup wizard until it is marked complete.
  if (isTauri() && !location.hash) {
    try {
      const s = await getSetupStatus();
      if (s.live && !s.data.setup_completed) location.hash = "#/wizard";
    } catch { /* fall through to default route */ }
  }
  void render();
}

void main();
