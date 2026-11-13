/* Dashboard view — fully functional.
   - Appliance: service cards, GPU strip, model placement, throughput chart.
   - Console: dense tables with sparklines/bars.
   - Topology: mini cluster node map (SVG).
   Lifecycle state comes from the shared service store (lifecycle.ts):
   every store refresh repaints the cards with the effective state (a
   restart from any surface reads "restarting…"), and getDashboard() —
   fed the store's backend/gateway rows — refreshes the telemetry and
   metrics at most every ~5 s. Pause live stops repainting; data older
   than 10s without a successful poll is labeled STALE (spec §4 global
   UI rules).
   Backend failures keep the last good model and show an error banner —
   never fabricated data presented as live. */

import { getDashboard, systemVerdict } from "../api";
import type { SystemVerdict } from "../api";
import {
  LIFECYCLE_COLORS, META, lifecycleChip, lifecycleColor, lifecycleDot, lifecycleEdge, lifecycleLabel,
  renderLifecycleActions, runLifecycle, serviceStore, summarize, toStatus, wireLifecycleActions,
} from "../lifecycle";
import type { LifecycleState, ServiceSnapshot } from "../lifecycle";
import { esc } from "../html";
import { fmtCount, fmtGb, fmtPct, gaugeRing, meter, sparkline, stackedBar, trim1 } from "../charts";
import { icon } from "../icons";
import type { BackendInfo, DashboardModel, GpuInfo, PlacementSegment, Status } from "../types";

type ViewMode = "appliance" | "console" | "topology";

const VIEW_KEY = "llm.view.dashboard";
let mode: ViewMode = (localStorage.getItem(VIEW_KEY) as ViewMode) || "appliance";
if (mode !== "appliance" && mode !== "console" && mode !== "topology") mode = "appliance";

let paused = false;
let lastOk = 0;
let lastModel: DashboardModel | null = null;
let lastLive = false;
let lastError: string | null = null;
/** Set when a card's lifecycle action fails; shown in the header
 *  notice area and cleared by the next successful poll. */
let restartError: string | null = null;
/** Store subscription and the delegated lifecycle-button handler of
 *  the current mount. */
let unsubscribeStore: (() => void) | null = null;
let unwireActions: (() => void) | null = null;
/** Date.now() when the last full getDashboard() fetch started, and
 *  whether one is in flight (store refreshes in between only repaint
 *  the lifecycle overlay). */
let lastFetchAt = 0;
let fetching = false;
const FULL_FETCH_MS = 4500;
/** Start All in flight: the bar stays rendered with a busy button so
 *  the action's state is visible while starts complete one by one. */
let startAllBusy = false;
/** Failures from the last Start All run; shown in the header notice
 *  area and cleared by the next successful poll. */
let startAllError: string | null = null;
/** Gateway-probe failure from the latest successful poll. The probe
 *  failing does not fail the poll (backends/telemetry stay fresh), so
 *  it gets its own banner line instead of the backend-error one. */
let lastGatewayError: string | null = null;
let tickTimer: number | null = null;
/** Set by the mount's teardown: an in-flight poll that resolves after
 *  navigation must not render into the next route's #view. */
let disposed = false;
/** Last computed stale-boundary state, so the 1s ticker re-renders
 *  exactly when the data crosses into (or out of) stale. */
let staleShown = false;

const C = { blue: "#4a9eff", purple: "#a371f7", teal: "#39c5cf", green: "#3fb950", amber: "#d29922", red: "#f85149" };

function setMode(m: ViewMode): void {
  mode = m;
  try { localStorage.setItem(VIEW_KEY, m); } catch { /* storage unavailable */ }
}

/** GPU health dot (telemetry health, NOT lifecycle: a GPU is ok,
 *  degraded, or down). Services render through lifecycleDot. */
function gpuDot(s: Status): string {
  const cls = s === "ok" ? "ok" : s === "down" ? "err" : s === "idle" ? "idle" : "warn";
  return `<span class="dot ${cls}"></span>`;
}

/** GPU health color for the topology nodes (same palette). */
function gpuColor2(s: Status): string {
  return s === "ok" ? LIFECYCLE_COLORS.green : s === "down" ? LIFECYCLE_COLORS.red : s === "idle" ? LIFECYCLE_COLORS.grey : LIFECYCLE_COLORS.amber;
}

/** A Status that means a live (or coming-up) process: placements and
 *  topology edges are drawn only for these. */
function statusLive(s: Status | undefined): boolean {
  return s === "ok" || s === "starting" || s === "restarting" || s === "degraded";
}

/** The model with the shared store's effective lifecycle state applied
 *  to every service card, placement row and the gateway: the store
 *  refreshes faster than the full dashboard fetch and carries the
 *  local pending overlay, so a click (or a restart elsewhere) shows
 *  the transition at once. */
function overlayModel(m: DashboardModel, snap: ServiceSnapshot): DashboardModel {
  if (!snap.live || snap.updatedAt === 0) return m;
  const rows = new Map(snap.backends.map((r) => [r.id, r]));
  const backends = m.backends.map((b) => {
    const r = rows.get(b.id);
    const key = `backend:${b.id}`;
    const st = r ? serviceStore.backendState(r) : serviceStore.effective(key, b.lifecycle);
    return {
      ...b,
      lifecycle: st,
      status: toStatus(st),
      transition: serviceStore.transitionOf(key, r ? r.transition ?? null : b.transition ?? null),
      enabled: r ? r.enabled : b.enabled,
      configured: r ? r.configured : b.configured,
    };
  });
  const gs = serviceStore.gatewayState();
  const gateway = m.gateway && gs
    ? { ...m.gateway, lifecycle: gs, status: toStatus(gs), transition: serviceStore.transitionOf("gateway", snap.gateway?.transition ?? null) }
    : m.gateway;
  const placement = m.placement.map((p) => {
    const b = p.backendId ? backends.find((x) => x.id === p.backendId) : undefined;
    return b ? { ...p, backendStatus: b.status } : p;
  });
  return { ...m, backends, gateway, placement, gatewayState: gs ?? m.gatewayState, gatewayUp: gs ? META[gs].isLive : m.gatewayUp };
}

/** GPU strip cards whose Trends expandable is open, by GPU id —
 *  module state so the 5 s poll repaint cannot collapse it. */
const openTrends = new Set<string>();

/** All services including the gateway when the backend reported one. */
function allServices(m: DashboardModel): BackendInfo[] {
  return m.gateway ? [m.gateway, ...m.backends] : [...m.backends];
}

/** Stable per-backend sparkline color: a hash of the backend id into
 *  the palette, so a card's color never changes with list order and
 *  is never keyed to a specific backend's name. */
function backendSparkColor(id: string): string {
  const palette = [C.blue, C.purple, C.teal, C.amber, C.green];
  let h = 0;
  for (let i = 0; i < id.length; i++) h = (h * 31 + id.charCodeAt(i)) >>> 0;
  return palette[h % palette.length];
}

function serviceCard(b: BackendInfo, isGateway: boolean): string {
  const ringColor = lifecycleColor(b.lifecycle);
  // requests_1h / errors_1h come from LiteLLM's Prometheus
  // counters via the app's metrics poller (live since v0.1.18);
  // throughput_toks is the poller's LIVE rate over the recent
  // window: the backend's own /slots rate when available, the
  // gateway live rate as fallback (absent when the current rate
  // is not measured); health is the
  // trailing-hour success share and
  // vram_held_gb is joined from telemetry attribution in
  // getDashboard. Render "—" when absent instead of crashing on
  // .toFixed or presenting a fabricated measurement.
  const m1k = isGateway ? "REQUESTS / 1H" : "THROUGHPUT";
  const m1v = isGateway
    ? (b.requests_1h !== undefined ? fmtCount(b.requests_1h) : "—")
    : (b.throughput_toks !== undefined ? trim1(b.throughput_toks) : "—");
  const m1u = isGateway || b.throughput_toks === undefined ? "" : " tok/s";
  const m2k = isGateway ? "ERRORS / 1H" : "VRAM HELD";
  const m2v = isGateway
    ? (b.errors_1h !== undefined ? fmtCount(b.errors_1h) : "—")
    : (b.vram_held_gb !== undefined ? fmtGb(b.vram_held_gb) : "—");
  const m2u = isGateway || b.vram_held_gb === undefined ? "" : " GB";
  // The ring's arc encodes a share, never a raw count. The gateway's
  // arc is its trailing-hour success share (1 - errors/requests)
  // with the integer percent centered; with no requests the share is
  // undefined and the ring is the neutral track. A backend's health
  // IS a percentage and drives its arc when present; when health is
  // absent the ring is omitted entirely and the rows take the width.
  let ring = "";
  if (isGateway) {
    const share = b.requests_1h !== undefined && b.requests_1h > 0
      ? 100 * (1 - (b.errors_1h ?? 0) / b.requests_1h)
      : undefined;
    ring = share === undefined
      ? gaugeRing(0, 64, "var(--line)", "—", "success / 1h")
      : gaugeRing(share, 64, ringColor, `${Math.round(share)}%`, "success / 1h");
  } else if (b.health !== undefined) {
    ring = gaugeRing(b.health, 64, ringColor, b.health.toFixed(0), "health");
  }
  // Sparkline captions render only when a series exists — live
  // services carry no series, so the whole strip is omitted rather
  // than captioning an empty chart.
  const sparkHtml = b.spark && b.spark.length
    ? `<div class="spark-wrap"><div class="cap">${isGateway ? "Requests per minute" : "Tokens/s"}</div>
      ${sparkline(b.spark, 220, 34, isGateway ? C.blue : backendSparkColor(b.id))}
    </div>`
    : "";
  // Start / Stop / Restart come from the shared lifecycle rule, for
  // the gateway card too (GW-11): a stopped backend offers Start (not
  // an unconditional Restart, UI-6), a transitioning one has its
  // buttons disabled with the reason. The gateway card keeps its link
  // to the Gateway view for everything else.
  const lc = isGateway
    ? renderLifecycleActions({ kind: "gateway" }, b.lifecycle)
    : renderLifecycleActions({ kind: "backend", id: b.id }, b.lifecycle, { enabled: b.enabled, configured: b.configured });
  const actions = isGateway
    ? `<div class="card-actions">${lc}<a class="btn icon-only" href="#/gateway" title="Open Gateway view" aria-label="Open Gateway view">${icon("gateway")}</a>
      <a class="btn icon-only" href="#/logs?scope=gateway&follow=1" title="Logs" aria-label="Gateway logs">${icon("logs")}</a></div>`
    : `<div class="card-actions">${lc}
      <button class="btn icon-only" data-act="logs" data-id="${esc(b.id)}" title="Logs" aria-label="Logs for ${esc(b.id)}">${icon("logs")}</button>
    </div>`;
  return `<div class="card">
    <h3>${lifecycleDot(b.lifecycle)} ${esc(b.name)} <span class="port">:${b.port}</span> ${lifecycleChip(b.lifecycle, { transition: b.transition ?? null })}</h3>
    <div class="svc-top">
      ${ring}
      <div class="kv">
        <span class="k">${m1k}</span><span class="v">${esc(m1v)}<small>${m1u}</small></span>
        <span class="k" style="margin-top:4px">${m2k}</span><span class="v">${esc(m2v)}<small>${m2u}</small></span>
      </div>
    </div>
    ${sparkHtml}
    ${actions}
  </div>`;
}

function gpuStripCard(g: GpuInfo): string {
  // The lead visual is the attributed VRAM bar: one segment per
  // resident process, sized by its measured share of the GPU's total
  // VRAM, with any unattributed remainder as a neutral "other"
  // segment and free space as the track. A process whose VRAM was
  // not measured (WDDM) collapses the whole bar to one neutral
  // segment — a breakdown the payload does not carry is never drawn.
  const totalMib = g.vram_total_gb * 1024;
  const procs = g.processes ?? [];
  const procLabel = (p: { process: string; backend: string }): string => (p.backend !== "—" ? p.backend : p.process);
  let vramHtml: string;
  if (!(g.vram_total_gb > 0)) {
    vramHtml = `<div class="mrow"><span class="mlabel">VRAM</span><span class="mval">not measured</span></div>`;
  } else {
    const usedPct = (g.vram_used_gb / g.vram_total_gb) * 100;
    const vramVal = `${fmtGb(g.vram_used_gb)}/${fmtGb(g.vram_total_gb)} GB · ${trim1(usedPct)}%`;
    let segments: { pct: number; color: string; label?: string }[];
    let keyHtml = "";
    if (procs.length && procs.some((p) => p.vram_mib === null)) {
      const unmeasured = procs.filter((p) => p.vram_mib === null).map(procLabel).join(", ");
      segments = [{ pct: usedPct, color: "#6d7686", label: `VRAM used — per-process breakdown not measured (not measured: ${unmeasured})` }];
    } else if (procs.length) {
      const palette = [C.blue, C.purple, C.teal, C.amber, C.green];
      segments = procs.map((p, i) => ({
        pct: ((p.vram_mib ?? 0) / totalMib) * 100,
        color: palette[i % palette.length],
        label: `${procLabel(p)} — ${fmtGb((p.vram_mib ?? 0) / 1024)} GB`,
      }));
      const attributed = procs.reduce((a, p) => a + (p.vram_mib ?? 0), 0);
      const otherMib = g.vram_used_gb * 1024 - attributed;
      if (otherMib > 0) {
        segments.push({ pct: (otherMib / totalMib) * 100, color: "#6d7686", label: `other — ${fmtGb(otherMib / 1024)} GB` });
      }
      keyHtml = `<div class="legend" style="margin-top:4px">${segments.map((s) => `<span><span class="sw" style="background:${s.color}"></span>${esc((s.label ?? "").split(" — ")[0])}</span>`).join("")}</div>`;
    } else {
      segments = [{ pct: usedPct, color: C.purple, label: `VRAM used — ${fmtGb(g.vram_used_gb)} GB` }];
    }
    vramHtml = `<div class="mrow"><span class="mlabel">VRAM</span><span class="mval">${vramVal}</span></div>
      <div class="mrow"><span></span><span>${stackedBar(segments, 220, 8)}</span></div>
      ${keyHtml ? `<div class="mrow"><span></span><span>${keyHtml}</span></div>` : ""}`;
  }
  // Peak temperature rides the value's tooltip, never inline text.
  const peakTip = g.temp_peak_c !== undefined ? ` title="Peak ${trim1(g.temp_peak_c)} °C"` : "";
  // Power: a thin meter of draw against the limit when both exist;
  // draw alone renders as a value only; power the driver reports as
  // N/A is "not measured" (never "N/A") and gets no sparkline
  // either. An absence the payload does not classify as the driver
  // state (a missing field — a bug state) renders as an em dash: it
  // must not borrow the hardware state's wording.
  const powerMeter = g.power_w != null && g.power_limit_w !== undefined && g.power_limit_w > 0
    ? `<div class="mrow"><span></span>${meter((g.power_w / g.power_limit_w) * 100, 220, 6, C.green)}</div>` : "";
  const powerVal = g.power_w == null
    ? g.power_absence === "driver_not_available" ? "not measured" : "—"
    : g.power_limit_w !== undefined ? `${trim1(g.power_w)}/${trim1(g.power_limit_w)} W` : `${trim1(g.power_w)} W`;
  // Sparklines collapse into one labeled Trends expandable (each
  // series keyed by its metric name); open state is module-level so
  // the poll repaint preserves it.
  const trendRows: string[] = [];
  if (g.util_spark && g.util_spark.length) trendRows.push(`<div class="mrow"><span class="mlabel">Utilization</span>${sparkline(g.util_spark, 220, 26, C.blue)}</div>`);
  if (g.temp_spark && g.temp_spark.length) trendRows.push(`<div class="mrow"><span class="mlabel">Temperature</span>${sparkline(g.temp_spark, 220, 26, C.amber)}</div>`);
  if (g.power_w != null && g.power_spark && g.power_spark.length) trendRows.push(`<div class="mrow"><span class="mlabel">Power</span>${sparkline(g.power_spark, 220, 26, C.green)}</div>`);
  const trendsOpen = openTrends.has(g.id);
  const trends = trendRows.length
    ? `<div style="margin-top:8px"><button class="btn" data-trends="${esc(g.id)}" aria-expanded="${trendsOpen}" title="Trends" aria-label="Trends" style="padding:2px 8px;font-size:11px">${icon(trendsOpen ? "chevD" : "chevR")}</button>
      ${trendsOpen ? `<div class="gpu-metrics" style="margin-top:6px">${trendRows.join("")}</div>` : ""}</div>`
    : "";
  return `<div class="card">
    <div class="gpu-head">${gpuDot(g.status)} ${icon("gpu")} ${esc(g.name)}</div>
    <div class="gpu-metrics">
      ${vramHtml}
      <div class="mrow"><span class="mlabel">Utilization</span><span class="mval">${trim1(g.util_pct)}%</span></div>
      <div class="mrow"><span class="mlabel">Temperature</span><span class="mval"${peakTip}>${trim1(g.temp_c)} °C</span></div>
      <div class="mrow"><span class="mlabel">Power</span><span class="mval">${powerVal}</span></div>
      ${powerMeter}
    </div>
    ${trends}
  </div>`;
}

/** Fixed palette for placement segments, indexed by GPU POSITION (the
 *  GPU's place in the model's GPU list) — never keyed by GPU id/name,
 *  so live segments (labeled with telemetry names) get the same stable
 *  colors as sample ones. */
const GPU_PALETTE = ["#4a9eff", "#a371f7", "#39c5cf", "#d29922", "#3fb950", "#f778ba", "#79b8ff", "#d2a8ff"];

/** The GPU-list position a placement segment belongs to: its recorded
 *  index when present, else matched against the GPU list by name or
 *  id, else the segment's own position within its row. */
function segmentGpuPos(seg: PlacementSegment, rowPos: number, gpus: GpuInfo[]): number {
  if (seg.gpuIndex !== undefined && gpus[seg.gpuIndex]) return seg.gpuIndex;
  const j = gpus.findIndex((g) => g.name === seg.gpu || g.id === seg.gpu);
  return j >= 0 ? j : rowPos;
}

function gpuColor(pos: number): string {
  return GPU_PALETTE[pos % GPU_PALETTE.length];
}

function applianceView(m: DashboardModel, live: boolean): string {
  const svcs = allServices(m);
  const cards = svcs.map((b, i) => serviceCard(b, i === 0 && m.gateway !== null)).join("");
  const gpus = m.gpus.map(gpuStripCard).join("");
  // Placement rows are per BACKEND (placements belong to backends,
  // not models). Eventual treatment keys off actuality: a disabled
  // backend's placement, and an enabled-but-not-running backend's,
  // are both not in effect — gray segments, reduced opacity, an
  // "eventual" tag. Bar widths and tooltip percentages both come
  // from the segment's unrounded fraction, so the chart and its
  // stated values are the same number; sub-1% shares render as a
  // minimum-width sliver with the true value in the tooltip. A
  // defaulted row (no split configured) renders ghosted and stays
  // out of the legend — a default is not a configuration.
  const EVENTUAL_TIP = "Placement takes effect when the backend is enabled and started.";
  const placeHtml = m.placement.map((p) => {
    const disabled = p.backendEnabled === false;
    const stopped = !disabled && p.backendStatus !== undefined && !statusLive(p.backendStatus);
    const eventual = disabled || stopped;
    const label = p.backendId ?? p.model;
    const tag = disabled
      ? ` <span class="eventual-tag" title="${EVENTUAL_TIP}">disabled · eventual</span>`
      : stopped
        ? ` <span class="eventual-tag" title="${EVENTUAL_TIP}">stopped · eventual</span>`
        : "";
    const barTip = p.defaulted ? ` title="No split configured — an unsplit launch loads on GPU 0."` : "";
    const segs = p.segments.map((s, k) => ({
      pct: Math.max(s.frac * 100, s.frac > 0 ? 2 : 0),
      color: eventual || p.defaulted ? "#6d7686" : gpuColor(segmentGpuPos(s, k, m.gpus)),
      label: `${s.gpu} ${fmtPct(s.frac)}%`,
    }));
    return `<div class="place-row${eventual ? " eventual" : ""}"${p.defaulted && !eventual ? ` style="opacity:.55"` : ""}><span class="mname">${esc(label)}${p.backendId ? ` <span class="place-model">${esc(p.model)}</span>` : ""}${tag}</span>
      <div${barTip}>${stackedBar(segs)}</div></div>`;
  }).join("");
  // The legend is built from the RUNNING rows actually rendered:
  // each distinct segment label with the palette color its GPU
  // position produced. Eventual and defaulted rows render gray or
  // ghosted and stay out of the legend — their colors assert nothing.
  const legendByLabel = new Map<string, string>();
  for (const p of m.placement) {
    if (p.backendEnabled === false || p.defaulted) continue;
    if (p.backendStatus !== undefined && !statusLive(p.backendStatus)) continue;
    p.segments.forEach((s, k) => {
      if (!legendByLabel.has(s.gpu)) legendByLabel.set(s.gpu, gpuColor(segmentGpuPos(s, k, m.gpus)));
    });
  }
  const legend = [...legendByLabel.entries()]
    .map(([label, color]) => `<span><span class="sw" style="background:${color}"></span>${esc(label)}</span>`).join("");
  const legendHtml = legend ? `<div class="legend">${legend}</div>` : "";
  // No placements -> the card is omitted entirely (principle 13);
  // the page-level empty card below covers the no-backends case.
  const placeCard = m.placement.length
    ? `<div class="card" style="margin-top:12px"><div class="sec-title">${icon("layers")} Backend placements</div>${placeHtml}${legendHtml}</div>`
    : "";
  const empty = !svcs.length && !m.gpus.length
    ? `<div class="card"><div class="empty">No backends or GPUs reported. Run the setup wizard to install and configure the stack.</div></div>` : "";
  // Blank stat fields were reported as a defect (Oct 15, 2026): when
  // no metrics window exists, state why instead of rendering dashes.
  // Only while the gateway is UP: the gateway card alone renders
  // whenever the status probe answered (m.gateway non-null covers
  // down too), and a down gateway has no statistics to wait for —
  // its card already says so.
  const noWindow = live && m.gateway !== null && m.gatewayState === "running" && svcs.every((b) => b.requests_1h === undefined)
    ? `<div class="card" style="margin-bottom:12px;font-size:12px;color:var(--text2)">${icon("warn")} Gateway statistics are not available yet. They appear after the gateway has been running about 30 s with the app polling it (the generated config enables its metrics at gateway start).</div>`
    : "";
  return `${empty}
    ${noWindow}
    <div class="group-box"><div class="sec-title">${icon("gpu")} GPUs</div><div class="cards3">${gpus}</div></div>
    <div class="group-box"><div class="sec-title">${icon("server")} Services</div><div class="cards5">${cards}</div></div>
    ${placeCard}`;
}

function consoleView(m: DashboardModel): string {
  const svcRows = allServices(m).map((b) => {
    // health/throughput/requests ride the metrics poller; show "—"
    // when absent (the old expression printed "undefined req/h"
    // and could not render). The throughput test must be
    // presence, not truthiness: a measured 0 tok/s is falsy in
    // JS, and the old test fell through to the req/h display
    // instead of rendering "0 tok/s".
    const rate = b.throughput_toks !== undefined
      ? `${trim1(b.throughput_toks)} tok/s`
      : b.requests_1h !== undefined ? `${fmtCount(b.requests_1h)} req/h` : "—";
    return `<tr>
    <td>${lifecycleDot(b.lifecycle)} ${esc(b.name)} ${lifecycleChip(b.lifecycle, { transition: b.transition ?? null })}</td><td>:${b.port}</td><td>${b.model ? esc(b.model) : "—"}</td>
    <td>${b.health == null ? "—" : trim1(b.health)}</td><td>${rate}</td>
    <td>${b.spark && b.spark.length ? sparkline(b.spark, 120, 20, C.blue) : "—"}</td></tr>`;
  }).join("");
  const gpuRows = m.gpus.map((g) => {
    const vramPct = Math.round((g.vram_used_gb / g.vram_total_gb) * 100);
    return `<tr><td>${gpuDot(g.status)} ${esc(g.name)}</td><td>${trim1(g.util_pct)}%</td>
      <td>${meter(g.util_pct, 120, 8, C.blue)}</td>
      <td>${fmtGb(g.vram_used_gb)}/${fmtGb(g.vram_total_gb)} GB</td>
      <td>${meter(vramPct, 120, 8, C.purple)}</td>
      <td>${trim1(g.temp_c)}°C</td><td>${g.power_w == null ? (g.power_absence === "driver_not_available" ? "not measured" : "—") : `${trim1(g.power_w)} W`}</td></tr>`;
  }).join("");
  return `<div class="card" style="margin-bottom:12px"><div class="sec-title">Services</div>
    <table class="tbl"><thead><tr><th>Service</th><th>Port</th><th>Model</th><th>Health</th><th>Rate</th><th>Trend</th></tr></thead>
    <tbody>${svcRows || `<tr><td colspan="6" class="empty">no services reported</td></tr>`}</tbody></table></div>
    <div class="card"><div class="sec-title">GPUs</div>
    <table class="tbl"><thead><tr><th>GPU</th><th>Util</th><th></th><th>VRAM</th><th></th><th>Temp</th><th>Power</th></tr></thead>
    <tbody>${gpuRows || `<tr><td colspan="7" class="empty">no GPUs reported</td></tr>`}</tbody></table></div>`;
}

function topologyView(m: DashboardModel): string {
  // Mini node map: LiteLLM -> backends -> GPUs -> models (SVG). No
  // client nodes are drawn — the app has no data about which clients
  // connect. Honest empty state when the backend reports nothing —
  // never fake nodes.
  if (!m.backends.length && !m.gpus.length && !m.gateway) {
    return `<div class="card"><div class="empty">No topology to draw — no backends or GPUs reported. Run the setup wizard to install and configure the stack.</div></div>`;
  }
  const W = 980, H = 380;
  const colX = [50, 290, 530, 770];
  const nodeW = 130, nodeH = 56;
  const backs = m.backends;
  const gpus = m.gpus;
  const models = m.placement;

  /** Edge class follows the backend's lifecycle state (the shared
   *  lifecycleEdge rule, as on the Service Map). */
  const edgeCls = (s: LifecycleState): string => lifecycleEdge(s).cls;
  /** A backend is linked in the topology only while it is live
   *  (code-review 5.3): an edge asserts a live connection, and a
   *  stopped or disabled backend has none. Its NODE still draws —
   *  the map shows what is configured — but unconnected. */
  const running = (s: Status | undefined): boolean => statusLive(s);

  const cy = (i: number, n: number) => 60 + (i * (H - 120)) / Math.max(1, n - 1 || 1);
  let svg = `<svg width="${W}" height="${H}" viewBox="0 0 ${W} ${H}" style="display:block;width:100%" role="img" aria-label="Cluster topology mini map">`;
  const edge = (x1: number, y1: number, x2: number, y2: number, label: string, cls = "") => {
    const mx = (x1 + x2) / 2;
    svg += `<path class="map-edge ${cls}" d="M${x1},${y1} C${mx},${y1} ${mx},${y2} ${x2},${y2}"/>`;
    if (label) svg += `<text class="map-edge-label" x="${mx}" y="${(y1 + y2) / 2 - 4}" text-anchor="middle">${esc(label)}</text>`;
  };
  const node = (x: number, y: number, title: string, sub: string, dot: string) => {
    svg += `<g><rect class="map-node" x="${x}" y="${y - nodeH / 2}" width="${nodeW}" height="${nodeH}" rx="8"/>
      <circle cx="${x + 14}" cy="${y - 8}" r="4" fill="${dot}"/>
      <text class="map-node-text" x="${x + 26}" y="${y - 2}">${esc(title)}</text>
      <text class="map-node-sub" x="${x + 14}" y="${y + 16}">${esc(sub)}</text></g>`;
  };

  node(colX[0], H / 2, "LiteLLM",
    m.gateway
      ? `:${m.gateway.port}${m.gateway.lifecycle !== "running" ? ` \u00b7 ${lifecycleLabel(m.gateway.lifecycle)}` : m.gatewayStrategy ? ` \u00b7 ${m.gatewayStrategy}` : ""}`
      : "not reported",
    m.gateway ? lifecycleColor(m.gateway.lifecycle) : LIFECYCLE_COLORS.grey);
  backs.forEach((b, i) => {
    const y = cy(i, backs.length);
    const sub = b.lifecycle !== "running"
      ? ` \u00b7 ${lifecycleLabel(b.lifecycle)}`
      : b.throughput_toks !== undefined ? ` \u00b7 ${trim1(b.throughput_toks)} tok/s` : "";
    node(colX[1], y, b.name, `:${b.port}${sub}`, lifecycleColor(b.lifecycle));
    if (running(b.status)) {
      edge(colX[0] + nodeW, H / 2, colX[1], y, "", edgeCls(b.lifecycle));
    }
  });
  gpus.forEach((g, i) => {
    node(colX[2], cy(i, gpus.length), g.name, `${trim1(g.util_pct)}% util`, gpuColor2(g.status));
  });
  // Backend -> GPU connectivity comes from the configured tensor
  // splits (the placement rows' segments), exactly like the full
  // Service Map — never a positional full mesh asserting links the
  // configuration does not have. A segment that cannot be matched to
  // a drawn GPU node draws no edge.
  const backendIdxById = new Map(backs.map((b, i) => [b.id, i] as const));
  const rowStatus = (p: (typeof models)[number]): Status | undefined =>
    p.backendStatus ?? (p.backendId ? backs.find((b) => b.id === p.backendId)?.status : undefined);
  const segGpuIdx = (seg: PlacementSegment): number => {
    if (seg.gpuIndex !== undefined && gpus[seg.gpuIndex]) return seg.gpuIndex;
    return gpus.findIndex((g) => g.name === seg.gpu || g.id === seg.gpu);
  };
  // Placement edges assert a configured split actually loaded on
  // the GPUs, so they draw only for rows that are real placements
  // of live backends (code-review 5.3): a `defaulted` row's single
  // 100% segment is the no-split fallback, not a configuration; a
  // disabled backend's placement is eventual; a stopped backend
  // holds nothing. The Appliance view keeps every row (grayed /
  // ghosted per those flags) — the Topology filters to what is
  // actually connected. Model nodes follow the same filter.
  const linked = (p: (typeof models)[number]): boolean =>
    !p.defaulted && p.backendEnabled !== false && running(rowStatus(p));
  for (const p of models) {
    if (!p.backendId || !linked(p)) continue;
    const bi = backendIdxById.get(p.backendId);
    if (bi === undefined) continue;
    p.segments.forEach((seg) => {
      const gj = segGpuIdx(seg);
      if (gj < 0) return;
      edge(colX[1] + nodeW, cy(bi, backs.length), colX[2], cy(gj, gpus.length), "", edgeCls(backs[bi].lifecycle));
    });
  }
  // Model nodes key to their owning backend's measured state: the dot
  // is the backend's status color, and the sublabel is the split
  // summary — configuration alone never asserts a model is "loaded".
  const linkedModels = models.filter(linked);
  linkedModels.slice(0, 4).forEach((p, i) => {
    const y = cy(i, Math.min(4, linkedModels.length));
    const owner = p.backendId ? backs.find((b) => b.id === p.backendId) : undefined;
    const sub = p.segments.length ? p.segments.map((s) => `${s.pct}%`).join(" \u00b7 ") : "\u2014";
    node(colX[3], y, p.model, sub, owner ? lifecycleColor(owner.lifecycle) : LIFECYCLE_COLORS.grey);
    p.segments.forEach((seg) => {
      const gj = segGpuIdx(seg);
      if (gj < 0) return;
      edge(colX[2] + nodeW, cy(gj, gpus.length), colX[3], y, "");
    });
  });
  svg += `<text class="map-col-label" x="${colX[0]}" y="24">GATEWAY</text>
    <text class="map-col-label" x="${colX[1]}" y="24">BACKENDS</text>
    <text class="map-col-label" x="${colX[2]}" y="24">GPUS</text>
    <text class="map-col-label" x="${colX[3]}" y="24">MODELS</text></svg>`;
  return `<div class="card"><div class="sec-title">${icon("map")} Topology — same data, node view</div>
    <div class="map-canvas" style="border:0;padding:0">${svg}</div>
    <div class="legend">${lifecycleLegend()}
    <a href="#/map" style="color:var(--accent)">Open full service map \u2192</a></div></div>`;
}

/** The service-state legend, generated from the shared META (one
 *  swatch per color class, labeled with the states it covers). */
function lifecycleLegend(): string {
  const groups: { color: string; states: LifecycleState[] }[] = [];
  for (const s of ["running", "starting", "restarting", "stopping", "degraded", "failed", "stopped"] as LifecycleState[]) {
    const color = META[s].color;
    const g = groups.find((x) => x.color === color);
    if (g) g.states.push(s); else groups.push({ color, states: [s] });
  }
  return groups.map((g) => `<span><span class="sw" style="background:${g.color}"></span>${esc(g.states.map((s) => META[s].label).join(" / "))}</span>`).join("");
}

/** What Start All would start right now: enabled, configured backends
 *  that are stopped or failed (never disabled ones, never ones in
 *  transition), and the gateway when it is stopped or failed — never
 *  while it is restarting/starting (UI-2). An unmeasured gateway
 *  (status call failed) is not a target: its state is unknown. */
function startAllTargets(m: DashboardModel): { backends: BackendInfo[]; gatewayDown: boolean } {
  const backends = m.backends.filter(
    (b) => b.enabled === true && b.configured !== false && (b.lifecycle === "stopped" || b.lifecycle === "failed"),
  );
  const gatewayDown = m.gateway !== null && (m.gatewayState === "stopped" || m.gatewayState === "failed");
  return { backends, gatewayDown };
}

/** The Start All bar under the topbar: rendered only in live mode
 *  when something that should be running is not (or while a start is
 *  in flight). Count label + one button; no prose. */
function startAllBar(m: DashboardModel): string {
  if (!lastLive) return "";
  const { backends, gatewayDown } = startAllTargets(m);
  if (!startAllBusy && backends.length === 0 && !gatewayDown) return "";
  // The shared count rule (enabled + configured backends) plus the gateway.
  const sum = summarize({ backends: m.backends, gateway: m.gatewayState });
  const running = sum.backendsRunning + (m.gatewayState === "running" ? 1 : 0);
  const total = sum.backendsTotal + (m.gateway ? 1 : 0);
  return `<div style="padding:12px 20px 0"><div class="banner" style="display:flex;align-items:center;gap:10px"><span>Services running: ${running} of ${total}</span><span class="spacer"></span><button class="btn primary" id="startAllBtn" ${startAllBusy ? "disabled" : ""}>${icon("play")} ${startAllBusy ? "Starting…" : "Start All Services"}</button></div></div>`;
}

function headerHtml(live: boolean, stale: boolean, updatedAgo: string, verdict: SystemVerdict, startAll = ""): string {
  return `<div class="topbar">
    <h1>Dashboard</h1>
    <span class="status-pill"><span class="dot ${verdict.dot}"></span> ${esc(verdict.text)}</span>
    <span class="viewsel-label">View:</span>
    <span class="viewsel" role="tablist">
      <button data-view="appliance" class="${mode === "appliance" ? "active" : ""}">Appliance</button>
      <button data-view="console" class="${mode === "console" ? "active" : ""}">Console</button>
      <button data-view="topology" class="${mode === "topology" ? "active" : ""}">Topology</button>
    </span>
    <span class="spacer"></span>
    ${stale ? `<span class="stale-tag">STALE</span>` : ""}
    <span class="updated" id="updatedAgo">Last updated ${esc(updatedAgo)}</span>
    <button class="btn icon-only" id="pauseBtn" title="${paused ? "Resume live" : "Pause live"}" aria-label="${paused ? "Resume live" : "Pause live"}">${icon(paused ? "play" : "pause")}</button>
    <a class="btn" href="#/wizard">Setup wizard</a>
  </div>
  ${startAll}
  ${lastError ? `<div style="padding:12px 20px 0"><div class="banner">${icon("warn")} backend error — showing last known data: ${esc(lastError)}</div></div>` : ""}
  ${lastGatewayError ? `<div style="padding:12px 20px 0"><div class="banner">${icon("warn")} Gateway status unavailable: ${esc(lastGatewayError)}</div></div>` : ""}
  ${restartError ? `<div style="padding:12px 20px 0"><div class="banner" id="restartNotice">${icon("warn")} ${esc(restartError)}</div></div>` : ""}
  ${startAllError ? `<div style="padding:12px 20px 0"><div class="banner" id="startAllNotice">${icon("warn")} ${esc(startAllError)}</div></div>` : ""}
  ${!live ? `<div style="padding:12px 20px 0"><div class="banner">${icon("warn")} disconnected — showing sample data (not running under Tauri)</div></div>` : ""}`;
}

function ago(ts: number): string {
  const s = Math.max(0, Math.round((Date.now() - ts) / 1000));
  return s < 2 ? "just now" : `${s}s ago`;
}

function render(): void {
  // Never paint into another route's #view: a poll that resolves after
  // navigation (or a click handler from a torn-down mount) stops here.
  const route = location.hash.split("?")[0];
  if (route && route !== "#/dashboard") return;
  const el = document.getElementById("view");
  if (!el) return;
  if (!lastModel) {
    // No data yet — either still loading or the first poll failed.
    el.innerHTML = lastError
      ? headerHtml(lastLive, true, "never", systemVerdict({ summary: summarize({ backends: [], gateway: null }), stale: true, error: lastError })) + `<div class="view"><div class="card"><div class="empty">Backend error: ${esc(lastError)} — retrying…</div></div></div>`
      : "";
    return;
  }
  const m = overlayModel(lastModel, serviceStore.get());
  // Staleness is a function of the last successful poll's age, so it
  // applies while polling is paused too — not only on a failed poll.
  const staleNow = m.stale || (lastOk > 0 && Date.now() - lastOk > 10_000);
  staleShown = staleNow;
  // The shared count rule (enabled + configured backends) and the
  // gateway lifecycle state: a restart reads as a transition, never
  // as "Gateway down".
  const verdict = systemVerdict({
    summary: summarize({ backends: m.backends, gateway: m.gatewayState }),
    stale: staleNow,
    error: lastError,
  });
  const body = mode === "appliance" ? applianceView(m, lastLive) : mode === "console" ? consoleView(m) : topologyView(m);
  el.innerHTML = headerHtml(lastLive, staleNow, ago(m.updatedAt), verdict, startAllBar(m)) + `<div class="view">${body}</div>`;
  el.querySelectorAll("[data-view]").forEach((b) =>
    b.addEventListener("click", () => { setMode((b as HTMLElement).dataset.view as ViewMode); render(); }));
  el.querySelectorAll("[data-trends]").forEach((b) =>
    b.addEventListener("click", () => {
      const id = (b as HTMLElement).dataset.trends ?? "";
      if (openTrends.has(id)) openTrends.delete(id); else openTrends.add(id);
      render();
    }));
  const pb = el.querySelector("#pauseBtn");
  pb?.addEventListener("click", () => { paused = !paused; render(); });
  const sab = el.querySelector("#startAllBtn");
  sab?.addEventListener("click", () => { void startAllServices(); });
  el.querySelectorAll("[data-act]").forEach((b) =>
    b.addEventListener("click", () => {
      const act = (b as HTMLElement).dataset.act;
      const id = (b as HTMLElement).dataset.id;
      if (act === "logs" && id) location.hash = `#/logs?scope=${encodeURIComponent(id)}&follow=1`;
    }));
}

/** Start everything that should be running and is not, through the
 *  shared lifecycle runner (the same entry points every surface uses:
 *  start_backend per backend — routed to the SCM in service mode —
 *  and the start-gateway step for the gateway, after the MCP
 *  credential prompt). Each start shows as "starting…" on every
 *  surface at once. Sequential, so one failure does not block the
 *  rest; failures are collected and reported together at the end.
 *  The server clears the activation plan itself after a gateway
 *  start — no client-side activationComplete. */
async function startAllServices(): Promise<void> {
  if (startAllBusy || !lastModel) return;
  startAllBusy = true;
  render();
  const errors: string[] = [];
  const { backends, gatewayDown } = startAllTargets(overlayModel(lastModel, serviceStore.get()));
  for (const b of backends) {
    if (disposed) break;
    const r = await runLifecycle({ kind: "backend", id: b.id }, "start", { origin: "dashboard" });
    if (!r.ok) errors.push(r.message);
  }
  if (gatewayDown && !disposed) {
    // MCP credential prompt (Pete, 2026-10-18): same pre-start step
    // as the Gateway view's Start button. Cancelling skips only the
    // gateway leg; the backends above already started.
    const r = await runLifecycle({ kind: "gateway" }, "start", { origin: "dashboard", promptMcp: true });
    if (r.cancelled) errors.push("gateway: start cancelled at the MCP credential prompt");
    else if (!r.ok) errors.push(r.message);
  }
  startAllBusy = false;
  if (disposed) return;
  await doPoll(serviceStore.get()); // refresh first; a successful poll clears prior notices
  if (errors.length) {
    startAllError = `Start All failed — ${errors.join("; ")}`;
    render();
  }
}

/** The full dashboard fetch (telemetry, metrics, config), fed the
 *  shared store's backend and gateway rows so list_backends and
 *  gateway_status are not polled twice. */
async function doPoll(snap?: ServiceSnapshot): Promise<void> {
  if (fetching) return;
  fetching = true;
  lastFetchAt = Date.now();
  const prefetched = snap && snap.live && snap.updatedAt > 0
    ? {
      backends: { data: snap.backends, live: true, ...(snap.backendsError ? { error: snap.backendsError } : {}) },
      ...(snap.gateway ? { gateway: { data: snap.gateway, live: true } } : {}),
    }
    : undefined;
  let r: Awaited<ReturnType<typeof getDashboard>>;
  try {
    r = await getDashboard(prefetched); // never rejects; failures arrive as r.error
  } finally {
    fetching = false;
  }
  if (disposed) return; // navigated away while the poll was in flight
  if (r.error) {
    lastError = r.error;
    // Keep the last good model; mark stale when it ages out.
    if (lastModel && Date.now() - lastOk > 10_000) lastModel = { ...lastModel, stale: true };
  } else {
    lastModel = r.data;
    lastLive = r.live;
    lastError = null;
    restartError = null; // a successful poll clears any restart notice
    startAllError = null; // ...and any Start All notice
    // A gateway-probe failure rides along on an otherwise successful
    // poll: lastOk advances (the data IS fresh) and the failure shows
    // as its own banner, not as backend staleness.
    lastGatewayError = r.data.gatewayError ?? null;
    lastOk = Date.now();
  }
  render();
}

/** The lifecycle signature of a store snapshot: everything the overlay
 *  paints (each backend's effective state, transition, enabled and
 *  configured; the gateway's; the activation apply message). */
let lastLcSig = "";
function lcSignature(snap: ServiceSnapshot): string {
  const backends = snap.backends.map((r) => {
    const tr = serviceStore.transitionOf(`backend:${r.id}`, r.transition ?? null);
    return `${r.id}:${serviceStore.backendState(r)}:${tr?.kind ?? ""}:${tr?.origin ?? ""}:${r.enabled}:${r.configured}`;
  }).join("|");
  const gtr = serviceStore.transitionOf("gateway", snap.gateway?.transition ?? null);
  return `${backends}#${serviceStore.gatewayState() ?? ""}:${gtr?.kind ?? ""}:${gtr?.origin ?? ""}#${snap.gateway?.activation_applying?.message ?? ""}`;
}

/** One store refresh: repaint the lifecycle overlay when something it
 *  shows changed (an identical repaint would replace the buttons under
 *  the pointer and lose the click), and run the full dashboard fetch
 *  when the last one is older than FULL_FETCH_MS. Paused: nothing
 *  repaints. */
function onStoreSnapshot(snap: ServiceSnapshot): void {
  if (disposed || paused) return;
  const sig = lcSignature(snap);
  if (!lastModel || Date.now() - lastFetchAt >= FULL_FETCH_MS) {
    lastLcSig = sig;
    void doPoll(snap);
    return;
  }
  if (sig === lastLcSig) return;
  lastLcSig = sig;
  render();
}

export async function mountDashboard(): Promise<() => void> {
  disposed = false;
  staleShown = false;
  // A pause from a previous mount must not leak into this one: the
  // view would sit blank (repaints no-op while paused) with no Pause
  // button painted to unstick it.
  paused = false;
  lastFetchAt = 0;
  lastLcSig = "";
  await doPoll(serviceStore.get().updatedAt > 0 ? serviceStore.get() : undefined);
  const view = document.getElementById("view");
  // The card Start/Stop/Restart buttons (gateway and backends) run
  // through the shared lifecycle runner; failures land in the notice
  // area, and the store refresh repaints the cards.
  unwireActions = view
    ? wireLifecycleActions(view, (r) => {
      if (disposed) return;
      if (!r.ok && !r.cancelled) { restartError = r.message; render(); }
    }, { origin: "dashboard" })
    : null;
  unsubscribeStore = serviceStore.subscribe(onStoreSnapshot);
  // "Xs ago" ticker: update the timestamp node in place instead of a full re-render.
  tickTimer = window.setInterval(() => {
    if (disposed) return;
    // Crossing the stale boundary (e.g. while paused) needs a full
    // re-render so the STALE tag and verdict appear without a poll.
    // Uses render()'s formula exactly, so the states cannot flap.
    if (lastModel) {
      const s = lastModel.stale || (lastOk > 0 && Date.now() - lastOk > 10_000);
      if (s !== staleShown) { render(); return; }
    }
    const node = document.getElementById("updatedAgo");
    if (node && lastModel) node.textContent = `Last updated ${ago(lastModel.updatedAt)}`;
  }, 1000);
  return () => {
    disposed = true;
    unsubscribeStore?.();
    unwireActions?.();
    unsubscribeStore = unwireActions = null;
    if (tickTimer) clearInterval(tickTimer);
    tickTimer = null;
  };
}
