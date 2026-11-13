/* Service Map — full cluster topology (spec §4.3).
   Layout faithful to mockup 05-service-map.png: LiteLLM gateway -> backends
   -> GPUs, dashed MXC boundary around the backends, cluster summary
   inspector, GPU load rings.
   Live wiring: renderServiceMap() returns the shell; wireServiceMap(root)
   takes backends and the gateway summary from the shared service store
   (lifecycle.ts: fast during transitions, with the local pending
   overlay), loads GPU telemetry and the config itself, and paints real
   nodes — gateway port
   and group count, backend id/port/process state, GPU name and VRAM
   used/total. Node dots and edges are colored by backend state. Nothing is
   fabricated: with no backends configured the canvas shows an empty state,
   and in the browser (live=false) a sample-mode banner says so. */

import { getAppConfig, getGpuTelemetry, systemVerdict } from "../api";
import type { BackendRow, GatewayStatusData } from "../api";
import { fmtGb, gaugeRing, trim1 } from "../charts";
import { icon } from "../icons";
import { esc } from "../html";
import { LIFECYCLE_COLORS, META, lifecycleColor, lifecycleEdge, lifecycleLabel, serviceStore } from "../lifecycle";
import type { LifecycleState, ServiceSnapshot } from "../lifecycle";
import type { GpuInfo } from "../types";

const C = { green: LIFECYCLE_COLORS.green, amber: LIFECYCLE_COLORS.amber, red: LIFECYCLE_COLORS.red, blue: "#4a9eff", grey: LIFECYCLE_COLORS.grey };

/** Node rect stroke for a lifecycle state: only failure/transition
 *  override the default border, so running/stopped keep the approved
 *  quiet look. */
/** Placeholder payload when gateway_status failed: the node renders
 *  "status unknown" from gatewayError, never these zeros. */
const GATEWAY_UNKNOWN: GatewayStatusData = {
  port: 0, groups: 0, routing_rules: 0, up: false, state: "unknown", transition: null, intent: null,
  pid: null, service_managed: false, failure: null, postgres: null, activation_applying: null,
  routing_log_len: 0, openrouter_cred_present: null, bind_host: "", lan_ip: null,
  activation_pending: false, activation_changes: 0, litellm_version: null,
};

function lifecycleStroke(s: LifecycleState): string {
  if (s === "failed") return C.red;
  if (META[s].isTransition || s === "degraded") return C.amber;
  return "";
}

function gpuDot(g: GpuInfo): string {
  switch (g.status) {
    case "ok": return C.green;
    case "degraded": case "starting": return C.amber;
    case "down": return C.red;
    default: return C.grey;
  }
}

function fmtUptime(secs: number): string {
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m`;
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  return m ? `${h}h${m}m` : `${h}h`;
}

interface MapData {
  backends: BackendRow[];
  /** Effective lifecycle state per backend id (the store's overlay). */
  states: Map<string, LifecycleState>;
  /** Effective gateway/PostgreSQL lifecycle state (null: unmeasured). */
  gatewayState: LifecycleState | null;
  postgresState: LifecycleState | null;
  gpus: GpuInfo[];
  stale: boolean;
  gateway: GatewayStatusData;
  /** True when the gateway_status call itself failed — the gateway's
   *  state is unknown, and the node renders that explicitly instead
   *  of the zeroed fallback payload (port 0, no groups/rules). */
  gatewayError: boolean;
  /** The list_backends error, when it failed — distinguishes "no
   *  backends configured" from "the backend list could not be
   *  loaded" in the empty-canvas state. */
  backendsError: string | null;
  /** Configured MXC mode (learning | enforced | disabled), or null
   *  when the config did not report one. */
  mxcMode: string | null;
  /** Backend id -> configured tensor split (fractions per GPU index),
   *  from the app config; backends absent here get no GPU edges. */
  splits: Map<string, number[]>;
  /** Backend id -> measured residency: GPU index -> MiB held there,
   *  from telemetry's per-process attribution. Raw attribution: a
   *  backend appears on every GPU where its process holds even a
   *  bare CUDA context, so consumers must apply RESIDENCY_MIN_MIB
   *  before treating an entry as model residency. A null value is
   *  presence without a measurement (WDDM GPU, counter data
   *  unavailable): it is neither residency nor context. */
  residency: Map<string, Map<number, number | null>>;
  live: boolean;
  errors: string[];
  /** When this paint's data was fetched (epoch ms). */
  updatedAt: number;
}

/** Minimum measured per-GPU memory for a backend to count as
 *  resident on that GPU. Below this a process holds only its CUDA
 *  context there (llama.cpp initializes a context on every visible
 *  GPU — 256 MiB each on Pete's P100 — while its weights sit where
 *  the tensor split, or the main GPU by default, placed them). The
 *  same 1 GiB threshold the GPUs view and backend cards label
 *  "context only" with. */
const RESIDENCY_MIN_MIB = 1024;

const W = 1200, H = 640;
const NW = 170, NH = 62;
const COL = { gateway: 110, backends: 470, gpus: 830 };

/** Even vertical spread for a column of n nodes on the canvas. */
function spreadY(i: number, n: number): number {
  if (n <= 1) return H / 2;
  const top = 110, bottom = 570;
  return top + (i * (bottom - top)) / (n - 1);
}

function topologySvg(d: MapData): string {
  const { backends, gpus, gateway } = d;
  let s = `<svg width="${W}" height="${H}" viewBox="0 0 ${W} ${H}" style="display:block;width:100%" role="img" aria-label="Full cluster service map">`;

  // MXC dashed boundary around the backends column. The label states
  // the configured mode; "POLICY-CHECKED" is claimed only when the
  // sandbox is not disabled.
  const ys = backends.map((_, i) => spreadY(i, backends.length));
  const rectY = Math.max(36, Math.min(...ys) - NH / 2 - 44);
  const rectH = Math.max(...ys) + NH / 2 + 44 - rectY;
  const mxcLabel = d.mxcMode ? `MXC \u00b7 ${d.mxcMode.toUpperCase()}` : "MXC";
  s += `<rect x="${COL.backends - 40}" y="${rectY}" width="${NW + 80}" height="${rectH}" rx="10" fill="none" stroke="#4a9eff" stroke-width="1" stroke-dasharray="7 5" opacity="0.55"/>
        <text x="${COL.backends + NW / 2}" y="${rectY - 8}" text-anchor="middle" fill="#4a9eff" font-size="10" letter-spacing="1">${esc(mxcLabel)}</text>`;

  const edge = (x1: number, y1: number, x2: number, y2: number, cls = "", stroke = "") => {
    const mx = (x1 + x2) / 2;
    s += `<path class="map-edge ${cls}"${stroke ? ` style="stroke:${stroke}"` : ""} d="M${x1},${y1} C${mx},${y1} ${mx},${y2} ${x2},${y2}"/>`;
  };
  const node = (x: number, y: number, title: string, sub: string, dot: string, stroke = "", ring?: { pct: number; color: string; label: string }, href?: string) => {
    const g = `<g>
      <rect class="map-node" x="${x}" y="${y - NH / 2}" width="${NW}" height="${NH}" rx="8"${stroke ? ` stroke="${stroke}" stroke-width="1.5"` : ""}/>
      <circle cx="${x + 16}" cy="${y - 10}" r="4" fill="${dot}"/>
      <text class="map-node-text" x="${x + 28}" y="${y - 4}">${esc(title)}</text>
      <text class="map-node-sub" x="${x + 16}" y="${y + 16}">${esc(sub)}</text>
      ${ring ? `<g transform="translate(${x + NW - 26},${y - 12})"><circle r="13" fill="none" stroke="#2b313b" stroke-width="4"/><circle r="13" fill="none" stroke="${ring.color}" stroke-width="4" stroke-linecap="round" stroke-dasharray="${(2 * Math.PI * 13).toFixed(1)}" stroke-dashoffset="${(2 * Math.PI * 13 * (1 - ring.pct / 100)).toFixed(1)}" transform="rotate(-90)"/><text y="4" text-anchor="middle" fill="#e8ebf0" font-size="9" font-weight="700">${esc(ring.label)}</text></g>` : ""}
    </g>`;
    // Nodes are actionable: backend nodes link to the Backends view,
    // the gateway node to the Gateway view (SVG <a> elements).
    s += href ? `<a href="${href}">${g}</a>` : g;
  };

  // Gateway -> backends for every backend. Backend -> GPU edges
  // claim where the model actually resides, from the strongest data
  // available:
  // 1. Measured residency of RESIDENCY_MIN_MIB or more on a GPU
  //    (solid edge). Raw per-process attribution alone is NOT
  //    residency: a llama-server process holds a small CUDA context
  //    on every visible GPU, so drawing every attribution row made
  //    every backend appear on every GPU.
  // 2. For a live backend with no measured residency anywhere, its
  //    configured placement (dotted edge): the tensor split it was
  //    launched with, or the main GPU (index 0) when no split is
  //    set — llama.cpp's default device. This is also the only
  //    placement signal for a display GPU under WDDM, where
  //    nvidia-smi cannot measure per-process memory at all.
  // A stopped or failed backend resides nowhere: no GPU edges.
  const stOf = (b: BackendRow): LifecycleState => d.states.get(b.id) ?? "unknown";
  backends.forEach((b, i) => {
    const e = lifecycleEdge(stOf(b));
    edge(COL.gateway + NW, H / 2, COL.backends, ys[i], e.cls, e.stroke);
    const measured = d.residency.get(b.id);
    const resident = measured
      ? [...measured.entries()].filter((e): e is [number, number] => e[1] !== null && e[1] >= RESIDENCY_MIN_MIB)
      : [];
    if (resident.length) {
      for (const [j] of resident) {
        if (gpus[j]) edge(COL.backends + NW, ys[i], COL.gpus, spreadY(j, gpus.length), e.cls, e.stroke);
      }
    } else if (META[stOf(b)].isLive) {
      const split = d.splits.get(b.id);
      const targets = split
        ? split.map((frac, j) => (frac > 0 ? j : -1)).filter((j) => j >= 0)
        : [];
      if (targets.length) {
        for (const j of targets) {
          if (gpus[j]) edge(COL.backends + NW, ys[i], COL.gpus, spreadY(j, gpus.length), "configured", "");
        }
      } else if (gpus.length) {
        edge(COL.backends + NW, ys[i], COL.gpus, spreadY(0, gpus.length), "configured", "");
      }
    }
  });

  // A failed gateway_status call leaves only a zeroed fallback
  // payload — render the measured-nothing state ("status unknown",
  // neutral dot) rather than a fabricated ":0 · 0 groups · down".
  if (d.gatewayError) {
    node(COL.gateway, H / 2, "LiteLLM", "status unknown", C.grey, "", undefined, "#/gateway");
  } else {
    // The gateway node names its lifecycle state when it is not simply
    // running ("restarting… (activation, 12 s)", "stopped", …) and the
    // PostgreSQL state when the database is not running.
    const gs = d.live ? d.gatewayState : null;
    const gwTxt = gs && gs !== "running" ? ` \u00b7 ${lifecycleLabel(gs, serviceStore.transitionOf("gateway", gateway.transition))}` : "";
    const pg = d.live ? d.postgresState : null;
    const pgTxt = pg && pg !== "running" && pg !== "not_installed" ? ` \u00b7 db ${lifecycleLabel(pg)}` : "";
    node(COL.gateway, H / 2, "LiteLLM",
      `:${gateway.port} \u00b7 ${gateway.groups} enabled groups \u00b7 ${gateway.routing_rules} rules${gwTxt}${pgTxt}`,
      gs ? lifecycleColor(gs) : C.grey, gs ? lifecycleStroke(gs) || C.blue : C.blue, undefined, "#/gateway");
  }

  backends.forEach((b, i) => {
    const st = stOf(b);
    const up = st === "running" && b.uptime_secs != null ? ` \u00b7 up ${fmtUptime(b.uptime_secs)}` : "";
    const model = b.model_file ? ` \u00b7 ${b.model_file.split(/[\\/]/).pop()}` : "";
    const res = d.residency.get(b.id);
    const resident = res ? [...res.entries()].filter((e): e is [number, number] => e[1] !== null && e[1] >= RESIDENCY_MIN_MIB) : [];
    const anyMeasured = res ? [...res.values()].some((v) => v !== null) : false;
    const resTxt = resident.length
      ? ` \u00b7 ${resident.map(([j, mib]) => `GPU${j} ${fmtGb(mib / 1024)}G`).join(" ")}`
      : res && res.size
        ? anyMeasured ? " \u00b7 context only" : " \u00b7 residency not measured"
        : "";
    node(COL.backends, ys[i], b.id, `:${b.port} \u00b7 ${lifecycleLabel(st, serviceStore.transitionOf(`backend:${b.id}`, b.transition ?? null))}${up}${model}${resTxt}`, lifecycleColor(st), lifecycleStroke(st), undefined, "#/backends");
  });

  gpus.forEach((g, j) => {
    const pct = g.vram_total_gb > 0 ? Math.round((g.vram_used_gb / g.vram_total_gb) * 100) : 0;
    node(COL.gpus, spreadY(j, gpus.length), g.name,
      `${fmtGb(g.vram_used_gb)} / ${fmtGb(g.vram_total_gb)} GB VRAM`,
      gpuDot(g), "", { pct, color: pct >= 80 ? C.amber : C.green, label: `${pct}%` });
  });

  s += `<text class="map-col-label" x="${COL.gateway}" y="48">GATEWAY</text>
    <text class="map-col-label" x="${COL.backends}" y="48">${d.mxcMode === "learning" || d.mxcMode === "enforced" ? "BACKENDS \u00b7 POLICY-CHECKED" : "BACKENDS"}</text>
    ${gpus.length ? `<text class="map-col-label" x="${COL.gpus}" y="48">GPUS</text>` : ""}</svg>`;
  return s;
}

function paintServiceMap(root: HTMLElement, d: MapData): void {
  const banner = root.querySelector("#mapBanner") as HTMLElement | null;
  if (banner) {
    if (d.errors.length) {
      banner.style.display = "";
      banner.title = "";
      banner.innerHTML = `${icon("warn")} backend error — showing what could be loaded: ${esc(d.errors.join("; "))}`;
    } else if (!d.live) {
      banner.style.display = "";
      banner.title = "Open the installed app to see this machine's topology.";
      banner.innerHTML = `${icon("warn")} Sample mode`;
    } else {
      banner.style.display = "none";
    }
  }

  // The shared count rule (enabled + configured backends), used by the
  // pill and the summary row alike.
  const counts = d.live ? serviceStore.summary() : { ...serviceStore.summary(), gateway: null };
  const pill = root.querySelector("#mapPill");
  if (pill) {
    // The shared verdict rule (api.ts systemVerdict) — the same one
    // the Dashboard header pill uses. In sample mode the gateway state
    // is unmeasured, so it does not degrade the verdict.
    const verdict = systemVerdict({
      summary: counts,
      stale: d.stale,
      error: d.errors.length ? d.errors.join("; ") : null,
    });
    pill.innerHTML = `<span class="dot ${verdict.dot}"></span> ${esc(verdict.text)}`;
  }

  const updated = root.querySelector("#mapUpdated");
  if (updated) {
    updated.textContent = `${d.live ? "live data" : "sample data"} \u00b7 updated ${new Date(d.updatedAt).toLocaleTimeString("en-GB")}` + (d.stale ? " \u00b7 telemetry stale" : "");
  }

  const canvas = root.querySelector("#mapCanvas");
  if (canvas) {
    if (!d.backends.length) {
      // Three distinct empty states: the list could not be loaded
      // (an error, disclosed in the banner too), live with none
      // configured, and sample mode's honest no-data state.
      const emptyText = d.backendsError
        ? "Backend list could not be loaded."
        : d.live
          ? "No backends configured — run the setup wizard to install and configure the stack."
          : "No backend data in sample mode — open the installed app to see this machine's topology.";
      canvas.innerHTML = `<div class="empty" style="padding:24px">${emptyText}</div>`;
    } else {
      canvas.innerHTML = `${topologySvg(d)}
        <div class="legend" style="margin-top:8px">
          ${mapLegend()}
          <span><span class="dot ok"></span>GPU ok</span>
          <span><span class="dot warn"></span>GPU degraded</span>
          <span><span class="dot err"></span>GPU down</span>
          <span style="border:1px dashed #4a9eff;border-radius:3px;padding:0 5px">MXC boundary</span>
          <span style="color:var(--text3)">Solid GPU edges: measured residency of 1 GiB or more. Dotted edges: configured placement for a live backend with no measured residency (its tensor split, or the main GPU when no split is set). A process holding under 1 GiB on a GPU holds only a CUDA context there — it is listed under GPU residency, not drawn as placement.</span>
          <span><a href="#/backends" style="color:var(--accent)">Backends</a> \u00b7 <a href="#/gateway" style="color:var(--accent)">Gateway</a></span>
        </div>`;
    }
  }

  const vramUsed = d.gpus.reduce((a, g) => a + g.vram_used_gb, 0);
  const vramTotal = d.gpus.reduce((a, g) => a + g.vram_total_gb, 0);
  const summary: [string, string][] = [
    ["Backends", `${counts.backendsRunning} / ${counts.backendsTotal} running`],
    ["Gateway", d.gatewayError ? "status unknown" : `:${d.gateway.port} \u00b7 ${d.gateway.groups} enabled groups${d.live && d.gatewayState && d.gatewayState !== "running" ? ` \u00b7 ${lifecycleLabel(d.gatewayState)}` : ""}`],
    ...(d.live && d.postgresState ? [["PostgreSQL", lifecycleLabel(d.postgresState)] as [string, string]] : []),
    ["Routing rules", String(d.gateway.routing_rules)],
    ["GPUs", `${d.gpus.length} detected`],
    ["VRAM used (all processes)", d.gpus.length ? `${fmtGb(vramUsed)} / ${fmtGb(vramTotal)} GB` : "—"],
  ];
  const summaryEl = root.querySelector("#mapSummary");
  if (summaryEl) {
    summaryEl.innerHTML = summary.map(([k, v]) => `<div style="display:flex;justify-content:space-between;padding:5px 0;border-bottom:1px solid #232a34"><span class="k">${esc(k)}</span><span class="v" style="font-size:12px">${esc(v)}</span></div>`).join("");
  }

  const resEl = root.querySelector("#mapResidency");
  if (resEl) {
    const byGpu = new Map<number, [string, number | null][]>();
    for (const [bid, gm] of d.residency) {
      for (const [j, mib] of gm) {
        const a = byGpu.get(j) ?? [];
        a.push([bid, mib]);
        byGpu.set(j, a);
      }
    }
    resEl.innerHTML = d.gpus.length
      ? d.gpus.map((g) => {
          const rows = byGpu.get(Number(g.id)) ?? [];
          return `<div style="margin-bottom:8px"><div style="font-size:12px">${esc(g.name)}</div>${rows.length ? rows.map(([bid, mib]) => `<div style="display:flex;justify-content:space-between;font-size:12px;padding:2px 0"><span>${esc(bid)}${mib !== null && mib < 1024 ? ` <span class="chip">context only</span>` : ""}</span><span>${mib === null ? "not measured" : `${fmtGb(mib / 1024)} GiB`}</span></div>`).join("") : `<div style="font-size:12px;color:var(--text3)">no backend processes measured</div>`}</div>`;
        }).join("")
      : `<div class="empty">no GPUs reported</div>`;
  }

  const gpuLoad = root.querySelector("#mapGpuLoad");
  if (gpuLoad) {
    gpuLoad.innerHTML = d.gpus.length
      ? d.gpus.map((g) => `<div style="display:flex;align-items:center;gap:10px;margin:8px 0"><span style="font-size:12px;width:70px">${esc(g.name.replace("Tesla ", ""))}</span>${gaugeRing(g.util_pct, 44, g.util_pct > 60 ? C.amber : C.green, trim1(g.util_pct), "%")}</div>`).join("")
      : `<div class="empty">no GPUs reported</div>`;
  }
}

/** The lifecycle legend, generated from the shared META: one swatch
 *  per color, labeled with the states it covers. */
function mapLegend(): string {
  const groups: { color: string; states: LifecycleState[] }[] = [];
  for (const s of ["running", "starting", "restarting", "stopping", "degraded", "failed", "stopped"] as LifecycleState[]) {
    const g = groups.find((x) => x.color === META[s].color);
    if (g) g.states.push(s); else groups.push({ color: META[s].color, states: [s] });
  }
  return groups.map((g) => `<span><span class="sw" style="background:${g.color}"></span>${esc(g.states.map((s) => META[s].label).join(" / "))}</span>`).join("");
}

export function renderServiceMap(): string {
  return `<div class="topbar"><h1>Service Map</h1>
      <span class="status-pill" id="mapPill"><span class="dot idle"></span> Loading</span>
      <span class="spacer"></span><span class="updated" id="mapUpdated"></span></div>
    <div class="view"><div class="banner" id="mapBanner">${icon("clock")} Loading topology…</div>
    <div class="map-wrap">
      <div class="map-canvas" id="mapCanvas"><div class="empty" style="padding:24px">Loading topology…</div></div>
      <div>
        <div class="card" style="margin-bottom:12px"><div class="sec-title">Cluster summary</div>
          <div class="kv" id="mapSummary"></div>
        </div>
        <div class="card"><div class="sec-title">GPU load</div>
          <div id="mapGpuLoad"><div class="empty">Loading…</div></div>
        </div>
        <div class="card" style="margin-top:12px"><div class="sec-title">GPU residency (measured)</div>
          <div id="mapResidency"><div class="empty">Loading…</div></div>
        </div>
      </div>
    </div></div>`;
}

/** Configured tensor splits by backend id, from the app config's
 *  backends[].flags.tensor_split (the same source the dashboard
 *  placement derivation reads). */
function splitsFromConfig(cfg: Record<string, unknown> | null): Map<string, number[]> {
  const out = new Map<string, number[]>();
  const backs = Array.isArray(cfg?.backends) ? cfg.backends : [];
  for (const b of backs) {
    if (!b || typeof b !== "object" || Array.isArray(b)) continue;
    const rec = b as Record<string, unknown>;
    const flags = rec.flags && typeof rec.flags === "object" && !Array.isArray(rec.flags)
      ? (rec.flags as Record<string, unknown>) : null;
    // Index-preserving: entry i belongs to GPU i, so a non-numeric
    // entry becomes 0 rather than being filtered out (filtering would
    // shift every later fraction onto the wrong GPU — the dashboard's
    // placementFromConfig maps over the raw array the same way).
    const split = Array.isArray(flags?.tensor_split)
      ? (flags.tensor_split as unknown[]).map((x) => (typeof x === "number" && Number.isFinite(x) ? x : 0))
      : [];
    if (typeof rec.id === "string" && split.length) out.set(rec.id, split);
  }
  return out;
}

export async function wireServiceMap(root: HTMLElement): Promise<() => void> {
  let disposed = false;
  let loading = false;
  /** Telemetry + config from the latest load; the lifecycle part of
   *  each paint comes from the shared store's latest snapshot. */
  let lastTele: Awaited<ReturnType<typeof getGpuTelemetry>> | null = null;
  let lastCfg: Awaited<ReturnType<typeof getAppConfig>> | null = null;
  let lastLoadAt = 0;

  const paint = (snap: ServiceSnapshot): void => {
    // No store snapshot yet: keep the Loading state rather than
    // painting an empty topology.
    if (disposed || !lastTele || !lastCfg || snap.updatedAt === 0) return;
    const g = lastTele;
    const cfg = lastCfg;
    const gwErr = snap.live ? snap.gatewayError : null;
    // A config failure belongs in the banner too: without it the MXC
    // labels fall back and the tensor-split edges vanish silently.
    const errors = [snap.backendsError, g.error, gwErr, cfg.error].filter((e): e is string => Boolean(e));
    const mxc = cfg.data?.mxc as { mode?: unknown } | undefined;
    const residency = new Map<string, Map<number, number | null>>();
    for (const gpu of g.data.gpus) {
      for (const p of gpu.processes ?? []) {
        if (p.backend === "\u2014") continue;
        const gm = residency.get(p.backend) ?? new Map<number, number | null>();
        // A backend can hold several processes on one GPU: its
        // residency there is their SUM (code-review 5.10 — the
        // plain set() kept only the last process's figure). A
        // null (not measured) contributes nothing; the entry is
        // null only when every process on that GPU is unmeasured.
        const key = Number(gpu.id);
        const prev = gm.get(key);
        if (p.vram_mib === null) {
          if (!gm.has(key)) gm.set(key, null);
        } else {
          gm.set(key, (prev ?? 0) + p.vram_mib);
        }
        residency.set(p.backend, gm);
      }
    }
    const live = snap.live && g.live;
    const gateway = snap.gateway ?? GATEWAY_UNKNOWN;
    paintServiceMap(root, {
      backends: snap.backends,
      states: new Map(snap.backends.map((r) => [r.id, serviceStore.backendState(r)])),
      gatewayState: serviceStore.gatewayState(),
      postgresState: serviceStore.postgresState(),
      gpus: g.data.gpus,
      stale: g.data.stale,
      gateway,
      gatewayError: snap.live && !snap.gateway,
      backendsError: snap.backendsError,
      mxcMode: typeof mxc?.mode === "string" ? mxc.mode : null,
      splits: splitsFromConfig(cfg.data),
      residency,
      live,
      errors,
      updatedAt: snap.updatedAt || Date.now(),
    });
  };

  /** Telemetry and config: refreshed on the store's cadence, at most
   *  every ~5 s (lifecycle repaints in between need neither). */
  const load = async (): Promise<void> => {
    if (loading) return;
    loading = true;
    lastLoadAt = Date.now();
    try {
      const [g, cfg] = await Promise.all([getGpuTelemetry(), getAppConfig()]);
      if (disposed) return;
      lastTele = g;
      lastCfg = cfg;
      paint(serviceStore.get());
    } finally {
      loading = false;
    }
  };

  await load();

  // The map asserts live state: every store refresh repaints it (the
  // shared poller — fast while anything is in transition, immediate on
  // lifecycle events), and telemetry reloads at most every ~5 s.
  const unsubscribe = serviceStore.subscribe((snap) => {
    if (disposed) return;
    if (Date.now() - lastLoadAt >= 4500) void load();
    else paint(snap);
  });

  return () => {
    disposed = true;
    unsubscribe();
  };
}
