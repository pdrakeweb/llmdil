/* Shared helpers for the secondary views (split from static.ts):
   topbar/banner/empty-state markup, formatters, short names. */

import type { BackendRow, ModelRow } from "../api";
import { trim1 } from "../charts";
import { esc } from "../html";
import { icon } from "../icons";
import type { IconName } from "../icons";

export const C = { blue: "#4a9eff", purple: "#a371f7", teal: "#39c5cf", green: "#3fb950", amber: "#d29922", red: "#f85149", grey: "#6d7686" };

export const SELECT_STYLE = "background:var(--panel);color:var(--text);border:1px solid var(--border);border-radius:8px;padding:6px 8px;font-size:12px";

export function topbar(title: string, extra = ""): string {
  return `<div class="topbar"><h1>${title}</h1>
    <span class="spacer"></span>${extra}</div>`;
}

export function dot(s: string): string {
  return `<span class="dot ${s === "ok" ? "ok" : s === "warn" || s === "starting" || s === "degraded" ? "warn" : s === "err" ? "err" : "idle"}"></span>`;
}


/** Status/health button for an overview-list row: a colored dot and
 *  icon; clicking it scrolls to the item's card (data-ov-goto holds
 *  the card's element id). kind colors both the dot and the icon. */
export function ovStatusBtn(kind: "ok" | "warn" | "err" | "idle", ic: IconName, target: string, label: string): string {
  return `<button class="ov-goto ${kind}" data-ov-goto="${esc(target)}" title="${esc(label)}">${dot(kind)}${icon(ic)}</button>`;
}

/** Shared click handler for overview status buttons: scroll the
 *  target card into view and flash its outline briefly. */
export function onOvGoto(ev: Event): void {
  const t = (ev.target as HTMLElement).closest("[data-ov-goto]") as HTMLElement | null;
  if (!t) return;
  const el = document.getElementById(t.getAttribute("data-ov-goto") ?? "");
  if (!el) return;
  el.scrollIntoView({ behavior: "smooth", block: "start" });
  el.classList.add("flash");
  window.setTimeout(() => el.classList.remove("flash"), 1300);
}

interface LoadMeta { live: boolean; error?: string }

/** Banner for a wired view: backend error, sample mode, or (GPUs) stale data. */
export function bannerHtml(r: LoadMeta, stale = false): string {
  if (r.error) return `<div class="banner" style="margin-bottom:12px">${icon("err")} Backend error \u2014 ${esc(r.error)}</div>`;
  if (!r.live) return `<div class="banner" style="margin-bottom:12px" title="Open the installed app for live data from this machine.">${icon("warn")} Sample mode</div>`;
  if (stale) return `<div class="banner" style="margin-bottom:12px">${icon("warn")} Telemetry is stale \u2014 showing the last received sample.</div>`;
  return "";
}

export function setBanner(root: HTMLElement, id: string, html: string): void {
  const el = root.querySelector(`#${id}`);
  if (el) el.innerHTML = html;
}

export function loadingHtml(label: string): string {
  return `<div class="card" style="color:var(--text2)">${icon("clock")} ${esc(label)}</div>`;
}

export function emptyHtml(text: string): string {
  return `<div class="card" style="color:var(--text2)">${esc(text)}</div>`;
}

export function fmtBytes(n: number | null): string {
  if (n === null) return "\u2014";
  if (n <= 0) return "0 B";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let v = n;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) { v /= 1024; u++; }
  return `${v >= 100 ? v.toFixed(0) : trim1(v)} ${units[u]}`;
}

export function fmtUptime(secs: number | null): string {
  if (secs === null) return "\u2014";
  const s = Math.floor(secs);
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${s % 60}s`;
  return `${s}s`;
}

/** Elapsed for an in-flight job, in the Optimize view's format
 *  ("45 s", "2 min 5 s"): the predicted-throughput refresh's
 *  Measuring state ticks in this shape. */
export function fmtElapsed(secs: number): string {
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  return m > 0 ? `${m} min ${s} s` : `${s} s`;
}

export function fmtTime(ts: string): string {
  const d = new Date(ts);
  return Number.isNaN(d.getTime()) ? ts : d.toLocaleTimeString();
}

export function fmtDate(ts: string): string {
  const d = new Date(ts);
  return Number.isNaN(d.getTime()) ? ts : d.toLocaleDateString();
}

/** The backend cadence buckets: Off, anything up to 1500 ms runs at 1s,
 *  everything above runs at 5s. Used both to initialize the poll select
 *  from the configured value and to word the change confirmation with
 *  the EFFECTIVE cadence. */
export function cadenceBucket(ms: number): { value: string; label: string } {
  if (ms <= 0) return { value: "0", label: "off" };
  if (ms <= 1500) return { value: "1000", label: "1s" };
  return { value: "5000", label: "5s" };
}

export function baseName(p: string): string {
  return p.split(/[\\/]/).pop() ?? p;
}

/** Does a backend's model_file resolve to a model's gguf_path?
 *  model_assign writes the gguf_path verbatim, so an exact match is
 *  the assigned state; a basename match covers configs written with
 *  a different directory prefix. */
export function sameModelFile(modelFile: string, ggufPath: string): boolean {
  return modelFile !== "" && (modelFile === ggufPath || baseName(modelFile) === baseName(ggufPath));
}

/* ---------------- Backends ---------------- */

export type CardNote = { text: string; err: boolean };

/** Quote an argv element for display when it contains whitespace or a
 *  quote, so the rendered launch command stays copy-paste accurate. */
export function quoteArg(a: string): string {
  return /[\s"]/.test(a) ? `"${a.replace(/"/g, '\\"')}"` : a;
}

/** Short label for model controls: the catalog display name when
 *  known, otherwise the model id. The GGUF filename is deliberately
 *  omitted — it duplicates the identity at much greater width. */
export function modelShortName(m: ModelRow): string {
  return m.name?.trim() || m.id;
}

/** Compact GPU name for the Placement split chips: the vendor
 *  prefix is dropped ("NVIDIA RTX A4000" -> "RTX A4000") so the
 *  name fits on the chip's second line. The full telemetry name
 *  stays available as the chip's tooltip. */
export function gpuShortName(name: string): string {
  return name.replace(/^nvidia\s+/i, "").trim() || name;
}

/** Short label for an unregistered model file: its filename stem,
 *  without the directory or the .gguf suffix. */
export function modelFileShortName(path: string): string {
  return path ? baseName(path).replace(/\.gguf$/i, "") : "no model assigned";
}

/** Options for a backend's model dropdown: every registered
 *  model, labeled by short name only. With no `selectedId`, the
 *  model the backend currently serves is selected; the Edit
 *  panel passes its draft id instead, so a poll repaint renders
 *  the operator's unsaved choice, not the saved model (the draft
 *  is what Save will apply). When the backend's model_file matches
 *  no registered model (a custom path), a leading option carries
 *  the current file's short name so the control shows the true
 *  state; choosing it is a no-op. */
export function modelOptions(b: BackendRow, models: ModelRow[], selectedId?: string): string {
  const matched = models.some((m) => sameModelFile(b.model_file, m.gguf_path));
  const lead = matched ? "" : `<option value=""${selectedId === undefined || selectedId === "" ? " selected" : ""}>${esc(modelFileShortName(b.model_file))} (current)</option>`;
  return lead + models.map((m) => {
    const sel = selectedId !== undefined ? m.id === selectedId : sameModelFile(b.model_file, m.gguf_path);
    return `<option value="${esc(m.id)}"${sel ? " selected" : ""}>${esc(modelShortName(m))}</option>`;
  }).join("");
}
