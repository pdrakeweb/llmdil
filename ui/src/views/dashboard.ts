/* Dashboard view — fully functional.
   - Appliance: service cards, GPU strip, model placement, throughput chart.
   - Console: dense tables with sparklines/bars.
   - Topology: mini cluster node map (SVG).
   Polls getDashboard() every 5s; Pause live stops polling; data older than
   10s without a successful poll is labeled STALE (spec §4 global UI rules).
   Backend failures keep the last good model and show an error banner —
   never fabricated data presented as live. */

import { getDashboard, isTauri, restartBackend, systemVerdict } from "../api";
import type { SystemVerdict } from "../api";
import { esc } from "../html";
import { gaugeRing, meter, multiLine, sparkline, stackedBar } from "../charts";
import { icon } from "../icons";
import type { BackendInfo, DashboardModel, GpuInfo, PlacementSegment } from "../types";

type ViewMode = "appliance" | "console" | "topology";

const VIEW_KEY = "llm.view.dashboard";
let mode: ViewMode = (localStorage.getItem(VIEW_KEY) as ViewMode) || "appliance";
if (mode !== "appliance" && mode !== "console" && mode !== "topology") mode = "appliance";

let timer: number | null = null;
let paused = false;
let lastOk = 0;
let lastModel: DashboardModel | null = null;
let lastLive = false;
let lastError: string | null = null;
/** Set when a restart_backend call fails; shown in the header notice area
 *  and cleared by the next successful poll. */
let restartError: string | null = null;
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

function statusDot(s: string): string {
  const cls = s === "ok" ? "ok" : s === "degraded" || s === "starting" ? "warn" : s === "down" ? "err" : "idle";
  return `<span class="dot ${cls}"></span>`;
}

/** All services including the gateway when the backend reported one. */
function allServices(m: DashboardModel): BackendInfo[] {
  return m.gateway ? [m.gateway, ...m.backends] : [...m.backends];
}

function serviceCard(b: BackendInfo, isGateway: boolean): string {
  const ringColor = b.status === "ok" ? C.green : b.status === "starting" ? C.amber : C.red;
  // requests_1h / errors_1h / throughput_toks come from LiteLLM's
  // Prometheus counters via the app's metrics poller (live since
  // v0.1.18); health / vram_held_gb still have no live source. Render
  // "—" when absent instead of crashing on .toFixed or presenting a
  // fabricated measurement.
  const ringVal = isGateway
    ? (b.requests_1h !== undefined ? String(b.requests_1h) : "—")
    : (b.health !== undefined ? b.health.toFixed(0) : "—");
  const ringSub = isGateway ? "req/1h" : "health";
  const m1k = isGateway ? "REQUESTS / 1H" : "THROUGHPUT";
  const m1v = isGateway
    ? (b.requests_1h !== undefined ? b.requests_1h.toLocaleString() : "—")
    : (b.throughput_toks !== undefined ? b.throughput_toks.toFixed(1) : "—");
  const m1u = isGateway || b.throughput_toks === undefined ? "" : " tok/s";
  const m2k = isGateway ? "ERRORS / 1H" : "VRAM HELD";
  const m2v = isGateway
    ? (b.errors_1h !== undefined ? String(b.errors_1h) : "—")
    : (b.vram_held_gb !== undefined ? b.vram_held_gb.toFixed(1) : "—");
  const m2u = isGateway || b.vram_held_gb === undefined ? "" : " GB";
  // The ring's arc is the metric itself: when the metric is absent the
  // arc must not assert a value — draw the neutral track only. The
  // gateway's requests_1h is a count, not a percentage, so its ring is
  // always the neutral track (the count shows as the center value and
  // in the REQUESTS / 1H row); a backend's health IS a percentage and
  // drives its arc when present.
  const ringMetric = isGateway ? undefined : b.health;
  const ring = ringMetric === undefined
    ? gaugeRing(0, 64, "var(--line)", esc(ringVal), ringSub)
    : gaugeRing(ringMetric, 64, ringColor, esc(ringVal), ringSub);
  // Sparkline captions render only when a series exists — live
  // services carry no series, so the whole strip is omitted rather
  // than captioning an empty chart.
  const sparkHtml = b.spark && b.spark.length
    ? `<div class="spark-wrap"><div class="cap">${isGateway ? "Requests per minute" : "Tokens/s"}</div>
      ${sparkline(b.spark, 220, 34, isGateway ? C.blue : b.id === "coder-30b" ? C.purple : b.id === "coder-7b" ? C.teal : C.blue)}
    </div>`
    : "";
  // The gateway card is the probe summary, not a supervised backend:
  // Restart would target a backend id that does not exist. Its action
  // is the Gateway view, where the start/restart control lives.
  const actions = isGateway
    ? `<div class="card-actions"><a class="btn" href="#/gateway">${icon("gateway")} Open Gateway view</a></div>`
    : `<div class="card-actions">
      <button class="btn" data-act="restart" data-id="${esc(b.id)}">${icon("restart")} Restart</button>
      <button class="btn" data-act="logs" data-id="${esc(b.id)}">${icon("logs")} Logs</button>
    </div>`;
  return `<div class="card">
    <h3>${statusDot(b.status)} ${esc(b.name)} <span class="port">:${b.port}</span></h3>
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
  const vramPct = Math.round((g.vram_used_gb / g.vram_total_gb) * 100);
  // Sparkline history and peak/limit values are not in the telemetry
  // payload; omit those rows/figures when absent instead of crashing
  // sparkline() on undefined or printing "undefined".
  const utilSpark = g.util_spark && g.util_spark.length
    ? `<div class="mrow"><span></span>${sparkline(g.util_spark, 220, 26, C.blue)}</div>` : "";
  const tempSpark = g.temp_spark && g.temp_spark.length
    ? `<div class="mrow"><span></span>${sparkline(g.temp_spark, 220, 26, C.amber)}</div>` : "";
  const powerSpark = g.power_spark && g.power_spark.length
    ? `<div class="mrow"><span></span>${sparkline(g.power_spark, 220, 26, C.green)}</div>` : "";
  const peak = g.temp_peak_c !== undefined ? ` · peak ${g.temp_peak_c}` : "";
  const limit = g.power_limit_w !== undefined ? ` / ${g.power_limit_w}` : "";
  return `<div class="card">
    <div class="gpu-head">${statusDot(g.status)} ${icon("gpu")} ${esc(g.name)}</div>
    <div class="gpu-metrics">
      <div class="mrow"><span class="mlabel">Utilization</span><span class="mval">${g.util_pct}%</span></div>
      ${utilSpark}
      <div class="mrow"><span class="mlabel">VRAM ${g.vram_used_gb.toFixed(1)}/${g.vram_total_gb} GB</span><span class="mval">${vramPct}%</span></div>
      <div class="mrow"><span></span>${meter(vramPct, 220, 8, C.purple)}</div>
      <div class="mrow"><span class="mlabel">Temperature</span><span class="mval">${g.temp_c} °C${peak}</span></div>
      ${tempSpark}
      <div class="mrow"><span class="mlabel">Power</span><span class="mval">${g.power_w == null ? "N/A" : `${g.power_w}${limit} W`}</span></div>
      ${powerSpark}
    </div>
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
  const placeHtml = m.placement.map((p) => {
    const total = p.segments.reduce((a, s) => a + s.pct, 0) || 1;
    return `<div class="place-row"><span class="mname">${esc(p.model)}</span>
      <div>${stackedBar(p.segments.map((s, k) => ({ pct: (s.pct / total) * 100, color: gpuColor(segmentGpuPos(s, k, m.gpus)), label: `${s.gpu} ${s.pct}%` })))}</div></div>`;
  }).join("");
  // The legend is built from the rows actually rendered: each distinct
  // segment label with the palette color its GPU position produced.
  const legendByLabel = new Map<string, string>();
  for (const p of m.placement) {
    p.segments.forEach((s, k) => {
      if (!legendByLabel.has(s.gpu)) legendByLabel.set(s.gpu, gpuColor(segmentGpuPos(s, k, m.gpus)));
    });
  }
  const legend = [...legendByLabel.entries()]
    .map(([label, color]) => `<span><span class="sw" style="background:${color}"></span>${esc(label)}</span>`).join("");
  const legendHtml = m.placement.length ? `<div class="legend">${legend}</div>` : "";
  // Live (and sample) empty state: a placement card with no splits must
  // say so and point at the control, never render as a blank titled card.
  const placeEmpty = m.placement.length ? ""
    : `<div style="font-size:12px;color:var(--text2)">No tensor splits configured. Set one under GPUs \u2192 Placement.</div>`;
  // A series exists only in sample data (the live path reports none);
  // the title must not claim a measured window for it.
  const chart = m.throughputSeries.length
    ? `<div class="card"><div class="sec-title">${icon("bolt")} Throughput \u2014 all backends \u00b7 ${live ? "1h" : "sample data"}</div>
       <div class="legend" style="margin:0 0 8px">${m.throughputSeries.map((s) => `<span><span class="sw" style="background:${esc(s.color)}"></span>${esc(s.name)}</span>`).join("")}</div>
       ${multiLine(m.throughputSeries, 560, 220)}</div>`
    : "";
  const events = m.events.length
    ? `<div class="card"><div class="sec-title">${icon("clock")} Recent activity</div>
       <div class="events">${m.events.map((e) => `<div class="ev"><span class="dot ${e.level === "ok" ? "ok" : e.level === "warn" ? "warn" : e.level === "error" ? "err" : "idle"}"></span><time>${esc(e.time)}</time><span>${esc(e.text)}</span></div>`).join("")}</div></div>`
    : "";
  const empty = !svcs.length && !m.gpus.length
    ? `<div class="card"><div class="empty">No backends or GPUs reported. Run the setup wizard to install and configure the stack.</div></div>` : "";
  return `${empty}
    <div class="cards5">${cards}</div>
    <div class="cards3" style="margin-top:12px">${gpus}</div>
    <div class="grid2">
      <div class="card"><div class="sec-title">${icon("layers")} Model placement</div>${placeHtml}${placeEmpty}${legendHtml}</div>
      ${chart}
    </div>
    ${events ? `<div style="margin-top:12px">${events}</div>` : ""}`;
}

function consoleView(m: DashboardModel): string {
  const svcRows = allServices(m).map((b) => {
    // health/throughput/requests have no live source; show "—" when absent
    // (the old expression printed "undefined req/h" and could not render).
    const rate = b.throughput_toks
      ? `${b.throughput_toks.toFixed(1)} tok/s`
      : b.requests_1h !== undefined ? `${b.requests_1h} req/h` : "—";
    return `<tr>
    <td>${statusDot(b.status)} ${esc(b.name)}</td><td>:${b.port}</td><td>${b.model ? esc(b.model) : "—"}</td>
    <td>${b.health ?? "—"}</td><td>${rate}</td>
    <td>${b.spark && b.spark.length ? sparkline(b.spark, 120, 20, C.blue) : "—"}</td></tr>`;
  }).join("");
  const gpuRows = m.gpus.map((g) => {
    const vramPct = Math.round((g.vram_used_gb / g.vram_total_gb) * 100);
    return `<tr><td>${statusDot(g.status)} ${esc(g.name)}</td><td>${g.util_pct}%</td>
      <td>${meter(g.util_pct, 120, 8, C.blue)}</td>
      <td>${g.vram_used_gb.toFixed(1)}/${g.vram_total_gb} GB</td>
      <td>${meter(vramPct, 120, 8, C.purple)}</td>
      <td>${g.temp_c}°C</td><td>${g.power_w == null ? "N/A" : `${g.power_w}W`}</td></tr>`;
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

  const statusColor = (s: string): string =>
    s === "ok" ? C.green : s === "degraded" || s === "starting" ? C.amber : s === "down" ? C.red : "#6d7686";
  /** Edge class follows the backend's state: hot only while running. */
  const edgeCls = (s: string): string => (s === "ok" ? "hot" : s === "starting" || s === "degraded" ? "warn" : "");

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
    m.gateway ? `:${m.gateway.port}${m.gatewayStrategy ? ` \u00b7 ${m.gatewayStrategy}` : ""}` : "not reported",
    m.gateway ? statusColor(m.gateway.status) : "#6d7686");
  backs.forEach((b, i) => {
    const y = cy(i, backs.length);
    node(colX[1], y, b.name, `:${b.port}${b.throughput_toks !== undefined ? ` \u00b7 ${b.throughput_toks.toFixed(1)} tok/s` : ""}`, statusColor(b.status));
    edge(colX[0] + nodeW, H / 2, colX[1], y, "", edgeCls(b.status));
  });
  gpus.forEach((g, i) => {
    node(colX[2], cy(i, gpus.length), g.name, `${g.util_pct}% util`, statusColor(g.status));
  });
  // Backend -> GPU connectivity comes from the configured tensor
  // splits (the placement rows' segments), exactly like the full
  // Service Map — never a positional full mesh asserting links the
  // configuration does not have. A segment that cannot be matched to
  // a drawn GPU node draws no edge.
  const backendIdxById = new Map(backs.map((b, i) => [b.id, i] as const));
  const rowStatus = (p: (typeof models)[number]): string | undefined =>
    p.backendStatus ?? (p.backendId ? backs.find((b) => b.id === p.backendId)?.status : undefined);
  const segGpuIdx = (seg: PlacementSegment): number => {
    if (seg.gpuIndex !== undefined && gpus[seg.gpuIndex]) return seg.gpuIndex;
    return gpus.findIndex((g) => g.name === seg.gpu || g.id === seg.gpu);
  };
  for (const p of models) {
    if (!p.backendId) continue;
    const bi = backendIdxById.get(p.backendId);
    if (bi === undefined) continue;
    p.segments.forEach((seg) => {
      const gj = segGpuIdx(seg);
      if (gj < 0) return;
      edge(colX[1] + nodeW, cy(bi, backs.length), colX[2], cy(gj, gpus.length), "", edgeCls(backs[bi].status));
    });
  }
  // Model nodes key to their owning backend's measured state: the dot
  // is the backend's status color, and the sublabel is the split
  // summary — configuration alone never asserts a model is "loaded".
  models.slice(0, 4).forEach((p, i) => {
    const y = cy(i, Math.min(4, models.length));
    const st = rowStatus(p);
    const sub = p.segments.length ? p.segments.map((s) => `${s.pct}%`).join(" \u00b7 ") : "\u2014";
    node(colX[3], y, p.model, sub, st ? statusColor(st) : "#6d7686");
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
    <div class="legend"><span><span class="sw" style="background:${C.green}"></span>healthy</span>
    <span><span class="sw" style="background:${C.amber}"></span>degraded / starting</span>
    <span><span class="sw" style="background:${C.red}"></span>down / failed</span>
    <a href="#/map" style="color:var(--accent)">Open full service map \u2192</a></div></div>`;
}

function headerHtml(live: boolean, stale: boolean, updatedAgo: string, verdict: SystemVerdict): string {
  const dotStatus = verdict.dot === "warn" ? "degraded" : verdict.dot === "err" ? "down" : verdict.dot;
  return `<div class="topbar">
    <h1>Dashboard</h1>
    <span class="status-pill">${statusDot(dotStatus)} ${esc(verdict.text)}</span>
    <span class="viewsel-label">View:</span>
    <span class="viewsel" role="tablist">
      <button data-view="appliance" class="${mode === "appliance" ? "active" : ""}">Appliance</button>
      <button data-view="console" class="${mode === "console" ? "active" : ""}">Console</button>
      <button data-view="topology" class="${mode === "topology" ? "active" : ""}">Topology</button>
    </span>
    <span class="spacer"></span>
    ${stale ? `<span class="stale-tag">STALE</span>` : ""}
    <span class="updated" id="updatedAgo">Last updated ${esc(updatedAgo)}</span>
    <button class="btn" id="pauseBtn">${icon(paused ? "play" : "pause")} ${paused ? "Resume live" : "Pause live"}</button>
    <a class="btn" href="#/wizard">Setup wizard</a>
  </div>
  ${lastError ? `<div style="padding:12px 20px 0"><div class="banner">${icon("warn")} backend error — showing last known data: ${esc(lastError)}</div></div>` : ""}
  ${lastGatewayError ? `<div style="padding:12px 20px 0"><div class="banner">${icon("warn")} Gateway status unavailable: ${esc(lastGatewayError)}</div></div>` : ""}
  ${restartError ? `<div style="padding:12px 20px 0"><div class="banner" id="restartNotice">${icon("warn")} ${esc(restartError)}</div></div>` : ""}
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
      ? headerHtml(lastLive, true, "never", systemVerdict({ total: 0, running: 0, stale: true, error: lastError, gatewayUp: false })) + `<div class="view"><div class="card"><div class="empty">Backend error: ${esc(lastError)} — retrying…</div></div></div>`
      : "";
    return;
  }
  const m = lastModel;
  // Staleness is a function of the last successful poll's age, so it
  // applies while polling is paused too — not only on a failed poll.
  const staleNow = m.stale || (lastOk > 0 && Date.now() - lastOk > 10_000);
  staleShown = staleNow;
  const running = m.backends.filter((b) => b.status === "ok").length;
  const verdict = systemVerdict({
    total: m.backends.length,
    running,
    stale: staleNow,
    error: lastError,
    gatewayUp: m.gatewayUp,
  });
  const body = mode === "appliance" ? applianceView(m, lastLive) : mode === "console" ? consoleView(m) : topologyView(m);
  el.innerHTML = headerHtml(lastLive, staleNow, ago(m.updatedAt), verdict) + `<div class="view">${body}</div>`;
  el.querySelectorAll("[data-view]").forEach((b) =>
    b.addEventListener("click", () => { setMode((b as HTMLElement).dataset.view as ViewMode); render(); }));
  const pb = el.querySelector("#pauseBtn");
  pb?.addEventListener("click", () => { paused = !paused; render(); });
  el.querySelectorAll("[data-act]").forEach((b) =>
    b.addEventListener("click", () => {
      const act = (b as HTMLElement).dataset.act;
      const id = (b as HTMLElement).dataset.id;
      if (act === "logs" && id) location.hash = `#/logs?scope=${encodeURIComponent(id)}`;
      else if (act === "restart" && id) {
        (b as HTMLButtonElement).disabled = true; // guard against double-submit; render() rebuilds the DOM
        void (async () => {
          const r = await restartBackend(id);
          if (r.error) {
            restartError = `Restart of ${id} failed: ${r.error}`;
            render();
          } else {
            await doPoll(); // immediate refresh so the new state shows
          }
        })();
      }
    }));
}

async function doPoll(): Promise<void> {
  const r = await getDashboard(); // never rejects; failures arrive as r.error
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
    // A gateway-probe failure rides along on an otherwise successful
    // poll: lastOk advances (the data IS fresh) and the failure shows
    // as its own banner, not as backend staleness.
    lastGatewayError = r.data.gatewayError ?? null;
    lastOk = Date.now();
  }
  render();
}

async function poll(): Promise<void> {
  if (paused) return;
  await doPoll();
}

export async function mountDashboard(): Promise<() => void> {
  disposed = false;
  staleShown = false;
  // A pause from a previous mount must not leak into this one: the
  // view would sit blank (poll() no-ops while paused) with no Pause
  // button painted to unstick it.
  paused = false;
  await poll();
  timer = window.setInterval(poll, 5000);
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
    if (timer) clearInterval(timer);
    if (tickTimer) clearInterval(tickTimer);
    timer = tickTimer = null;
  };
}
