/* Local LLM Service Manager — UI shell (plain TypeScript, no framework).
   Hash router over 9 views; the Tauri command boundary stays in api.ts so a
   framework can replace this file later without changing command contracts. */

import "./styles.css";
import { getAppConfig, getBackends, getGatewayStatus, getSetupStatus, getStatus, isTauri } from "./api";
import { esc } from "./html";
import { icon, type IconName } from "./icons";
import { mountDashboard } from "./views/dashboard";
import { renderServiceMap, wireServiceMap } from "./views/serviceMap";
import {
  renderBackends, renderGateway, renderGpus, renderLogs,
  renderModels, renderSettings, renderWizard,
  wireBackends, wireGateway, wireGpus, wireLogs, wireModels, wireSettings, wireWizard,
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
      <div id="view" style="display:flex;flex-direction:column;flex:1;min-height:0"></div>
      <div class="taskbar" id="taskbar"></div>
    </div>`;
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
