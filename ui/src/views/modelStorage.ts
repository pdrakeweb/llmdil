/* Tiered model storage on the Models view (docs/tiered-model-storage.md
   §11): the Storage card at the top of the view, and the storage
   chips, actions and tiles of each model card. Pure renderers — the
   Models view (models.ts) owns the state, the polling and the clicks. */

import { isColdTier } from "../api";
import type { ModelRow, ModelStorageRow, ModelStorageStatus, StorageFileStatus, StorageMoveRecord, StorageMoving, StorageTierStatus } from "../api";
import { meter, stackedBar, trim1 } from "../charts";
import { esc } from "../html";
import { icon } from "../icons";
import { enableSwitch } from "../switch";
import { C, fmtBytes, fmtDate, fmtTime, modelFileShortName, modelShortName } from "./common";

const GIB = 1024 ** 3;

/** Per-file segment colors: series colors only — none of the status
 *  colors (green / amber / red), so a segment never reads as a state. */
const FILE_COLORS = [C.blue, C.purple, C.teal, "#5b8def", "#c678dd", "#56b6c2", "#8fa3ff", "#b48ead"];
/** Non-model bytes on the volume. */
const OTHER_COLOR = C.grey;
/** Threshold markers on the hot bar. */
const RESERVE_COLOR = "#e6edf3";
const BUDGET_COLOR = "#ff9bce";

/** View state the Storage card renders from (held by wireModels). */
export interface StorageCardUi {
  unregOpen: boolean;
  movesOpen: boolean;
  note: { text: string; err: boolean } | null;
}

/** Whether any cold tier is configured, and whether one is reachable
 *  (demotion needs a reachable one). */
export interface StorageCtx {
  hasCold: boolean;
  coldAvailable: boolean;
}

export function storageCtx(st: ModelStorageStatus | null): StorageCtx {
  const cold = (st?.tiers ?? []).filter((t) => isColdTier(t.tier));
  return { hasCold: cold.length > 0, coldAvailable: cold.some((t) => t.available) };
}

/** Is a move for this file in flight (running or queued)? */
export function storageMoving(st: ModelStorageStatus | null, rows: readonly ModelRow[]): boolean {
  return (st?.files ?? []).some((f) => f.moving) || rows.some((m) => m.storage?.moving);
}

/** "Hot", or the cold tier's label ("Cold" / "Cold 2" without one). */
export function tierDisplayName(t: Pick<StorageTierStatus, "tier" | "label">): string {
  if (t.tier === "hot") return "Hot";
  if (t.label.trim()) return t.label.trim();
  const n = /^cold:(\d+)$/.exec(t.tier);
  return n ? `Cold ${Number(n[1]) + 1}` : "Cold";
}

/** "3 min ago" / "5 h ago" / "2 days ago"; a date beyond 30 days. */
export function fmtAgo(ts: string, now = Date.now()): string {
  const t = Date.parse(ts);
  if (Number.isNaN(t)) return ts;
  const s = Math.max(0, (now - t) / 1000);
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)} min ago`;
  if (s < 86400) return `${Math.floor(s / 3600)} h ago`;
  if (s < 30 * 86400) {
    const d = Math.floor(s / 86400);
    return `${d} day${d === 1 ? "" : "s"} ago`;
  }
  return fmtDate(ts);
}

/** Whole percent of a move's 0..1 fraction (never rounds up to 100
 *  before the move finishes). */
function movePct(m: StorageMoving): number {
  return Math.min(100, Math.floor((m.fraction ?? 0) * 100));
}

/** The amber chip of an in-flight move: "Promoting 42%" /
 *  "Demoting 42%", or "Queued" behind another move. */
export function moveChipHtml(m: StorageMoving): string {
  const verb = m.kind === "promote" ? "Promoting" : "Demoting";
  if (m.queued) {
    return `<span class="chip badge-warn" title="${m.kind === "promote" ? "Promotion" : "Demotion"} queued behind another move">Queued</span>`;
  }
  return `<span class="chip badge-warn">${verb} ${movePct(m)}%</span>${m.paused ? ` <span class="chip badge-warn" title="The copy pauses while a backend is starting.">Paused</span>` : ""}`;
}

/** Model card chips: Cold (gray; hot is the default, so no chip),
 *  the move in flight (amber), Unavailable (light red), Archived
 *  (gray) for a hot model with a cold archive copy. */
export function modelStorageChipsHtml(s: ModelStorageRow | null | undefined): string {
  if (!s) return "";
  const chips: string[] = [];
  if (isColdTier(s.tier)) chips.push(`<span class="chip" title="Stored on the cold tier: ${esc(s.path)}">Cold</span>`);
  if (s.tier === "missing" && s.cold_unavailable) {
    chips.push(`<span class="chip badge-err" title="The model's file is on a cold tier that is not reachable.">Unavailable</span>`);
  }
  if (s.tier === "hot" && s.archived) {
    chips.push(`<span class="chip" title="A verified copy is archived on the cold tier; demoting this model only deletes the hot copy.">Archived</span>`);
  }
  if (s.moving) chips.push(moveChipHtml(s.moving));
  return chips.join(" ");
}

/** Icon-only Promote / Demote / Cancel-move buttons for a target
 *  (model id or logical path), under the given data-* action
 *  attribute. Demote of a pinned file renders disabled with the pin
 *  reason as its tooltip. */
export function moveButtonsHtml(o: {
  actAttr: string;
  idAttr: string;
  target: string;
  name: string;
  tier: string | null;
  moving: StorageMoving | null;
  pinned: string | null;
  ctx: StorageCtx;
}): string {
  const a = (act: string): string => `${o.actAttr}="${act}" ${o.idAttr}="${esc(o.target)}"`;
  if (o.moving) {
    return `<button class="btn icon-only" ${a("move-cancel")} title="Cancel move" aria-label="Cancel move of ${esc(o.name)}">${icon("x")}</button>`;
  }
  if (isColdTier(o.tier)) {
    return `<button class="btn icon-only" ${a("promote")} title="Promote to hot" aria-label="Promote ${esc(o.name)} to hot">${icon("arrowUp")}</button>`;
  }
  if (o.tier === "hot" && o.ctx.coldAvailable) {
    return o.pinned
      ? `<button class="btn icon-only" ${a("demote")} title="${esc(`Demote to cold: unavailable — ${o.pinned}`)}" aria-label="Demote ${esc(o.name)} to cold (unavailable: ${esc(o.pinned)})" disabled>${icon("arrowDown")}</button>`
      : `<button class="btn icon-only" ${a("demote")} title="Demote to cold" aria-label="Demote ${esc(o.name)} to cold">${icon("arrowDown")}</button>`;
  }
  return "";
}

/** Model card actions for storage (see moveButtonsHtml). */
export function modelStorageActionsHtml(m: ModelRow, ctx: StorageCtx): string {
  const s = m.storage;
  if (!s) return "";
  return moveButtonsHtml({
    actAttr: "data-model-act", idAttr: "data-model-id", target: m.id, name: m.id,
    tier: s.tier, moving: s.moving, pinned: s.pinned, ctx,
  });
}

/** Model card tiles: the Keep hot switch (only with a cold tier —
 *  without one there is nothing to keep a model hot against) and
 *  Last started (only when recorded). A backend pin shows as the
 *  switch disabled-on, naming the backend in its tooltip. */
export function modelStorageTilesHtml(m: ModelRow, ctx: StorageCtx): string {
  const s = m.storage;
  if (!s) return "";
  const tiles: string[] = [];
  if (ctx.hasCold) {
    const byBackend = s.pinned !== null && !s.keep_hot;
    tiles.push(`<div class="kv"><span class="k">Keep hot</span><span>${enableSwitch({
      on: s.keep_hot || byBackend,
      name: `Keep ${m.id} hot`,
      attrs: `data-model-act="keep-hot" data-model-id="${esc(m.id)}"`,
      disabled: byBackend,
      title: byBackend ? `Keep hot: on — ${s.pinned}` : undefined,
    })}</span></div>`);
  }
  if (s.last_started) {
    tiles.push(`<div class="kv"><span class="k">Last started</span><span class="v" style="font-size:12px" title="${esc(fmtDate(s.last_started))} ${esc(fmtTime(s.last_started))}">${esc(fmtAgo(s.last_started))}</span></div>`);
  }
  return tiles.join("");
}

/* ---------------- Storage card ---------------- */

type NameOf = (file: string, modelId: string | null) => string;

function tierHtml(t: StorageTierStatus, st: ModelStorageStatus, colorOf: (file: string) => string, nameOf: NameOf): string {
  const name = tierDisplayName(t);
  const head = `<div style="display:flex;gap:8px;align-items:center;flex-wrap:wrap;min-width:0">
      <span style="font-weight:600;font-size:12px">${esc(name)}</span>
      ${t.available ? "" : `<span class="chip badge-err" title="The tier's directory is not reachable (drive missing or disconnected). Models on it cannot start until it is back.">Unavailable</span>`}
      <span style="font-family:var(--mono);font-size:11px;color:var(--text2);word-break:break-all;min-width:0">${esc(t.path)}</span>
      <button class="btn" data-storage-act="copy-path" data-storage-target="${esc(t.path)}" title="Copy tier path" aria-label="Copy ${esc(name)} tier path" style="padding:1px 7px;font-size:11px">${icon("copy")}</button>
    </div>`;
  const total = t.total_bytes;
  const free = t.free_bytes;
  const scale = total !== null && total > 0 ? total : t.model_bytes;
  if (!(scale > 0)) return `<div style="margin-top:10px">${head}</div>`;
  const pct = (b: number): number => (b / scale) * 100;
  const segments = t.files.map((f) => ({ pct: pct(f.bytes), color: colorOf(f.file), label: `${nameOf(f.file, f.model_id)}: ${fmtBytes(f.bytes)}` }));
  if (total !== null && free !== null) {
    const other = total - free - t.model_bytes;
    if (other > 0) segments.push({ pct: pct(other), color: OTHER_COLOR, label: `Other files: ${fmtBytes(other)}` });
  }
  const markers: { pct: number; color: string; label: string }[] = [];
  const keys: string[] = [];
  if (t.tier === "hot" && total !== null && total > 0) {
    const reserve = st.config.hot_min_free_gib * GIB;
    if (reserve > 0 && reserve < total) {
      markers.push({ pct: pct(total - reserve), color: RESERVE_COLOR, label: `Free-space reserve: ${trim1(st.config.hot_min_free_gib)} GiB` });
      keys.push(`<span title="Models are demoted to keep this much of the hot volume free."><span style="display:inline-block;width:3px;height:10px;background:${RESERVE_COLOR};vertical-align:-1px;margin-right:4px"></span>Reserve: ${trim1(st.config.hot_min_free_gib)} GiB</span>`);
    }
    const budget = st.config.hot_budget_gib;
    if (budget !== null && budget * GIB < total) {
      markers.push({ pct: pct(budget * GIB), color: BUDGET_COLOR, label: `Hot budget: ${trim1(budget)} GiB` });
      keys.push(`<span title="Cap on model bytes in the hot tier."><span style="display:inline-block;width:3px;height:10px;background:${BUDGET_COLOR};vertical-align:-1px;margin-right:4px"></span>Budget: ${trim1(budget)} GiB</span>`);
    }
  }
  const values = [
    `Models: ${fmtBytes(t.model_bytes)}`,
    free !== null ? `Free: ${fmtBytes(free)}` : "",
    total !== null ? `Disk: ${fmtBytes(total)}` : "",
  ].filter(Boolean).map((v) => `<span>${esc(v)}</span>`);
  return `<div style="margin-top:10px">${head}
    <div style="margin-top:6px">${stackedBar(segments, 600, 12, markers)}</div>
    <div style="display:flex;gap:6px 14px;flex-wrap:wrap;font-size:11px;color:var(--text2);margin-top:4px">${[...values, ...keys].join("")}</div>
  </div>`;
}

function movesInFlightHtml(files: StorageFileStatus[], nameOf: NameOf): string {
  const moving = files.filter((f) => f.moving);
  if (!moving.length) return "";
  return `<div style="margin-top:12px;display:grid;gap:6px">${moving.map((f) => {
    const m = f.moving as StorageMoving;
    const n = nameOf(f.file, f.model_id);
    const kind = m.kind === "promote" ? "Promote" : "Demote";
    return `<div style="display:flex;gap:8px;align-items:center;flex-wrap:wrap;font-size:12px;min-width:0">
      <span title="${kind}" aria-label="${kind}" style="display:inline-flex">${icon(m.kind === "promote" ? "arrowUp" : "arrowDown")}</span>
      <span style="font-weight:600;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap" title="${esc(f.file)}">${esc(n)}</span>
      ${m.queued
        ? `<span class="chip badge-warn" title="${kind} queued behind another move">Queued</span><span style="flex:1"></span>`
        : `<span style="flex:1 1 120px;min-width:80px">${meter(movePct(m), 300, 8, C.blue)}</span><span style="color:var(--text2)">${movePct(m)}%</span>${m.paused ? `<span class="chip badge-warn" title="The copy pauses while a backend is starting.">Paused</span>` : ""}`}
      <button class="btn icon-only" data-storage-act="move-cancel" data-storage-target="${esc(f.file)}" title="Cancel move" aria-label="Cancel move of ${esc(n)}">${icon("x")}</button>
    </div>`;
  }).join("")}</div>`;
}

function unregisteredHtml(files: StorageFileStatus[], ui: StorageCardUi, ctx: StorageCtx, nameOf: NameOf): string {
  const unreg = files.filter((f) => !f.registered);
  if (!unreg.length) return "";
  const rows = ui.unregOpen ? `<div style="display:grid;gap:4px;margin-top:6px">${unreg.map((f) => {
    const n = nameOf(f.file, null);
    return `<div style="display:flex;gap:8px;align-items:center;flex-wrap:wrap;font-size:12px;min-width:0">
      <span style="font-family:var(--mono);font-size:11px;min-width:0;flex:1 1 160px;word-break:break-all" title="${esc(f.file)}">${esc(n)}</span>
      ${isColdTier(f.tier) ? `<span class="chip">Cold</span>` : ""}${f.tier === "missing" ? `<span class="chip badge-err">Unavailable</span>` : ""}${f.moving ? moveChipHtml(f.moving) : ""}
      <span style="color:var(--text2)">${esc(fmtBytes(f.bytes))}</span>
      ${moveButtonsHtml({ actAttr: "data-storage-act", idAttr: "data-storage-target", target: f.file, name: n, tier: f.tier, moving: f.moving, pinned: f.pinned, ctx })}
    </div>`;
  }).join("")}</div>` : "";
  return `<div style="margin-top:12px">
    <button class="btn" data-storage-act="toggle-unreg" aria-expanded="${ui.unregOpen}" title="Unregistered files" aria-label="Unregistered files" style="padding:2px 8px;font-size:11px">${icon(ui.unregOpen ? "chevD" : "chevR")} Unregistered files: ${unreg.length}</button>
    ${rows}
  </div>`;
}

/** Warning per lost tier (a directory with recorded cold copies that
 *  is no longer configured), with a button that adds it back. */
export function lostTiersHtml(st: ModelStorageStatus): string {
  const lost = st.lost_tiers ?? [];
  return lost.map((t) => {
    const n = t.files === 1 ? "1 model file" : `${t.files} model files`;
    return `<div class="banner" style="margin:8px 0;display:flex;gap:8px;align-items:center;flex-wrap:wrap" role="alert">
      ${icon("warn")}<span style="flex:1 1 240px;min-width:0">${n} (${esc(fmtBytes(t.bytes))}) ${t.files === 1 ? "is" : "are"} on <span style="font-family:var(--mono);word-break:break-all">${esc(t.path)}</span>, which is no longer a configured cold tier, so ${t.files === 1 ? "it shows" : "they show"} as missing. This happens when a build without tiered storage saves the settings.</span>
      <button class="btn" data-storage-act="restore-tier" data-storage-target="${esc(t.path)}" title="Add ${esc(t.path)} back as a cold tier">${icon("plus")} Add back as cold tier</button>
    </div>`;
  }).join("");
}

function outcomeChip(r: StorageMoveRecord): string {
  if (r.outcome === "failed") return `<span class="chip badge-err"${r.error ? ` title="${esc(r.error)}"` : ""}>Failed</span>`;
  if (r.outcome === "running") return `<span class="chip badge-warn">Running</span>`;
  if (r.outcome === "cancelled") return `<span class="chip">Cancelled</span>`;
  if (r.outcome === "interrupted") return `<span class="chip"${r.error ? ` title="${esc(r.error)}"` : ""}>Interrupted</span>`;
  return "";
}

function movesHistoryHtml(st: ModelStorageStatus, ui: StorageCardUi, nameOf: NameOf): string {
  const count = st.moves.length + st.planned.length;
  if (!count) return "";
  const kindIcon = (k: string): string => `<span title="${k === "promote" ? "Promote" : "Demote"}" style="display:inline-flex">${icon(k === "promote" ? "arrowUp" : "arrowDown")}</span>`;
  const planned = st.planned.map((p) => `<div style="display:flex;gap:8px;align-items:center;flex-wrap:wrap;font-size:12px;min-width:0">
      ${kindIcon(p.kind)}<span style="min-width:0;flex:1 1 160px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap" title="${esc(p.file)}">${esc(nameOf(p.file, null))}</span>
      <span class="chip" title="${esc(p.reason)}">Planned</span></div>`).join("");
  const done = st.moves.map((r) => `<div style="display:flex;gap:8px;align-items:center;flex-wrap:wrap;font-size:12px;min-width:0">
      ${kindIcon(r.kind)}<span style="min-width:0;flex:1 1 160px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap" title="${esc(r.logical)}${r.reason ? ` · ${esc(r.reason)}` : ""}">${esc(nameOf(r.logical, null))}</span>
      ${outcomeChip(r)}
      <span style="color:var(--text2)">${esc(fmtBytes(r.bytes))}</span>
      <span style="color:var(--text3)">${esc(fmtDate(r.finished_at ?? r.started_at))} ${esc(fmtTime(r.finished_at ?? r.started_at))}</span>
    </div>`).join("");
  return `<div style="margin-top:12px">
    <button class="btn" data-storage-act="toggle-moves" aria-expanded="${ui.movesOpen}" title="Moves" aria-label="Moves" style="padding:2px 8px;font-size:11px">${icon(ui.movesOpen ? "chevD" : "chevR")} Moves: ${count}</button>
    ${ui.movesOpen ? `<div style="display:grid;gap:4px;margin-top:6px">${planned}${done}</div>` : ""}
  </div>`;
}

/** The Storage card: one stacked bar per tier (bytes by model file
 *  over the volume, other files gray, free space unfilled; the hot
 *  reserve and budget as markers), moves in flight, the unregistered
 *  files and the move history behind toggles. Nothing renders
 *  without a status (rule 13); a failed status call is a one-line
 *  error, never a fabricated bar. */
export function storageCardHtml(st: ModelStorageStatus | null, error: string | null, rows: readonly ModelRow[], ui: StorageCardUi): string {
  if (error) return `<div class="card" style="color:var(--text2);margin-bottom:12px">${icon("err")} Storage unavailable: ${esc(error)}</div>`;
  if (!st || !st.tiers.length) return "";
  const colors = new Map<string, string>();
  for (const f of [...st.files.map((x) => x.file), ...st.tiers.flatMap((t) => t.files.map((x) => x.file))]) {
    const k = f.toLowerCase();
    if (!colors.has(k)) colors.set(k, FILE_COLORS[colors.size % FILE_COLORS.length]);
  }
  const colorOf = (file: string): string => colors.get(file.toLowerCase()) ?? C.blue;
  const nameOf: NameOf = (file, modelId) => {
    const id = modelId ?? st.files.find((f) => f.file.toLowerCase() === file.toLowerCase())?.model_id ?? null;
    const row = id ? rows.find((m) => m.id === id) : undefined;
    return row ? modelShortName(row) : id ?? modelFileShortName(file);
  };
  const ctx = storageCtx(st);
  const note = ui.note ? `<div style="margin-top:8px;font-size:12px;color:${ui.note.err ? "var(--err)" : "var(--text2)"}">${esc(ui.note.text)}</div>` : "";
  return `<div class="card" id="card-model-storage" style="margin-bottom:12px">
    <h3>${icon("layers")} Storage<span class="info-tip" title="Model files live on the hot tier (fast drive) or a cold tier (slower drive). Promoting copies a cold file to hot and keeps the cold copy as an archive; demoting deletes the hot copy when its archive is intact, and copies it to cold first otherwise. Each bar is the tier's volume: one segment per model file, other files gray, free space unfilled.">${icon("info")}</span>
      ${st.config.auto ? "" : ` <span class="chip" title="Policy-driven moves are off; Promote and Demote still work.">Auto moves: off</span>`}</h3>
    ${lostTiersHtml(st)}
    ${st.tiers.map((t) => tierHtml(t, st, colorOf, nameOf)).join("")}
    ${movesInFlightHtml(st.files, nameOf)}
    ${unregisteredHtml(st.files, ui, ctx, nameOf)}
    ${movesHistoryHtml(st, ui, nameOf)}
    ${note}
  </div>`;
}
