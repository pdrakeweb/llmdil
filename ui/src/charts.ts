/* Hand-rolled SVG chart helpers — no external charting library.
   All charts render offline from plain number arrays. */

import { esc } from "./html";

function points(values: number[], w: number, h: number, pad = 1): string {
  if (values.length === 0) return "";
  const min = Math.min(...values);
  const max = Math.max(...values);
  const span = max - min || 1;
  const stepX = values.length > 1 ? (w - pad * 2) / (values.length - 1) : 0;
  return values
    .map((v, i) => {
      const x = (pad + i * stepX).toFixed(1);
      const y = (h - pad - ((v - min) / span) * (h - pad * 2)).toFixed(1);
      return `${x},${y}`;
    })
    .join(" ");
}

/** Area sparkline with line on top. */
export function sparkline(
  values: number[],
  w: number,
  h: number,
  color: string,
  fillOpacity = 0.18,
): string {
  const pts = points(values, w, h);
  if (!pts) return `<svg width="${w}" height="${h}"></svg>`;
  const area = `${1},${h} ${pts} ${w - 1},${h}`;
  return `<svg width="${w}" height="${h}" viewBox="0 0 ${w} ${h}" aria-hidden="true">
    <polygon points="${area}" fill="${color}" opacity="${fillOpacity}"/>
    <polyline points="${pts}" fill="none" stroke="${color}" stroke-width="1.5"/>
  </svg>`;
}

/** Donut gauge ring with centered label. */
export function gaugeRing(
  pct: number, // 0-100
  size: number,
  color: string,
  centerTop: string,
  centerBottom: string,
): string {
  const r = (size - 10) / 2;
  const c = 2 * Math.PI * r;
  const off = c * (1 - Math.min(100, Math.max(0, pct)) / 100);
  const s = size / 2;
  return `<span class="ring" style="width:${size}px;height:${size}px">
    <svg width="${size}" height="${size}" viewBox="0 0 ${size} ${size}" aria-hidden="true">
      <circle cx="${s}" cy="${s}" r="${r}" fill="none" stroke="#2b313b" stroke-width="6"/>
      <circle cx="${s}" cy="${s}" r="${r}" fill="none" stroke="${color}" stroke-width="6"
        stroke-linecap="round" stroke-dasharray="${c.toFixed(1)}" stroke-dashoffset="${off.toFixed(1)}"
        transform="rotate(-90 ${s} ${s})"/>
    </svg>
    <span class="ring-label"><b>${esc(centerTop)}</b><span>${esc(centerBottom)}</span></span>
  </span>`;
}

/** Horizontal stacked bar. segments: {pct, color}. */
export function stackedBar(
  segments: { pct: number; color: string; label?: string }[],
  width = 300,
  height = 10,
): string {
  let x = 0;
  const rects = segments
    .map((s) => {
      const w = (s.pct / 100) * width;
      const el = `<rect x="${x.toFixed(1)}" y="0" width="${Math.max(0, w - 1).toFixed(1)}" height="${height}" rx="3" fill="${s.color}">${s.label ? `<title>${esc(s.label)}</title>` : ""}</rect>`;
      x += w;
      return el;
    })
    .join("");
  return `<svg width="${width}" height="${height}" viewBox="0 0 ${width} ${height}" style="display:block;width:100%" preserveAspectRatio="none" aria-hidden="true">
    <rect x="0" y="0" width="${width}" height="${height}" rx="3" fill="#2a313c"/>${rects}</svg>`;
}

/** Simple horizontal meter bar (single value). */
export function meter(pct: number, width: number, height: number, color: string): string {
  const w = Math.min(100, Math.max(0, pct));
  return `<svg width="${width}" height="${height}" viewBox="0 0 ${width} ${height}" style="display:block;width:100%" preserveAspectRatio="none" aria-hidden="true">
    <rect x="0" y="0" width="${width}" height="${height}" rx="2" fill="#2a313c"/>
    <rect x="0" y="0" width="${((w / 100) * width).toFixed(1)}" height="${height}" rx="2" fill="${color}"/>
  </svg>`;
}

/** Small deterministic pseudo-random walk for mock history series. */export function walk(n: number, base: number, amp: number, seed: number): number[] {
  let s = seed;
  const out: number[] = [];
  let v = base;
  for (let i = 0; i < n; i++) {
    s = (s * 1103515245 + 12345) & 0x7fffffff;
    v += ((s / 0x7fffffff) - 0.5) * amp;
    v = Math.max(base - amp, Math.min(base + amp, v));
    out.push(Math.round(v * 10) / 10);
  }
  return out;
}

/** Number presentation — mirrors the canonical Rust rules in
 *  crates/manager-present: at most one decimal, a trailing ".0"
 *  dropped (16.0 -> "16", 15.1 -> "15.1"), whole numbers at
 *  magnitude >= 100, non-finite -> em dash. */
export function trim1(v: number): string {
  if (!Number.isFinite(v)) return "\u2014";
  if (Math.abs(v) >= 100) return String(Math.round(v));
  const s = v.toFixed(1);
  return s.endsWith(".0") ? s.slice(0, -2) : s;
}

/** A GB/GiB quantity under the trim1 rule: fmtGb(15.9921875) is "16". */
export function fmtGb(v: number): string {
  return trim1(v);
}

/** A count: integer with thousands separators (1,234). Counts are
 *  never decimal — this is the one formatter for request/error/group
 *  counts everywhere (principle 14). */
export function fmtCount(v: number): string {
  return Math.round(v).toLocaleString("en-US");
}

/** A percentage from a 0-1 fraction: integer when whole, one decimal
 *  otherwise (trim1 rule). Chart widths and their tooltip values are
 *  computed from the same fraction so they can never disagree. */
export function fmtPct(frac: number): string {
  return trim1(frac * 100);
}

/* ---------------- Time-series history chart ----------------
   One line per series over a shared time axis, for the GPUs page
   history section. Every drawn point is a real sample (or a stated
   average of real samples, produced server-side): a null value or
   a time gap wider than 3x the series' nominal interval breaks
   the line — gaps are drawn as gaps, never interpolated across an
   app-off period. The geometry is returned alongside the SVG so
   the view's hover handler can map pointer positions back to
   timestamps with the same scales the renderer used. */

export interface HistoryChartPoint {
  ts: number;
  v: number | null;
}

export interface HistoryChartSeries {
  label: string;
  color: string;
  /** Nominal spacing of the points (ms); wider dt = a gap. */
  intervalMs: number;
  points: HistoryChartPoint[];
}

export interface HistoryChartGeom {
  w: number;
  h: number;
  padL: number;
  padR: number;
  padT: number;
  padB: number;
  t0: number;
  t1: number;
  vmin: number;
  vmax: number;
}

export function historyChartX(g: HistoryChartGeom, ts: number): number {
  return g.padL + ((ts - g.t0) / Math.max(1, g.t1 - g.t0)) * (g.w - g.padL - g.padR);
}

export function historyChartY(g: HistoryChartGeom, v: number): number {
  return g.padT + (1 - (v - g.vmin) / Math.max(1e-9, g.vmax - g.vmin)) * (g.h - g.padT - g.padB);
}

export interface HistoryChartOpts {
  w?: number;
  h?: number;
  /** Fixed value range (e.g. utilization 0-100); default: data range, padded. */
  fixedRange?: [number, number];
  /** Axis time label for a timestamp (range-dependent format). */
  fmtTime: (ts: number) => string;
}

export function historyChart(
  series: HistoryChartSeries[],
  opts: HistoryChartOpts,
): { svg: string; geom: HistoryChartGeom } {
  const w = opts.w ?? 720;
  const h = opts.h ?? 190;
  const geom: HistoryChartGeom = {
    w, h, padL: 44, padR: 10, padT: 8, padB: 20,
    t0: 0, t1: 1, vmin: 0, vmax: 1,
  };
  const all = series.flatMap((s) => s.points);
  if (!all.length) return { svg: "", geom };
  geom.t0 = Math.min(...all.map((p) => p.ts));
  geom.t1 = Math.max(...all.map((p) => p.ts));
  if (geom.t1 <= geom.t0) geom.t1 = geom.t0 + 1;
  const vals = all.map((p) => p.v).filter((v): v is number => v !== null);
  if (opts.fixedRange) {
    [geom.vmin, geom.vmax] = opts.fixedRange;
  } else if (vals.length) {
    geom.vmin = Math.min(...vals);
    geom.vmax = Math.max(...vals);
    const pad = (geom.vmax - geom.vmin) * 0.08 || 1;
    geom.vmin -= pad;
    geom.vmax += pad;
    if (geom.vmin < 0 && Math.min(...vals) >= 0) geom.vmin = 0;
  }
  const plotW = w - geom.padL - geom.padR;
  const plotH = h - geom.padT - geom.padB;
  const gridY = [geom.vmax, (geom.vmin + geom.vmax) / 2, geom.vmin]
    .map((v) => {
      const y = historyChartY(geom, v);
      return `<line x1="${geom.padL}" y1="${y.toFixed(1)}" x2="${w - geom.padR}" y2="${y.toFixed(1)}" stroke="#232a34" stroke-width="1"/>`
        + `<text x="${geom.padL - 6}" y="${(y + 3).toFixed(1)}" text-anchor="end" font-size="10" fill="var(--text3)">${esc(trim1(v))}</text>`;
    })
    .join("");
  const gridX = [geom.t0, (geom.t0 + geom.t1) / 2, geom.t1]
    .map((t, i) => {
      const x = historyChartX(geom, t);
      const anchor = i === 0 ? "start" : i === 2 ? "end" : "middle";
      return `<text x="${x.toFixed(1)}" y="${h - 5}" text-anchor="${anchor}" font-size="10" fill="var(--text3)">${esc(opts.fmtTime(Math.round(t)))}</text>`;
    })
    .join("");
  const paths = series
    .map((s) => {
      // Split into contiguous segments: a null value (not measured
      // at that tick) or a time gap wider than 3x the nominal
      // interval ends the segment — the line never connects across
      // either.
      const segs: { x: number; y: number }[][] = [];
      let cur: { x: number; y: number }[] = [];
      let prevTs: number | null = null;
      let broken = true;
      for (const p of s.points) {
        if (p.v === null) {
          if (cur.length) segs.push(cur);
          cur = [];
          broken = true;
          continue;
        }
        if (!broken && prevTs !== null && p.ts - prevTs > 3 * s.intervalMs) {
          broken = true;
        }
        if (broken && cur.length) {
          segs.push(cur);
          cur = [];
        }
        broken = false;
        cur.push({ x: historyChartX(geom, p.ts), y: historyChartY(geom, p.v) });
        prevTs = p.ts;
      }
      if (cur.length) segs.push(cur);
      return segs
        .map((seg) => {
          if (seg.length === 1) {
            // An isolated point has no line to draw; mark it with
            // a dot so a lone real sample is visible as itself.
            return `<circle cx="${seg[0].x.toFixed(1)}" cy="${seg[0].y.toFixed(1)}" r="1.8" fill="${s.color}"/>`;
          }
          const d = seg
            .map((pt, i) => `${i === 0 ? "M" : "L"} ${pt.x.toFixed(1)} ${pt.y.toFixed(1)}`)
            .join(" ");
          return `<path d="${d}" fill="none" stroke="${s.color}" stroke-width="1.5"/>`;
        })
        .join("");
    })
    .join("");
  const svg = `<svg viewBox="0 0 ${w} ${h}" style="display:block;width:100%;height:auto" aria-hidden="true">`
    + `${gridY}${gridX}${paths}`
    + `<line class="hist-guide" x1="0" y1="${geom.padT}" x2="0" y2="${h - geom.padB}" stroke="var(--text3)" stroke-width="1" opacity="0"/>`
    + `<g class="hist-markers"></g>`
    + `<rect class="hist-zone" x="${geom.padL}" y="${geom.padT}" width="${plotW}" height="${plotH}" fill="transparent"/>`
    + `</svg>`;
  return { svg, geom };
}
