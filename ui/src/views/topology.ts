/* The Dashboard's Topology view: gateway -> groups -> GPUs.
   Each model group is drawn as a container holding one row per member
   backend (its model, backend id and port, and lifecycle state), so the
   operator sees at a glance which models each group can route to and
   which of them are deployed (running). Backends in no group sit in
   their own container, unlinked: the gateway does not route to them.
   Row -> GPU edges come from the configured tensor splits of live
   backends only, as before: an edge asserts a live placement.
   Pure: everything comes from the DashboardModel. */

import { esc } from "../html";
import { fitText, trim1 } from "../charts";
import { LIFECYCLE_COLORS, META, lifecycleColor, lifecycleEdge, lifecycleLabel } from "../lifecycle";
import type { LifecycleState } from "../lifecycle";
import type { BackendInfo, DashboardModel, PlacementRow, PlacementSegment, Status } from "../types";

export interface TopologyRow {
  /** The member as configured (a backend id, or a host:port reference). */
  member: string;
  /** The backend this member names, when it is one of ours. */
  backend?: BackendInfo;
}

export interface TopologyGroup {
  /** The group name; null for the backends no group routes to. */
  name: string | null;
  strategy: string;
  fallbacks: string[];
  enabled: boolean;
  rows: TopologyRow[];
}

/** The containers to draw: the configured groups in order, then the
 *  backends no group lists. With the groups unknown (the gateway
 *  config could not be read), one unnamed container holds every
 *  backend, so the diagram never invents a grouping. */
export function topologyGroups(m: Pick<DashboardModel, "backends" | "groups">): TopologyGroup[] {
  const byId = new Map(m.backends.map((b) => [b.id, b] as const));
  if (!m.groups) {
    return m.backends.length
      ? [{ name: null, strategy: "", fallbacks: [], enabled: true, rows: m.backends.map((b) => ({ member: b.id, backend: b })) }]
      : [];
  }
  const out: TopologyGroup[] = m.groups.map((g) => ({
    name: g.name,
    strategy: g.strategy,
    fallbacks: g.fallbacks,
    enabled: g.enabled,
    rows: g.members.map((id) => ({ member: id, backend: byId.get(id) })),
  }));
  const grouped = new Set(m.groups.flatMap((g) => g.members));
  const loose = m.backends.filter((b) => !grouped.has(b.id));
  if (loose.length) {
    out.push({ name: null, strategy: "", fallbacks: [], enabled: false, rows: loose.map((b) => ({ member: b.id, backend: b })) });
  }
  return out;
}

/** A Status that means a live (or coming-up) process. */
function statusLive(s: Status | undefined): boolean {
  return s === "ok" || s === "starting" || s === "restarting" || s === "degraded";
}

function gpuColor(s: Status): string {
  return s === "ok" ? LIFECYCLE_COLORS.green : s === "down" ? LIFECYCLE_COLORS.red : s === "idle" ? LIFECYCLE_COLORS.grey : LIFECYCLE_COLORS.amber;
}

/** The best state among a group's members, for its gateway edge: a
 *  group with one running member is served. */
function groupState(g: TopologyGroup): LifecycleState {
  const states = g.rows.map((r) => r.backend?.lifecycle).filter((s): s is LifecycleState => Boolean(s));
  for (const s of ["running", "degraded", "restarting", "starting", "stopping", "failed"] as LifecycleState[]) {
    if (states.includes(s)) return s;
  }
  return "stopped";
}

const W = 960;
const GW_X = 20, GW_W = 210, NODE_H = 52;
const GRP_X = 300, GRP_W = 330;
const GPU_X = 730, GPU_W = 210;
const TOP = 40, HEAD = 30, ROW_H = 44, PAD = 8, GAP = 16;

export function topologySvg(m: DashboardModel): string {
  const groups = topologyGroups(m);
  const gpus = m.gpus;
  // Containers stacked down the middle column; their height sets the
  // canvas, and the GPU column spreads over the same height.
  const heights = groups.map((g) => HEAD + Math.max(1, g.rows.length) * ROW_H + PAD);
  const stack = heights.reduce((a, h) => a + h, 0) + GAP * Math.max(0, groups.length - 1);
  const H = Math.max(320, TOP + Math.max(stack, gpus.length * (NODE_H + 16)) + 20);
  const gpuY = (j: number): number => TOP + (H - TOP - 20) * ((j + 0.5) / Math.max(1, gpus.length));
  const gwY = H / 2;

  let edges = "";
  let body = "";
  const edge = (x1: number, y1: number, x2: number, y2: number, cls: string, label = ""): void => {
    const mx = (x1 + x2) / 2;
    edges += `<path class="map-edge ${cls}" d="M${x1},${y1} C${mx},${y1} ${mx},${y2} ${x2},${y2}"/>`;
    if (label) edges += `<text class="map-edge-label" x="${mx}" y="${(y1 + y2) / 2 - 4}" text-anchor="middle">${esc(label)}</text>`;
  };
  const node = (x: number, y: number, w: number, title: string, sub: string, dot: string): void => {
    body += `<g><title>${esc(`${title}\n${sub}`)}</title><rect class="map-node" x="${x}" y="${y - NODE_H / 2}" width="${w}" height="${NODE_H}" rx="8"/>
      <circle cx="${x + 14}" cy="${y - 8}" r="4" fill="${dot}"/>
      <text class="map-node-text" x="${x + 26}" y="${y - 2}">${esc(fitText(title, w - 36, 12, 0.6))}</text>
      <text class="map-node-sub" x="${x + 14}" y="${y + 16}">${esc(fitText(sub, w - 24, 10))}</text></g>`;
  };

  // Gateway.
  const gw = m.gateway;
  node(GW_X, gwY, GW_W, "LiteLLM",
    gw
      ? `:${gw.port}${gw.lifecycle !== "running" ? ` · ${lifecycleLabel(gw.lifecycle)}` : m.gatewayStrategy ? ` · ${m.gatewayStrategy}` : ""}`
      : "not reported",
    gw ? lifecycleColor(gw.lifecycle) : LIFECYCLE_COLORS.grey);
  const gwLive = gw ? META[gw.lifecycle].isLive : false;

  // GPUs.
  gpus.forEach((g, j) => node(GPU_X, gpuY(j), GPU_W, g.name, `${trim1(g.util_pct)}% util`, gpuColor(g.status)));
  const segGpu = (seg: PlacementSegment): number => {
    if (seg.gpuIndex !== undefined && gpus[seg.gpuIndex]) return seg.gpuIndex;
    return gpus.findIndex((g) => g.name === seg.gpu || g.id === seg.gpu);
  };
  // A placement edge asserts the split is loaded: only real (not
  // defaulted) placements of enabled, live backends draw one.
  const placementOf = (b: BackendInfo): PlacementRow | undefined =>
    m.placement.find((p) => p.backendId === b.id && !p.defaulted && p.backendEnabled !== false);

  // Groups.
  let y = TOP;
  groups.forEach((g, gi) => {
    const h = heights[gi];
    const routed = g.name !== null && g.enabled;
    const title = g.name ?? (m.groups ? "Not in a group" : "Backends");
    const deployed = g.rows.filter((r) => r.backend && r.backend.lifecycle === "running").length;
    const meta = g.name === null
      ? (m.groups ? "not routed by the gateway" : "groups not loaded")
      : `${deployed} of ${g.rows.length} deployed${g.enabled ? "" : " · disabled"}`;
    const tip = [title, meta, g.strategy ? `strategy: ${g.strategy}` : "", g.fallbacks.length ? `fallbacks: ${g.fallbacks.join(" → ")}` : ""].filter(Boolean).join("\n");
    body += `<g class="topo-group${routed ? "" : " loose"}"><title>${esc(tip)}</title>
      <rect class="topo-group-box" x="${GRP_X}" y="${y}" width="${GRP_W}" height="${h}" rx="10"/>
      <text class="map-node-text" x="${GRP_X + 12}" y="${y + 19}">${esc(fitText(title, GRP_W / 2 - 16, 12, 0.6))}</text>
      <text class="map-node-sub" x="${GRP_X + GRP_W - 12}" y="${y + 19}" text-anchor="end">${esc(fitText(meta, GRP_W / 2 - 4, 10))}</text></g>`;
    if (routed && gwLive) {
      const st = groupState(g);
      edge(GW_X + GW_W, gwY, GRP_X, y + HEAD / 2, META[st].isLive ? lifecycleEdge(st).cls : "dim");
    }
    if (!g.rows.length) {
      body += `<text class="map-node-sub" x="${GRP_X + 14}" y="${y + HEAD + ROW_H / 2}">no members</text>`;
    }
    g.rows.forEach((r, ri) => {
      const ry = y + HEAD + ri * ROW_H + ROW_H / 2 - 2;
      const b = r.backend;
      const st: LifecycleState | null = b ? b.lifecycle : null;
      const up = b !== undefined && b.lifecycle === "running";
      const name = b ? b.model || "no model" : r.member;
      const sub = b
        ? `${b.id} :${b.port} · ${b.enabled === false ? "disabled" : st === "running" && b.throughput_toks !== undefined ? `${trim1(b.throughput_toks)} tok/s` : lifecycleLabel(b.lifecycle)}`
        : "external";
      body += `<g${up ? "" : ` class="map-dim"`}><title>${esc(`${name}\n${sub}`)}</title>
        <rect class="map-node" x="${GRP_X + 10}" y="${ry - 18}" width="${GRP_W - 20}" height="36" rx="6"/>
        <circle cx="${GRP_X + 22}" cy="${ry - 5}" r="4" fill="${st ? lifecycleColor(st) : LIFECYCLE_COLORS.grey}"/>
        <text class="map-node-text" x="${GRP_X + 32}" y="${ry - 1}">${esc(fitText(name, GRP_W - 56, 12, 0.6))}</text>
        <text class="map-node-sub" x="${GRP_X + 22}" y="${ry + 13}">${esc(fitText(sub, GRP_W - 44, 10))}</text></g>`;
      const p = b && statusLive(b.status) && b.enabled !== false ? placementOf(b) : undefined;
      for (const seg of p?.segments ?? []) {
        const j = segGpu(seg);
        if (j >= 0) edge(GRP_X + GRP_W - 10, ry, GPU_X, gpuY(j), lifecycleEdge(b!.lifecycle).cls, p!.segments.length > 1 ? `${seg.pct}%` : "");
      }
    });
    y += h + GAP;
  });

  return `<svg width="${W}" height="${H}" viewBox="0 0 ${W} ${H}" style="display:block;width:100%;height:auto" role="img" aria-label="Cluster topology: gateway, model groups and their models, GPUs">
    ${edges}${body}
    <text class="map-col-label" x="${GW_X}" y="24">GATEWAY</text>
    <text class="map-col-label" x="${GRP_X}" y="24">GROUPS · MODELS</text>
    <text class="map-col-label" x="${GPU_X}" y="24">GPUS</text></svg>`;
}
