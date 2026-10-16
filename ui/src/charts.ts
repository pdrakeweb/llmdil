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

/** Multi-series line chart with area fills (throughput history). */
export function multiLine(
  series: { name: string; color: string; values: number[] }[],
  w: number,
  h: number,
): string {
  const all = series.flatMap((s) => s.values);
  const min = Math.min(...all, 0);
  const max = Math.max(...all, 1);
  const span = max - min || 1;
  const n = Math.max(...series.map((s) => s.values.length));
  const stepX = n > 1 ? (w - 4) / (n - 1) : 0;
  const xy = (v: number, i: number) =>
    `${(2 + i * stepX).toFixed(1)},${(h - 4 - ((v - min) / span) * (h - 8)).toFixed(1)}`;
  const paths = series
    .map((s) => {
      const pts = s.values.map(xy).join(" ");
      const area = `2,${h} ${pts} ${(w - 2).toFixed(1)},${h}`;
      return `<polygon points="${area}" fill="${s.color}" opacity="0.10"/>
              <polyline points="${pts}" fill="none" stroke="${s.color}" stroke-width="1.5"/>`;
    })
    .join("");
  return `<svg width="${w}" height="${h}" viewBox="0 0 ${w} ${h}" style="display:block;width:100%" preserveAspectRatio="none" aria-hidden="true">${paths}</svg>`;
}

/** Small deterministic pseudo-random walk for mock history series. */
export function walk(n: number, base: number, amp: number, seed: number): number[] {
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
