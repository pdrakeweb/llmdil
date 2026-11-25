/* Start All Services: the sidebar block shown on every page while
   something that should be running is not. It belongs to the shell, so
   a start keeps going when the operator changes page. */

import { esc, setHtml } from "./html";
import { icon } from "./icons";
import { runLifecycle, serviceStore, type LifecycleState } from "./lifecycle";
import type { ServiceSnapshot } from "./lifecycle";

interface StartableBackend { id: string; enabled?: boolean; configured?: boolean }

/** What Start All would start right now: enabled, configured backends
 *  that are stopped or failed (never disabled ones, never ones in
 *  transition), and the gateway when it is stopped or failed — never
 *  while it is restarting/starting (UI-2). An unmeasured gateway
 *  (status call failed) is not a target: its state is unknown. */
export function startAllTargets<B extends StartableBackend>(
  backends: readonly B[],
  stateOf: (b: B) => LifecycleState,
  gateway: LifecycleState | null,
): { backends: string[]; gateway: boolean } {
  const down = (s: LifecycleState): boolean => s === "stopped" || s === "failed";
  return {
    backends: backends.filter((b) => b.enabled !== false && b.configured !== false && down(stateOf(b))).map((b) => b.id),
    gateway: gateway !== null && down(gateway),
  };
}

/** The block: the count, the one button, and the failures of the last
 *  run. Empty when everything that should run does. */
export function startAllHtml(v: { running: number; total: number; stopped: boolean; busy: boolean; error: string | null }): string {
  if (!v.stopped && !v.busy && !v.error) return "";
  const count = `Services running: ${v.running} of ${v.total}`;
  return `<div class="side-services">
    ${v.stopped || v.busy ? `<div class="count">${count}</div>
    <button class="btn primary" data-start-all title="${count}" aria-label="Start All Services. ${count}"${v.busy ? " disabled" : ""}>${icon(v.busy ? "clock" : "play")}<span class="lbl">${v.busy ? "Starting…" : "Start All Services"}</span></button>` : ""}
    ${v.error ? `<div class="err" role="alert" title="${esc(v.error)}">${esc(v.error)}</div>
    <button class="btn" data-start-all-dismiss title="${esc(v.error)}" aria-label="Dismiss the Start All error">${icon("x")}<span class="lbl">Dismiss</span></button>` : ""}
  </div>`;
}

let busy = false;
let error: string | null = null;

function targets(snap: ServiceSnapshot): { backends: string[]; gateway: boolean } {
  return startAllTargets(snap.backends, (b) => serviceStore.backendState(b), serviceStore.gatewayState());
}

export function paintStartAll(snap: ServiceSnapshot): void {
  const el = document.getElementById("sideServices");
  if (!el) return;
  // Sample data, or nothing read yet: nothing to start.
  if (!snap.live || snap.updatedAt === 0) { setHtml(el, ""); return; }
  const t = targets(snap);
  const sum = serviceStore.summary();
  setHtml(el, startAllHtml({
    running: sum.backendsRunning + (sum.gateway === "running" ? 1 : 0),
    total: sum.backendsTotal + (sum.gateway === null ? 0 : 1),
    stopped: t.backends.length > 0 || t.gateway,
    busy,
    error,
  }));
}

/** Start everything that should be running and is not, through the
 *  shared lifecycle runner (the same entry points every surface uses:
 *  start_backend per backend — routed to the SCM in service mode —
 *  and the start-gateway step for the gateway, after the MCP
 *  credential prompt). Each start shows as "starting…" on every
 *  surface at once. Sequential, so one failure does not block the
 *  rest; failures are collected and reported together at the end. */
async function startAllServices(): Promise<void> {
  if (busy) return;
  busy = true;
  error = null;
  const t = targets(serviceStore.get());
  paintStartAll(serviceStore.get());
  const errors: string[] = [];
  for (const id of t.backends) {
    const r = await runLifecycle({ kind: "backend", id }, "start", { origin: "ui" });
    if (!r.ok) errors.push(r.message);
  }
  if (t.gateway) {
    // MCP credential prompt (Pete, 2026-10-18): same pre-start step
    // as the Gateway view's Start button. Cancelling skips only the
    // gateway; the backends above already started.
    const r = await runLifecycle({ kind: "gateway" }, "start", { origin: "ui", promptMcp: true });
    if (r.cancelled) errors.push("gateway: start cancelled at the MCP credential prompt");
    else if (!r.ok) errors.push(r.message);
  }
  busy = false;
  error = errors.length ? `Start All failed — ${errors.join("; ")}` : null;
  paintStartAll(serviceStore.get());
}

export function onStartAllClick(ev: Event): void {
  const t = ev.target as HTMLElement;
  if (t.closest("[data-start-all]")) void startAllServices();
  else if (t.closest("[data-start-all-dismiss]")) {
    error = null;
    paintStartAll(serviceStore.get());
  }
}
