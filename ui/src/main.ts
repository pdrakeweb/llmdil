/* Local LLM Service Manager — UI shell (plain TypeScript, no framework).
   Hash router over 9 views; the Tauri command boundary stays in api.ts so a
   framework can replace this file later without changing command contracts. */

import "./styles.css";
import { getStatus, isTauri, listBackends, getSetupStatus } from "./api";
import { esc } from "./html";
import { icon, type IconName } from "./icons";
import { mountDashboard } from "./views/dashboard";
import { renderServiceMap } from "./views/serviceMap";
import {
  renderBackends, renderGateway, renderGpus, renderLogs,
  renderModels, renderSettings, renderWizard, wireWizard,
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
  const [s, b] = await Promise.all([getStatus(), listBackends()]);
  if (s.error || b.error) {
    el.innerHTML = `<div class="row"><span class="dot err"></span> backend unreachable</div>`;
    return;
  }
  const gw = b.data.find((x) => x.id === "gateway" || x.port === 4000) ?? null;
  const backs = gw ? b.data.filter((x) => x !== gw) : b.data;
  const up = backs.filter((x) => x.status === "ok").length;
  const gwOk = gw?.status === "ok";
  el.innerHTML = `
    <div class="row"><span class="dot ${gw ? (gwOk ? "ok" : "warn") : "idle"}"></span> LiteLLM :4000${gw ? "" : " · not reported"}</div>
    <div class="row"><span class="dot ${backs.length && up === backs.length ? "ok" : "warn"}"></span> ${up}/${backs.length} backends up</div>
    <div class="row"><span class="dot warn"></span> MXC · Learning-mode</div>`;
}

function renderTaskbar(): void {
  const el = document.getElementById("taskbar");
  if (!el) return;
  // No background-task feed yet — render idle instead of a fabricated download.
  el.innerHTML = `<span class="quiet">No background tasks</span>
    <span class="grow"></span><span class="quiet">Task drawer</span>`;
}

async function render(): Promise<void> {
  const gen = ++renderGen;
  if (cleanup) { cleanup(); cleanup = null; }
  const hash = location.hash || "#/dashboard";
  const route = ROUTES.find((r) => r.hash === hash) ?? ROUTES[0];
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
      break;
    case "#/gpus":
      view.innerHTML = renderGpus();
      break;
    case "#/backends":
      view.innerHTML = renderBackends();
      break;
    case "#/models":
      view.innerHTML = renderModels();
      break;
    case "#/logs":
      view.innerHTML = renderLogs();
      break;
    case "#/gateway":
      view.innerHTML = renderGateway();
      break;
    case "#/settings":
      view.innerHTML = renderSettings();
      break;
    case "#/wizard":
      view.innerHTML = renderWizard();
      wireWizard(view);
      break;
    default:
      view.innerHTML = "";
  }
  if (gen !== renderGen) return; // a newer render took over mid-flight
  renderTaskbar();
  void renderSideFoot();
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
