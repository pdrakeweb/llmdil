/* Models view (split from static.ts). */

import { cancelDownload, deleteModel, downloadModel, getBackends, getModels, isTauri, modelAdd, modelHfHeader, modelHfLookup, modelHfSuggest, modelSetEnabled, modelSetMtp, modelSetOffloadSpec, modelSetShortName, pauseModelDownload, resumeModelDownload, verifyModel } from "../api";
import type { HfHeaderResult, HfLookupQuant, HfLookupRepo, ModelRow } from "../api";
import { fmtCount, meter, stackedBar, trim1 } from "../charts";
import { confirmDialog } from "../dialog";
import { editingIn, esc, keepFocus } from "../html";
import { icon } from "../icons";
import { enableSwitch } from "../switch";
import { C, SELECT_STYLE, bannerHtml, emptyHtml, fmtBytes, fmtDate, loadingHtml, modelShortName, onOvGoto, ovStatusBtn, sameModelFile, setBanner, topbar } from "./common";

/* ---------------- Models ---------------- */

/** A hash counts as pinned exactly when the backend accepts it as
 *  one: 64 characters, all hex (the placeholder text the catalog ships
 *  fails both halves of the test). */
function hashPinned(m: ModelRow): boolean {
  return typeof m.sha256 === "string" && m.sha256.length === 64 && /^[0-9a-fA-F]+$/.test(m.sha256);
}

function modelShaBadge(m: ModelRow): string {
  // "Verified" asserts a file on disk matched the pin — a verified_at
  // stamp whose file is gone must not keep claiming it.
  if (m.verified_at && m.on_disk) {
    return `<span class="chip badge-ok">${icon("shield")} sha256 verified ${esc(fmtDate(m.verified_at))}</span>`;
  }
  if (!hashPinned(m)) {
    // A static configuration state, not a transition — gray, with
    // the warn icon and the explanation in the tooltip (principle 11).
    return `<span class="chip" title="No SHA-256 is pinned for this model — its downloads run unverified until a hash is recorded in the configuration.">${icon("warn")} hash not pinned</span>`;
  }
  return `<span class="chip">${icon("shield")} pinned, not verified</span>`;
}

type ModelNote = { text: string; err: boolean };

/** Overview list for the Models view: one line per model with column
 *  headings, divider lines, and a status button that scrolls to the
 *  model's card. The status reflects the file/download lifecycle:
 *  verified > on disk unverified > partial > downloading/verifying >
 *  disabled > not downloaded; a recorded download failure outranks
 *  all but an in-flight retry. */
function modelOverviewHtml(rows: ModelRow[], stats: Map<string, { backends: number; req?: number }>): string {
  if (!rows.length) return "";
  const body = rows.map((m) => {
    const st = stats.get(m.id);
    let btn: string;
    let stateTxt: string;
    if (m.download_error && !m.downloading) { btn = ovStatusBtn("err", "err", `card-model-${m.id}`, `${m.id}: download failed — go to card`); stateTxt = "download failed"; }
    else if (m.downloading) { btn = ovStatusBtn("warn", m.paused ? "pause" : "clock", `card-model-${m.id}`, `${m.id}: ${m.paused ? "download paused" : "downloading"} — go to card`); stateTxt = m.paused ? "download paused" : "downloading"; }
    else if (m.verifying) { btn = ovStatusBtn("warn", "shield", `card-model-${m.id}`, `${m.id}: verifying checksum — go to card`); stateTxt = "verifying checksum"; }
    else if (!m.enabled) { btn = ovStatusBtn("idle", "pause", `card-model-${m.id}`, `${m.id}: disabled — go to card`); stateTxt = "disabled"; }
    else if (m.on_disk && m.verified_at) { btn = ovStatusBtn("ok", "check", `card-model-${m.id}`, `${m.id}: downloaded and verified — go to card`); stateTxt = "verified"; }
    else if (m.on_disk) { btn = ovStatusBtn("warn", "shield", `card-model-${m.id}`, `${m.id}: on disk, checksum not verified — go to card`); stateTxt = "on disk, not verified"; }
    else if ((m.partial_bytes ?? 0) > 0) { btn = ovStatusBtn("warn", "warn", `card-model-${m.id}`, `${m.id}: partial download — go to card`); stateTxt = "partial download"; }
    else { btn = ovStatusBtn("idle", "download", `card-model-${m.id}`, `${m.id}: not downloaded — go to card`); stateTxt = "not downloaded"; }
    const size = m.on_disk ? fmtBytes(m.size_bytes) : (m.partial_bytes ? `${fmtBytes(m.partial_bytes)} partial` : "\u2014");
    return `<div class="ov-row">${btn}<span class="ov-name" title="${esc(m.id)}">${esc(modelShortName(m))}</span><span class="ov-dim">${esc(m.quant)}</span><span class="ov-dim">${esc(size)}</span><span class="ov-dim">${esc(stateTxt)}</span><span class="ov-dim">${st ? fmtCount(st.backends) : "0"}</span><span class="ov-dim">${st?.req !== undefined ? fmtCount(st.req) : "\u2014"}</span></div>`;
  }).join("");
  return `<div class="ovlist ov-m"><div class="ov-head"><span></span><span>Model</span><span>Quant</span><span>Size</span><span>State</span><span>Serving backends</span><span>Requests (1h)</span></div>${body}</div>`;
}

/** Recover (repo, file) from an HF resolve URL — the inverse
 *  of the source URL an Add Model pick constructs. The Edit
 *  form's re-fetch (P2-D8) is offered only when this succeeds;
 *  mirrors manager_app::hf_forms::parse_hf_source_url. */
function parseHfSourceUrl(url: string): { repo: string; file: string } | null {
  const schemeEnd = url.indexOf("://");
  if (schemeEnd < 0) return null;
  const rest = url.slice(schemeEnd + 3);
  if (!rest.startsWith("huggingface.co/")) return null;
  const path = rest.slice("huggingface.co/".length);
  const idx = path.indexOf("/resolve/");
  if (idx < 0) return null;
  const repo = path.slice(0, idx);
  const tail = path.slice(idx + "/resolve/".length);
  const slash = tail.indexOf("/");
  if (!repo.includes("/") || slash < 0) return null;
  const file = tail.slice(slash + 1);
  return file ? { repo, file } : null;
}

/** "42% · 35 MiB/s" for a download with bytes on the row; parts that
 *  are not known are left out. */
export function downloadProgressText(partial: number, total: number | null, bytesPerSec: number | null): string {
  return [
    total !== null && total > 0 ? `${Math.min(100, Math.floor((partial / total) * 100))}%` : "",
    bytesPerSec !== null && bytesPerSec > 0 ? `${fmtBytes(bytesPerSec)}/s` : "",
  ].filter(Boolean).join(" \u00b7 ");
}

function modelCard(m: ModelRow, downloading: Set<string>, notes: Map<string, ModelNote>, stats?: { backends: number; req?: number; err?: number; toks?: number }, rename?: { draft: string } | null, detailsOpen = false, offload?: { layers: string; def: string; moe: boolean; mtp: boolean } | null, hfRefetch?: HfHeaderResult | null, rate: number | null = null): string {
  const note = notes.get(m.id);
  const noteHtml = note
    ? `<div style="margin-top:8px;font-size:12px;color:${note.err ? "var(--err)" : "var(--text2)"}">${esc(note.text)}</div>` : "";
  // Backend-recorded terminal failure (transport error, or a hash
  // mismatch — which also removed the file). Shown until a new download
  // is started.
  const dlErrHtml = !m.downloading && m.download_error
    ? `<div style="margin-top:8px;font-size:12px;color:var(--err)">${esc(m.download_error)}</div>` : "";
  const partial = m.partial_bytes ?? 0;
  const inFlight = downloading.has(m.id) || m.downloading;
  // Spawn window: this view started the download but the spawn invoke
  // has not settled, so the backend does not track it yet — Pause and
  // Cancel would have nothing to act on and stay inert until it lands.
  const spawning = downloading.has(m.id) && !m.downloading;
  const spawnDis = spawning ? " disabled" : "";
  /* Lifecycle state as ONE chip, colored per principle 11:
   * downloading / paused / verifying are transitional (amber);
   * partial and a recorded download failure are light red. The
   * state sentences move into the chips' tooltips. */
  const stateChip = inFlight
    ? `<span class="chip badge-warn">${m.paused ? "Paused" : "Downloading"}</span>`
    : m.verifying
      ? `<span class="chip badge-warn" title="The download finished and the file is being checked against the pinned SHA-256.">Verifying checksum</span>`
      : partial > 0
        ? `<span class="chip badge-err" title="The file is incomplete and cannot be used yet.">Partial download</span>`
        : m.download_error
          ? `<span class="chip badge-err">Download failed</span>`
          : "";
  /* Download progress: one meter, fill = partial / total (on-disk
   * size when known, else the catalog estimate). The byte counts
   * appear once, as the value beside the bar — the Size tile no
   * longer repeats them. */
  const dlTotal = m.total_bytes ?? (m.on_disk ? m.size_bytes : m.catalog_size_bytes);
  const exactTotal = m.total_bytes != null || m.on_disk;
  const speed = downloadProgressText(partial, dlTotal, inFlight && !m.paused ? m.bytes_per_sec ?? rate : null);
  const progressHtml = (inFlight || partial > 0) && partial > 0 && (dlTotal === null || dlTotal <= 0)
    ? `<div style="font-size:12px;color:var(--text2);margin-top:8px">Downloaded: ${esc(fmtBytes(partial))}${speed ? ` \u00b7 ${esc(speed)}` : ""}</div>`
    : (inFlight || partial > 0) && dlTotal !== null && dlTotal > 0 && partial > 0
    ? `<div style="display:flex;gap:10px;align-items:center;margin-top:8px">
        <span style="flex:1">${meter((partial / dlTotal) * 100, 300, 8, C.blue)}</span>
        <span style="font-size:12px;color:var(--text2)">Downloaded: ${esc(fmtBytes(partial))} of ${exactTotal ? "" : "~"}${esc(fmtBytes(dlTotal))}${exactTotal ? "" : " (estimate)"}${speed ? ` \u00b7 ${esc(speed)}` : ""}</span>
      </div>`
    : "";
  let actions: string;
  if (inFlight && m.downloading && m.paused) {
    actions = `<button class="btn icon-only" data-model-act="resume" data-model-id="${esc(m.id)}" title="Resume download" aria-label="Resume download of ${esc(m.id)}">${icon("play")}</button>
      <button class="btn" data-model-act="cancel" data-model-id="${esc(m.id)}">${icon("x")} Cancel</button>`;
  } else if (inFlight) {
    actions = `<button class="btn icon-only" data-model-act="pause" data-model-id="${esc(m.id)}" title="Pause download" aria-label="Pause download of ${esc(m.id)}"${spawnDis}>${icon("pause")}</button>
      <button class="btn" data-model-act="cancel" data-model-id="${esc(m.id)}"${spawnDis}>${icon("x")} Cancel</button>`;
  } else if (m.verifying) {
    // The transfer finished; the backend watcher is hashing the file
    // against the pinned SHA-256. Not a plain complete state yet —
    // the state chip above carries it; no actions apply.
    actions = "";
  } else if (partial > 0) {
    actions = `<button class="btn icon-only" data-model-act="download" data-model-id="${esc(m.id)}" title="Resume download" aria-label="Resume download of ${esc(m.id)}">${icon("play")}</button>
      <button class="btn" data-model-act="delete" data-model-id="${esc(m.id)}">${icon("err")} Delete</button>`;
  } else if (!m.on_disk) {
    actions = `<button class="btn" data-model-act="download" data-model-id="${esc(m.id)}">${icon("download")} Download</button>`;
  } else {
    // Verify is only offered with a pinned hash — against an unpinned
    // placeholder it could never succeed (the badge explains the state).
    actions = `${hashPinned(m) ? `<button class="btn icon-only" data-model-act="verify" data-model-id="${esc(m.id)}" title="Verify checksum" aria-label="Verify checksum of ${esc(m.id)}">${icon("shield")}</button>` : ""}
      <button class="btn" data-model-act="delete" data-model-id="${esc(m.id)}">${icon("err")} Delete</button>`;
  }
  // Enable/disable is always offered: it controls whether the setup
  // wizard's download step includes this model. The control is the
  // shared state switch (rule 16).
  actions += ` ${enableSwitch({ on: m.enabled, name: `Model ${m.id}`, attrs: `data-model-act="toggle-enabled" data-model-id="${esc(m.id)}"` })}`;
  // Rename is always offered: the short name is the operator's own
  // label for the model, independent of its download state.
  actions += ` <button class="btn icon-only" data-model-act="rename" data-model-id="${esc(m.id)}" title="Rename short name" aria-label="Rename short name of ${esc(m.id)}">${icon("pencil")}</button>`;
  // The capability declaration editor is always offered too: it
  // is how a model outside the curated catalog acquires
  // expert-offload and MTP draft controls for its backends.
  actions += ` <button class="btn icon-only" data-model-act="offload" data-model-id="${esc(m.id)}" title="Capability declarations" aria-label="Capability declarations of ${esc(m.id)}">${icon("layers")}</button>`;
  const renameHtml = rename ? `<div class="backend-form-panel flags-panel">
      <div style="font-size:12px;font-weight:600">Rename short name</div>
      <div style="display:flex;gap:6px;margin-top:6px;min-width:0">
        <input data-model-rename-input="${esc(m.id)}" type="text" value="${esc(rename.draft)}" maxlength="64" style="${SELECT_STYLE};flex:1;min-width:0">
      </div>
      <div class="card-actions"><button class="btn" data-model-act="rename-save" data-model-id="${esc(m.id)}">${icon("check")} Save name</button>
        <button class="btn icon-only" data-model-act="rename-cancel" data-model-id="${esc(m.id)}" title="Cancel" aria-label="Cancel">${icon("x")}</button></div>
    </div>` : "";
  /* The effective offload figures: a catalog pin wins over the
   * record's declaration wherever the catalog carries one. */
  const effMoeLayers = m.catalog_moe_layers ?? m.moe_layers ?? null;
  const effMoeDefault = m.catalog_default_offload_layers ?? m.default_offload_layers ?? null;
  const catalogPinned = m.catalog_moe_layers != null && m.catalog_default_offload_layers != null;
  /* The effective MTP capability: the same precedence — the
   * catalog's pin (where it knows the model) over the record's
   * declaration. */
  const effMtp = m.catalog_mtp ?? m.mtp ?? false;
  const mtpPinned = m.catalog_mtp != null;
  /* Re-fetch proposals (P2-D8), rendered from the last
   * Refresh result held by wireModels: per-field, applied
   * explicitly, and never over a catalog pin. The changed
   * hash is a warning line only — a re-fetch never re-pins. */
  const hfSource = parseHfSourceUrl(m.source_url);
  const hfRefetchHtml = ((): string => {
    if (!offload || !hfRefetch) return "";
    const parts: string[] = [];
    if (hfRefetch.sha256 && hashPinned(m) && hfRefetch.sha256 !== m.sha256) {
      parts.push(`<div style="font-size:12px;color:var(--err);margin-top:6px" title="Stored: ${esc(m.sha256)} · Fetched: ${esc(hfRefetch.sha256)}">SHA-256 changed in the repository — Stored: ${esc(m.sha256.slice(0, 16))}… · Fetched: ${esc(hfRefetch.sha256.slice(0, 16))}…</div>`);
    }
    const hdr = hfRefetch.header;
    if (!hdr) {
      parts.push(`<div style="font-size:12px;color:var(--text2);margin-top:6px">File header could not be read.</div>`);
    } else if (!hfRefetch.catalog) {
      if (hdr.moe_layers != null && !catalogPinned && hdr.moe_layers !== (m.moe_layers ?? null)) {
        parts.push(`<div style="font-size:12px;color:var(--text2);margin-top:6px">MoE layers — Declared: ${m.moe_layers ?? "—"} · File: ${hdr.moe_layers} <button class="btn" data-model-act="offload-use-file" data-model-id="${esc(m.id)}" data-file-layers="${hdr.moe_layers}" style="padding:2px 8px;font-size:11px">Use file value</button></div>`);
      }
      if (hdr.mtp && !mtpPinned && !effMtp) {
        parts.push(`<div style="font-size:12px;color:var(--text2);margin-top:6px">MTP — File: head present <button class="btn" data-model-act="mtp-use-file" data-model-id="${esc(m.id)}" style="padding:2px 8px;font-size:11px">Declare MTP</button></div>`);
      }
    }
    return parts.join("");
  })();
  const offloadHtml = offload ? `<div class="backend-form-panel flags-panel">
      <div style="font-size:12px;font-weight:600">Capability declarations</div>
      <div style="margin-top:6px"><label style="font-size:12px;color:var(--text2);display:flex;gap:6px;align-items:center">
        <input data-model-mtp-check="${esc(m.id)}" type="checkbox" ${offload.mtp ? "checked" : ""}${mtpPinned ? ` disabled title="Pinned by the curated catalog"` : ""}> MTP capable</label></div>
      ${catalogPinned
        ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">This model's offload figures come from the curated catalog — ${effMoeDefault} of ${effMoeLayers} MoE layers by default — and cannot be changed here.</div>`
        : `<div style="margin-top:8px"><label style="font-size:12px;color:var(--text2);display:flex;gap:6px;align-items:center">
            <input data-model-moe-check="${esc(m.id)}" type="checkbox" ${offload.moe ? "checked" : ""}> MoE</label></div>
          <div style="font-size:12px;color:var(--text2);margin-top:6px">Declare this model as mixture-of-experts (MoE) to give its backends expert-offload controls: a new backend starts at the default offload, and the Edit Flags slider ranges over the MoE layers. Uncheck MoE and save to remove the declaration.</div>
          <div style="display:flex;gap:10px;margin-top:6px;flex-wrap:wrap">
            <label style="font-size:12px;color:var(--text2)">MoE layers<br>
              <input data-model-offload-layers="${esc(m.id)}" type="text" inputmode="numeric" value="${esc(offload.layers)}" placeholder="48" style="${SELECT_STYLE};width:110px"${offload.moe ? "" : " disabled"}></label>
            <label style="font-size:12px;color:var(--text2)">Default offload layers<br>
              <input data-model-offload-default="${esc(m.id)}" type="text" inputmode="numeric" value="${esc(offload.def)}" placeholder="16" style="${SELECT_STYLE};width:110px"${offload.moe ? "" : " disabled"}></label>
          </div>`}
      ${m.moe_reconcile ? `<div style="font-size:12px;color:var(--text2);margin-top:8px">MoE layers — Declared: ${m.moe_reconcile.declared} · File: ${m.moe_reconcile.file} <button class="btn" data-model-act="offload-use-file" data-model-id="${esc(m.id)}" data-file-layers="${m.moe_reconcile.file}" style="padding:2px 8px;font-size:11px">Use file value</button></div>` : ""}
      ${m.file_nextn && !mtpPinned && !effMtp ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">MTP — File: head present <button class="btn" data-model-act="mtp-use-file" data-model-id="${esc(m.id)}" style="padding:2px 8px;font-size:11px">Declare MTP</button></div>` : ""}
      ${hfSource && !(catalogPinned && mtpPinned) ? `<div style="margin-top:8px"><button class="btn" data-model-act="hf-refresh" data-model-id="${esc(m.id)}">${icon("search")} Refresh from Hugging Face</button></div>` : ""}
      ${hfRefetchHtml}
      ${catalogPinned && mtpPinned
        ? `<div class="card-actions"><button class="btn icon-only" data-model-act="offload-cancel" data-model-id="${esc(m.id)}" title="Close" aria-label="Close">${icon("x")}</button></div>`
        : `<div class="card-actions"><button class="btn" data-model-act="offload-save" data-model-id="${esc(m.id)}">${icon("check")} Save declaration</button>
          <button class="btn icon-only" data-model-act="offload-cancel" data-model-id="${esc(m.id)}" title="Cancel" aria-label="Cancel">${icon("x")}</button></div>`}
    </div>` : "";
  // Size carries the final size only when the file is on disk; in
  // download states the progress value carries the partial figure,
  // and before any download the catalog estimate is the size.
  const sizeText = m.on_disk
    ? esc(fmtBytes(m.size_bytes))
    : m.catalog_size_bytes !== null ? `~${esc(fmtBytes(m.catalog_size_bytes))} (estimate)` : "not downloaded";
  /* Stats row: only when a metrics window exists (req defined).
   * Each field is its own tile; a missing field omits its tile —
   * never a coerced 0 (principle 12). The error-share bar encodes
   * the proportion; the counts live in the tiles. No window: no
   * stats row and no placeholder sentence. */
  const statsHtml = stats && stats.backends > 0 && stats.req !== undefined
    ? `<div style="display:grid;grid-template-columns:repeat(3,1fr);gap:12px;margin:0 0 8px;align-items:center">
        <div class="kv"><span class="k">Requests (1h)</span><span class="v" style="font-size:12px">${fmtCount(stats.req)}</span></div>
        ${stats.err !== undefined ? `<div class="kv"><span class="k">Errors (1h)</span><span class="v" style="font-size:12px">${fmtCount(stats.err)}</span></div>` : ""}
        ${stats.toks !== undefined ? `<div class="kv"><span class="k">Throughput</span><span class="v" style="font-size:12px">${trim1(stats.toks)} tok/s</span></div>` : ""}
      </div>
      ${stats.err !== undefined && stats.req > 0 ? stackedBar([
        { pct: (stats.err / stats.req) * 100, color: C.red, label: `Errors: ${fmtCount(stats.err)} of ${fmtCount(stats.req)}` },
        { pct: ((stats.req - stats.err) / stats.req) * 100, color: "#3a4350", label: `Succeeded: ${fmtCount(stats.req - stats.err)} of ${fmtCount(stats.req)}` },
      ], 600, 6) : ""}`
    : "";
  /* Details expandable: the full hash (with copy), verified date,
   * full path, and model id. The 16-char fragment line is gone —
   * the hash is printed once, here, in full. No hash: no SHA-256
   * row. */
  const detailsHtml = `<div style="margin-top:8px">
      <button class="btn" data-model-act="details" data-model-id="${esc(m.id)}" aria-expanded="${detailsOpen}" title="Details" aria-label="Details" style="padding:2px 8px;font-size:11px">${icon(detailsOpen ? "chevD" : "chevR")}</button>
      ${detailsOpen ? `<div style="display:grid;gap:4px;margin-top:8px;font-size:12px">
        ${m.sha256 ? `<div style="display:flex;gap:8px;align-items:center;min-width:0"><span style="color:var(--text2)">SHA-256:</span><span style="font-family:var(--mono);font-size:11px;word-break:break-all">${esc(m.sha256)}</span><button class="btn" data-model-act="copy-hash" data-model-id="${esc(m.id)}" title="Copy SHA-256" aria-label="Copy SHA-256" style="padding:1px 7px;font-size:11px">${icon("copy")}</button></div>` : ""}
        ${m.verified_at ? `<div><span style="color:var(--text2)">Verified:</span> ${esc(fmtDate(m.verified_at))}</div>` : ""}
        <div style="min-width:0"><span style="color:var(--text2)">File:</span> <span style="font-family:var(--mono);font-size:11px;word-break:break-all">${esc(m.gguf_path)}</span></div>
        <div><span style="color:var(--text2)">Model ID:</span> <span style="font-family:var(--mono)">${esc(m.id)}</span></div>
      </div>` : ""}
    </div>`;
  return `<div class="card" id="card-model-${esc(m.id)}">
    <h3>${icon("box")} ${esc(modelShortName(m))} <span class="chip">${esc(m.quant)}</span>${m.enabled ? "" : ` <span class="chip">disabled</span>`}
      <span class="spacer" style="flex:1"></span>
      ${modelShaBadge(m)}${hashPinned(m) ? ` <button class="btn" data-model-act="copy-hash" data-model-id="${esc(m.id)}" title="Copy the full SHA-256" aria-label="Copy SHA-256" style="padding:2px 8px;font-size:11px">${icon("copy")}</button>` : ""}</h3>
    ${modelShortName(m) !== m.id ? `<div style="font-size:11px;color:var(--text3);font-family:var(--mono);margin-top:2px">${esc(m.id)}</div>` : ""}
    ${stateChip || progressHtml ? `<div style="margin-top:8px">${stateChip}${progressHtml}</div>` : ""}
    <div style="display:grid;grid-template-columns:1fr 1fr 1fr;gap:12px;margin:12px 0;align-items:center">
      <div class="kv"><span class="k">File</span><span class="v" style="font-size:12px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap" title="${esc(m.gguf_path)}">${esc(m.gguf_path)}</span> <button class="btn" data-model-act="copy-path" data-model-id="${esc(m.id)}" title="Copy file path" aria-label="Copy file path" style="padding:1px 7px;font-size:11px">${icon("copy")}</button></div>
      <div class="kv"><span class="k">Size</span><span class="v" style="font-size:12px">${sizeText}</span></div>
      ${m.params_b > 0 ? `<div class="kv"><span class="k">Parameters</span><span class="v" style="font-size:12px">${trim1(m.params_b)}B</span></div>` : ""}
      ${m.ctx_max != null ? `<div class="kv"><span class="k">Context</span><span class="v" style="font-size:12px">${fmtCount(m.ctx_max)}</span></div>` : ""}
      ${effMoeLayers != null && effMoeDefault != null ? `<div class="kv"><span class="k">Expert offload</span><span class="v" style="font-size:12px">Default: ${effMoeDefault} of ${effMoeLayers} layers</span></div>` : ""}
      ${effMtp ? `<div class="kv"><span class="k">MTP</span><span class="v" style="font-size:12px">Capable</span></div>` : ""}
    </div>
    ${statsHtml}
    <div class="card-actions">${actions}</div>
    ${detailsHtml}
    ${renameHtml}
    ${offloadHtml}
    ${dlErrHtml}
    ${noteHtml}
  </div>`;
}

/** The Add Model form card, painted into #modelAddForm by
 *  wireModels when the topbar's Add model button opens it (the
 *  Add Backend form's host pattern). Closed = the host is empty,
 *  so a successful submit clears and hides the form by emptying
 *  the host; the page's pre-open state is the topbar button. */
function modelAddFormHtml(): string {
  return `<div class="card"><h3>${icon("box")} Add a model<span class="info-tip" title="The model is added enabled. A model picked from Hugging Face starts pinned to the picked file's SHA-256; a manually entered model has no pinned SHA-256 — its downloads run unverified until a hash is recorded in the configuration.">${icon("info")}</span></h3>
    <div style="display:grid;grid-template-columns:1fr auto;gap:10px;margin-top:10px;align-items:end">
      <label style="font-size:12px;color:var(--text2)">Model name<br>
        <span style="position:relative;display:block">
          <input id="addModelName" type="text" placeholder="qwen3 coder" autocomplete="off" style="${SELECT_STYLE};width:100%">
          <span id="addModelSuggest" style="display:none;position:absolute;left:0;right:0;top:100%;z-index:30;max-height:264px;overflow-y:auto;background:var(--panel);border:1px solid var(--border);border-radius:6px"></span>
        </span></label>
      <button class="btn" id="addModelLookupBtn">${icon("search")} Look up</button>
    </div>
    <div id="addModelPick"></div>
    <div style="display:grid;grid-template-columns:1fr 2fr;gap:10px;margin-top:10px">
      <label style="font-size:12px;color:var(--text2)">Model id <span id="addModelIdSrc"></span><br>
        <input id="addModelId" type="text" placeholder="my-model-7b" style="${SELECT_STYLE};width:100%"></label>
      <label style="font-size:12px;color:var(--text2)">Source URL <span id="addModelUrlSrc"></span><br>
        <input id="addModelUrl" type="text" placeholder="Model download URL (.gguf)" style="${SELECT_STYLE};width:100%"></label>
    </div>
    <div style="margin-top:10px"><button class="btn" id="addModelOptToggle" aria-expanded="false" title="Optional fields" aria-label="Optional fields" style="padding:2px 8px;font-size:11px">${icon("chevR")}</button></div>
    <div id="addModelOptional" style="display:none">
      <div style="display:grid;grid-template-columns:1fr 1fr 1fr;gap:10px;margin-top:10px">
        <label style="font-size:12px;color:var(--text2)">Quant <span id="addModelQuantSrc"></span><br>
          <input id="addModelQuant" type="text" placeholder="Q4_K_M" style="${SELECT_STYLE};width:100%"></label>
        <label style="font-size:12px;color:var(--text2)">File name <span id="addModelFileSrc"></span><span class="info-tip" title="Defaults to the URL's file name.">${icon("info")}</span><br>
          <input id="addModelFile" type="text" placeholder="model.gguf" style="${SELECT_STYLE};width:100%"></label>
        <label style="font-size:12px;color:var(--text2)">Parameters (B) <span id="addModelParamsSrc"></span><span class="info-tip" title="Total parameter count in billions (e.g. 80 for an 80B model — for a mixture-of-experts model, the total, not the active count). Required for models outside the curated catalog: the count feeds memory-fit estimation and is never re-derived later.">${icon("info")}</span><br>
          <input id="addModelParams" type="text" inputmode="decimal" placeholder="80" style="${SELECT_STYLE};width:100%"></label>
      </div>
      <div style="display:flex;gap:16px;margin-top:10px">
        <label style="font-size:12px;color:var(--text2);display:flex;gap:6px;align-items:center">
          <input id="addModelMtp" type="checkbox"> MTP capable <span id="addModelMtpSrc"></span><span class="info-tip" title="The model's GGUF carries an MTP (multi-token prediction) head. Declaring it gives the model's backends the MTP draft control.">${icon("info")}</span></label>
        <label style="font-size:12px;color:var(--text2);display:flex;gap:6px;align-items:center">
          <input id="addModelMoe" type="checkbox"> MoE <span id="addModelMoeSrc"></span></label>
      </div>
      <div style="display:grid;grid-template-columns:1fr 1fr 1fr;gap:10px;margin-top:10px">
        <label style="font-size:12px;color:var(--text2)">MoE layers <span id="addModelMoeLayersSrc"></span><span class="info-tip" title="For a mixture-of-experts (MoE) model: the number of layers carrying routed experts. Declaring this and the default offload gives the model's backends expert-offload controls — a new backend starts at the default offload and the Edit Flags slider ranges over these layers.">${icon("info")}</span><br>
          <input id="addModelMoeLayers" type="text" inputmode="numeric" placeholder="48" style="${SELECT_STYLE};width:100%" disabled></label>
        <label style="font-size:12px;color:var(--text2)">Default offload layers <span id="addModelMoeDefaultSrc"></span><span class="info-tip" title="How many MoE layers a new backend for this model offloads to system RAM by default (0 to the MoE layer count; the full count is full expert offload).">${icon("info")}</span><br>
          <input id="addModelMoeDefault" type="text" inputmode="numeric" placeholder="16" style="${SELECT_STYLE};width:100%" disabled></label>
      </div>
    </div>
    <div class="card-actions"><button class="btn" id="addModelBtn">${icon("check")} Create model</button>
      <button class="btn icon-only" id="addModelCancel" title="Cancel" aria-label="Cancel">${icon("x")}</button></div>
    <div id="addModelMsg" style="font-size:12px;margin-top:6px;display:none"></div>
  </div>`;
}

export function renderModels(): string {
  return `${topbar("Models", `<button class="btn" id="modelAdd" title="Add model">${icon("plus")} Add model</button>`)}
  <div class="view"><div id="modelBanner"></div>
  <div id="modelAddForm"></div>
  <div id="modelOverview"></div>
  <div id="modelBody">${loadingHtml("Loading models…")}</div></div>`;
}

export async function wireModels(root: HTMLElement): Promise<() => void> {
  /** Downloads started from this view whose spawn invoke has not settled
   *  yet; the backend's `downloading` flag carries the state after that. */
  const downloading = new Set<string>();
  const notes = new Map<string, ModelNote>();
  let lastRows: ModelRow[] = [];
  /** Rename form state: the model whose inline rename form is open,
   *  and its draft text. Kept outside the repainted DOM (synced on
   *  input) so poll repaints never lose typed text. */
  let renameFor: string | null = null;
  let renameDraft = "";
  /** Capability-declaration form state: the model whose inline
   *  declaration form is open, and its draft fields (the MoE pair
   *  kept as text, like the rename draft, so poll repaints never
   *  lose typed text; the two capability checkboxes as booleans). */
  let offloadFor: string | null = null;
  let offloadDraftLayers = "";
  let offloadDraftDefault = "";
  let offloadDraftMoe = false;
  let offloadDraftMtp = false;
  /** Last Hugging Face re-fetch result per model (P2-D8):
   *  the declaration editor renders its proposals from this
   *  until the view is left. */
  const hfRefetchResults = new Map<string, HfHeaderResult>();
  const syncOffloadDraft = (ev: Event): void => {
    const t = ev.target as HTMLElement;
    if (t.hasAttribute("data-model-offload-layers") && t.getAttribute("data-model-offload-layers") === offloadFor) {
      offloadDraftLayers = (t as HTMLInputElement).value;
    }
    if (t.hasAttribute("data-model-offload-default") && t.getAttribute("data-model-offload-default") === offloadFor) {
      offloadDraftDefault = (t as HTMLInputElement).value;
    }
  };
  root.addEventListener("input", syncOffloadDraft);
  /** Checkbox drafts sync on change; toggling MoE also enables or
   *  disables the pair's inputs in place (no repaint, so the
   *  toggle never loses focus). */
  const syncOffloadChecks = (ev: Event): void => {
    const t = ev.target as HTMLElement;
    if (t.hasAttribute("data-model-mtp-check") && t.getAttribute("data-model-mtp-check") === offloadFor) {
      offloadDraftMtp = (t as HTMLInputElement).checked;
    }
    if (t.hasAttribute("data-model-moe-check") && t.getAttribute("data-model-moe-check") === offloadFor) {
      offloadDraftMoe = (t as HTMLInputElement).checked;
      const card = t.closest(".card");
      card?.querySelectorAll("[data-model-offload-layers],[data-model-offload-default]").forEach((el) => {
        (el as HTMLInputElement).disabled = !offloadDraftMoe;
      });
    }
  };
  root.addEventListener("change", syncOffloadChecks);
  /** Models whose Details expandable is open (wire state outside
   *  the repainted DOM, so poll repaints preserve it). */
  const openModelDetails = new Set<string>();
  const syncRenameDraft = (ev: Event): void => {
    const t = ev.target as HTMLElement;
    if (!t.hasAttribute("data-model-rename-input")) return;
    if (t.getAttribute("data-model-rename-input") !== renameFor) return;
    renameDraft = (t as HTMLInputElement).value;
  };
  root.addEventListener("input", syncRenameDraft);
  /** Enter in the rename input saves, like a form submit. */
  const onRenameKey = (ev: KeyboardEvent): void => {
    const t = ev.target as HTMLElement;
    if (!t.hasAttribute("data-model-rename-input") || ev.key !== "Enter") return;
    ev.preventDefault();
    const btn = root.querySelector(`[data-model-act="rename-save"][data-model-id="${CSS.escape(renameFor ?? "")}"]`) as HTMLButtonElement | null;
    btn?.click();
  };
  root.addEventListener("keydown", onRenameKey);
  /** Per-model gateway statistics, aggregated over the backends whose
   *  model_file matches the model's gguf_path. */
  let modelStats = new Map<string, { backends: number; req?: number; err?: number; toks?: number }>();
  let pollTimer: number | null = null;
  let disposed = false;
  /** Download speed per model from successive polls' byte counts. */
  const dlSamples = new Map<string, { bytes: number; at: number; rate: number | null }>();
  const sampleDownloads = (rows: ModelRow[]): void => {
    for (const m of rows) {
      if (!m.downloading || m.paused) { dlSamples.delete(m.id); continue; }
      const bytes = m.partial_bytes ?? 0;
      const prev = dlSamples.get(m.id);
      const secs = prev ? (Date.now() - prev.at) / 1000 : 0;
      if (prev && secs < 1) continue;
      dlSamples.set(m.id, { bytes, at: Date.now(), rate: prev && bytes >= prev.bytes ? (bytes - prev.bytes) / secs : null });
    }
  };

  /** Poll every 2s while any download is in flight (or verifying) so
   *  progress and the terminal state (complete, verified, failed)
   *  reach the view; stop when nothing is in flight. */
  const syncPoll = (): void => {
    // A load that resolves after the view was left must not start a
    // poll the (already-run) cleanup can never clear (UI-8).
    if (disposed) return;
    const any = downloading.size > 0 || lastRows.some((m) => m.downloading || m.verifying);
    if (any && pollTimer === null) {
      pollTimer = window.setInterval(() => { void load(); }, 2000);
    } else if (!any && pollTimer !== null) {
      window.clearInterval(pollTimer);
      pollTimer = null;
    }
  };

  const load = async (): Promise<void> => {
    const r = await getModels();
    if (disposed) return;
    const br = await getBackends();
    // Re-check after the second await too: a navigation away and back
    // while list_backends was in flight left this disposed instance
    // painting into the new view and starting a poll interval nothing
    // cleared (code review v0.1.60 UI-8).
    if (disposed) return;
    if (!br.error) {
      const m = new Map<string, { backends: number; req?: number; err?: number; toks?: number }>();
      for (const model of r.data) {
        const serving = br.data.filter((b) => b.configured && sameModelFile(b.model_file, model.gguf_path));
        if (!serving.length) continue;
        const withWin = serving.filter((b) => b.requests_1h !== undefined);
        // Throughput sums only backends whose live rate is
        // measured; absent when none is (unknown is not zero).
        const withTput = serving.filter((b) => b.throughput_toks !== undefined);
        m.set(model.id, withWin.length
          ? {
              backends: serving.length,
              req: withWin.reduce((a, b) => a + (b.requests_1h ?? 0), 0),
              err: withWin.reduce((a, b) => a + (b.errors_1h ?? 0), 0),
              toks: withTput.length
                ? withTput.reduce((a, b) => a + (b.throughput_toks ?? 0), 0)
                : undefined,
            }
          : { backends: serving.length });
      }
      modelStats = m;
    }
    setBanner(root, "modelBanner", bannerHtml(r));
    const b = root.querySelector("#modelBody");
    if (!b) return;
    const mov = root.querySelector("#modelOverview");
    if (r.error) { if (mov) mov.innerHTML = ""; b.innerHTML = emptyHtml(`Models unavailable: ${r.error}`); return; }
    if (!r.data.length) {
      lastRows = [];
      if (mov) mov.innerHTML = "";
      b.innerHTML = emptyHtml(r.live
        ? "No models are configured. Add one with the Add model button."
        : "No model data in sample mode.");
      syncPoll();
      return;
    }
    lastRows = r.data;
    sampleDownloads(lastRows);
    // While a card form's input has focus, a poll repaint would drop the
    // caret mid-word; the drafts are synced to wire state, so skipping
    // one repaint loses nothing.
    if (!editingIn(b)) keepFocus(root, () => {
      if (mov) mov.innerHTML = modelOverviewHtml(lastRows, modelStats);
      b.innerHTML = `<div class="cards-fluid">${lastRows.map((m) => modelCard(m, downloading, notes, modelStats.get(m.id), renameFor === m.id ? { draft: renameDraft } : null, openModelDetails.has(m.id), offloadFor === m.id ? { layers: offloadDraftLayers, def: offloadDraftDefault, moe: offloadDraftMoe, mtp: offloadDraftMtp } : null, hfRefetchResults.get(m.id) ?? null, dlSamples.get(m.id)?.rate ?? null)).join("")}</div>`;
    });
    syncPoll();
  };

  const onClick = async (ev: Event): Promise<void> => {
    const t = (ev.target as HTMLElement).closest("[data-model-act]") as HTMLElement | null;
    if (!t) return;
    const id = t.getAttribute("data-model-id") ?? "";
    const act = t.getAttribute("data-model-act") ?? "";

    if (act === "details") {
      if (openModelDetails.has(id)) openModelDetails.delete(id); else openModelDetails.add(id);
      await load();
      return;
    }

    if (act === "copy-hash" || act === "copy-path") {
      const row = lastRows.find((m) => m.id === id);
      const value = act === "copy-hash" ? row?.sha256 : row?.gguf_path;
      if (!value) return;
      try {
        await navigator.clipboard.writeText(value);
        notes.set(id, { text: act === "copy-hash" ? "SHA-256 copied to the clipboard." : "File path copied to the clipboard.", err: false });
      } catch {
        notes.set(id, { text: "Copy failed — copy the value manually.", err: true });
      }
      await load();
      return;
    }

    if (act === "download") {
      // Disable synchronously so a second click cannot fire while the
      // first spawn is in flight (the repaint also replaces the button).
      (t as HTMLButtonElement).disabled = true;
      downloading.add(id);
      // A fresh attempt clears any stale note (the backend clears the
      // recorded download_error when a new download starts).
      notes.delete(id);
      await load();
      const r = await downloadModel(id);
      downloading.delete(id);
      if (r.error) {
        notes.set(id, { text: `Download failed: ${r.error}`, err: true });
      }
      await load();
      return;
    }

    if (act === "pause") {
      (t as HTMLButtonElement).disabled = true;
      const r = await pauseModelDownload(id);
      if (r.error) notes.set(id, { text: `Pause failed: ${r.error}`, err: true });
      await load();
      return;
    }

    if (act === "resume") {
      (t as HTMLButtonElement).disabled = true;
      const r = await resumeModelDownload(id);
      if (r.error) notes.set(id, { text: `Resume failed: ${r.error}`, err: true });
      await load();
      return;
    }

    if (act === "cancel") {
      if (!await confirmDialog({ title: `Cancel the download of ${id}?`, body: "The downloaded progress will be discarded — the partial file is deleted.", confirmLabel: "Cancel download", cancelLabel: "Keep downloading", danger: true })) return;
      const r = await cancelDownload(id);
      if (r.error) {
        notes.set(id, { text: `Cancel failed: ${r.error}`, err: true });
      } else {
        downloading.delete(id);
        notes.set(id, { text: "Download cancelled.", err: false });
      }
      await load();
      return;
    }

    if (act === "toggle-enabled") {
      (t as HTMLButtonElement).disabled = true;
      const row = lastRows.find((m) => m.id === id);
      const target = !(row?.enabled ?? true);
      const r = await modelSetEnabled(id, target);
      if (r.error) {
        notes.set(id, { text: `${target ? "Enable" : "Disable"} failed: ${r.error}`, err: true });
      } else {
        notes.set(id, {
          text: target
            ? "Enabled — the setup wizard's download step will include this model."
            : "Disabled — the setup wizard's download step will skip this model. The file and configuration are kept.",
          err: false,
        });
      }
      await load();
      return;
    }

    if (act === "rename") {
      if (renameFor === id) { renameFor = null; await load(); return; }
      const row = lastRows.find((m) => m.id === id);
      renameFor = id;
      renameDraft = row ? modelShortName(row) : id;
      await load();
      const input = root.querySelector(`[data-model-rename-input="${CSS.escape(id)}"]`) as HTMLInputElement | null;
      input?.focus();
      input?.select();
      return;
    }

    if (act === "rename-cancel") {
      renameFor = null;
      await load();
      return;
    }

    if (act === "rename-save") {
      (t as HTMLButtonElement).disabled = true;
      const r = await modelSetShortName(id, renameDraft);
      if (r.error) {
        notes.set(id, { text: `Rename failed: ${r.error}`, err: true });
      } else {
        renameFor = null;
        notes.set(id, { text: `Short name saved: ${r.data.name}`, err: false });
      }
      await load();
      return;
    }

    if (act === "offload") {
      if (offloadFor === id) { offloadFor = null; await load(); return; }
      const row = lastRows.find((m) => m.id === id);
      offloadFor = id;
      offloadDraftLayers = row?.moe_layers != null ? String(row.moe_layers) : "";
      offloadDraftDefault = row?.default_offload_layers != null ? String(row.default_offload_layers) : "";
      // The checkboxes start from the EFFECTIVE capabilities
      // (catalog pin over the record's declaration).
      offloadDraftMoe = (row?.catalog_moe_layers ?? row?.moe_layers) != null
        && (row?.catalog_default_offload_layers ?? row?.default_offload_layers) != null;
      offloadDraftMtp = row?.catalog_mtp ?? row?.mtp ?? false;
      await load();
      return;
    }

    if (act === "offload-cancel") {
      offloadFor = null;
      await load();
      return;
    }

    if (act === "offload-use-file") {
      // "Use file value" (P2-D7): the file's parsed MoE layer
      // count fills the editor's layers field — a proposal in
      // the form, persisted by Save declaration through
      // model_set_offload_spec like any other edit. If the
      // stored default exceeds the corrected count, Save's
      // validation refuses and the operator adjusts the
      // default in this same editor; nothing clamps silently.
      const fileLayers = t.getAttribute("data-file-layers");
      if (fileLayers) {
        offloadDraftLayers = fileLayers;
        offloadDraftMoe = true;
      }
      await load();
      return;
    }

    if (act === "mtp-use-file") {
      (t as HTMLButtonElement).disabled = true;
      const r = await modelSetMtp(id, true);
      if (r.error) {
        notes.set(id, { text: `Declaration failed: ${r.error}`, err: true });
      } else {
        notes.set(id, { text: "MTP declared capable from the file's header: this model's backends gain the MTP draft control.", err: false });
      }
      await load();
      return;
    }

    if (act === "hf-refresh") {
      // Re-fetch (P2-D8): re-read this model's repo record
      // and file header from Hugging Face and hold the
      // result for the editor's proposal lines. Nothing is
      // written here — proposals apply per field.
      const row = lastRows.find((m) => m.id === id);
      const src = row ? parseHfSourceUrl(row.source_url) : null;
      if (!row || !src) return;
      (t as HTMLButtonElement).disabled = true;
      const r = await modelHfHeader(src.repo, src.file);
      if (r.error || !r.live) {
        notes.set(id, { text: `Refresh failed: ${r.error ?? "the re-fetch runs in the installed app."}`, err: true });
      } else {
        hfRefetchResults.set(id, r.data);
      }
      await load();
      return;
    }

    if (act === "offload-save") {
      (t as HTMLButtonElement).disabled = true;
      const row = lastRows.find((m) => m.id === id);
      const moePinned = row != null && row.catalog_moe_layers != null && row.catalog_default_offload_layers != null;
      const mtpPinned = row?.catalog_mtp != null;
      // MoE pair, validated before anything persists: checked
      // requires both fields; unchecked clears the declaration.
      let layers: number | null = null;
      let def: number | null = null;
      if (!moePinned && offloadDraftMoe) {
        const layersRaw = offloadDraftLayers.trim();
        const defRaw = offloadDraftDefault.trim();
        const l = Number(layersRaw);
        const d = Number(defRaw);
        if (!layersRaw || !defRaw || !Number.isInteger(l) || !Number.isInteger(d) || l < 1 || d < 0 || d > l) {
          notes.set(id, { text: "Declaration not saved: MoE is checked — give both fields, MoE layers of 1 or more and a default offload between 0 and the layer count (or uncheck MoE to remove the declaration).", err: true });
          await load();
          return;
        }
        layers = l;
        def = d;
      }
      const savedParts: string[] = [];
      if (!moePinned) {
        const r = await modelSetOffloadSpec(id, layers, def);
        if (r.error) {
          notes.set(id, { text: `Declaration failed: ${r.error}`, err: true });
          await load();
          return;
        }
        savedParts.push(layers === null
          ? "MoE offload declaration cleared."
          : `MoE offload declared: default ${def} of ${layers} layers. New backends start at the default, and this model's backends gain the Edit Flags offload slider.`);
      }
      if (!mtpPinned) {
        // Checked stores the declaration; unchecked clears it —
        // either way the effective capability follows the box.
        const r = await modelSetMtp(id, offloadDraftMtp ? true : null);
        if (r.error) {
          notes.set(id, { text: `Declaration failed: ${r.error}`, err: true });
          await load();
          return;
        }
        savedParts.push(offloadDraftMtp
          ? "MTP declared capable: this model's backends gain the MTP draft control."
          : "MTP declaration cleared.");
      }
      offloadFor = null;
      if (savedParts.length) notes.set(id, { text: savedParts.join(" "), err: false });
      await load();
      return;
    }

    if (act === "verify") {
      const btn = t as HTMLButtonElement;
      btn.disabled = true;
      const r = await verifyModel(id);
      if (r.error) {
        notes.set(id, { text: `Verify failed: ${r.error}`, err: true });
      } else {
        notes.set(id, r.data.ok
          ? { text: "Checksum verified — the file on disk matches the pinned SHA-256.", err: false }
          : { text: "Checksum mismatch — the file on disk does not match the pinned SHA-256.", err: true });
      }
      await load();
      return;
    }

    if (act === "delete") {
      if (!await confirmDialog({ title: `Delete model ${id}?`, body: "This removes the downloaded file and its config entry. The delete is refused while a backend references the model.", confirmLabel: "Delete", danger: true })) return;
      const r = await deleteModel(id);
      if (r.error) {
        notes.set(id, { text: `Delete failed: ${r.error}`, err: true });
      } else {
        notes.delete(id);
      }
      await load();
    }
  };
  root.addEventListener("click", onClick);
  root.addEventListener("click", onOvGoto);

  /* ---------------- Add model form ----------------
   * Lives in #modelAddForm, outside the repainted #modelBody, so
   * the poll never clobbers typed input (the Add Backend form's
   * host pattern). The topbar's Add model button opens it; a
   * successful create empties the host — the form clears and
   * hides, returning the page to its pre-open state — while a
   * failed create keeps the form open with the input intact. */
  const addModelHost = root.querySelector("#modelAddForm");
  const closeAddModelForm = (): void => { if (addModelHost) addModelHost.innerHTML = ""; };
  const openAddModelForm = (): void => {
    if (!addModelHost) return;
    if (addModelHost.innerHTML) { closeAddModelForm(); return; }
    addModelHost.innerHTML = modelAddFormHtml();
    addModelHost.scrollIntoView({ block: "start" });
    (addModelHost.querySelector("#addModelName") as HTMLElement | null)?.focus({ preventScroll: true });
    const optToggle = addModelHost.querySelector("#addModelOptToggle") as HTMLButtonElement | null;
    optToggle?.addEventListener("click", () => {
      const box = addModelHost.querySelector("#addModelOptional") as HTMLElement | null;
      if (!box) return;
      const open = box.style.display === "none";
      box.style.display = open ? "" : "none";
      optToggle.setAttribute("aria-expanded", String(open));
      optToggle.innerHTML = icon(open ? "chevD" : "chevR");
    });
    addModelHost.querySelector("#addModelCancel")?.addEventListener("click", closeAddModelForm);
    // The MoE checkbox gates the declaration pair's inputs.
    const addMoeCheck = addModelHost.querySelector("#addModelMoe") as HTMLInputElement | null;
    addMoeCheck?.addEventListener("change", () => {
      const on = addMoeCheck.checked;
      (addModelHost.querySelector("#addModelMoeLayers") as HTMLInputElement | null)?.toggleAttribute("disabled", !on);
      (addModelHost.querySelector("#addModelMoeDefault") as HTMLInputElement | null)?.toggleAttribute("disabled", !on);
    });

    /* ---- Hugging Face lookup, two-step pick, and prefill
     * (deep-discovery design Part 2). The lookup is a form
     * aid only: every field stays manually editable, and a
     * failed or empty lookup leaves manual entry untouched.
     * Prefill writes only into empty fields and fields still
     * holding an unedited prefill (P2.8 rule 2); each
     * prefilled field carries its source chip beside the
     * label (principle 11), cleared when the operator edits
     * the field. Nothing is stored until Create model. ---- */
    const pickHost = addModelHost.querySelector("#addModelPick") as HTMLElement | null;
    const nameInput = addModelHost.querySelector("#addModelName") as HTMLInputElement | null;
    const suggestHost = addModelHost.querySelector("#addModelSuggest") as HTMLElement | null;
    interface HfPickState { repoId: string; file: string; url: string; fileName: string; sha256: string | null }
    let hfPick: HfPickState | null = null;
    let lastLookup: HfLookupRepo[] = [];
    let expandedRepo: string | null = null;
    const prefilled = new Set<string>();
    const fieldOf = (sel: string): HTMLInputElement | null => addModelHost.querySelector(sel) as HTMLInputElement | null;
    const setChip = (key: string, text: string | null): void => {
      const slot = addModelHost.querySelector(`#addModel${key}Src`) as HTMLElement | null;
      if (slot) slot.innerHTML = text ? `<span class="chip">${esc(text)}</span>` : "";
    };
    const setPrefill = (sel: string, key: string, value: string, chip: string): void => {
      const el = fieldOf(sel);
      if (!el) return;
      if (el.value.trim() !== "" && !prefilled.has(key)) return;
      el.value = value;
      prefilled.add(key);
      setChip(key, chip);
    };
    const watchEdit = (sel: string, key: string): void => {
      fieldOf(sel)?.addEventListener("input", () => { prefilled.delete(key); setChip(key, null); });
    };
    watchEdit("#addModelId", "Id");
    watchEdit("#addModelUrl", "Url");
    watchEdit("#addModelQuant", "Quant");
    watchEdit("#addModelFile", "File");
    watchEdit("#addModelParams", "Params");
    watchEdit("#addModelMoeLayers", "MoeLayers");
    watchEdit("#addModelMoeDefault", "MoeDefault");
    fieldOf("#addModelMtp")?.addEventListener("change", () => setChip("Mtp", null));
    addMoeCheck?.addEventListener("change", () => setChip("Moe", null));
    const setOptionalOpen = (open: boolean): void => {
      const box = addModelHost.querySelector("#addModelOptional") as HTMLElement | null;
      if (!box) return;
      box.style.display = open ? "" : "none";
      optToggle?.setAttribute("aria-expanded", String(open));
      if (optToggle) optToggle.innerHTML = icon(open ? "chevD" : "chevR");
    };
    const fitLabel = (fit: string): string => fit === "fits" ? "Fits" : fit === "fits_with_split" ? "Fits with split" : fit === "does_not_fit" ? "Does not fit" : "";
    const quantRowHtml = (r: HfLookupRepo, q: HfLookupQuant): string => {
      const fit = fitLabel(q.fit);
      return `<div style="display:flex;gap:8px;align-items:center;margin-top:6px;flex-wrap:wrap">
        <button class="btn" data-hf-quant="${esc(q.file)}" data-hf-repo="${esc(r.repo_id)}">${icon("check")} Pick</button>
        <span style="font-size:12px;color:var(--text2)">File: ${esc(q.file)} · Size: ${esc(fmtBytes(q.size_bytes))}${fit ? ` · Fit: ${fit}` : ""}</span>
      </div>`;
    };
    const renderPickList = (): void => {
      if (!pickHost) return;
      pickHost.innerHTML = lastLookup.map((r) => {
        const facts = [
          r.params_b != null ? `Params: ${trim1(r.params_b)}B` : "",
          r.architecture ? `Architecture: ${esc(r.architecture)}` : "",
          r.ctx_max != null ? `Rated context: ${fmtCount(r.ctx_max)}` : "",
          `Downloads: ${fmtCount(r.downloads)}`,
        ].filter(Boolean).join(" · ");
        const open = expandedRepo === r.repo_id;
        return `<div style="border:1px solid var(--border);border-radius:6px;padding:8px;margin-top:8px">
          <div style="display:flex;gap:8px;align-items:center;flex-wrap:wrap">
            <span style="font-size:12px;color:var(--text2)">Repo:</span>
            <button class="btn" data-hf-repo="${esc(r.repo_id)}" style="font-family:var(--mono);font-size:12px">${icon(open ? "chevD" : "chevR")} ${esc(r.repo_id)}</button>
            ${r.catalog_model_id ? `<span class="chip">Catalog</span>` : ""}
            <span style="font-size:12px;color:var(--text2)">${facts}</span>
          </div>
          ${open ? r.quants.map((q) => quantRowHtml(r, q)).join("") : ""}
        </div>`;
      }).join("");
    };
    const renderPickSummary = (note: string): void => {
      if (!pickHost || !hfPick) return;
      pickHost.innerHTML = `<div style="display:flex;gap:8px;align-items:center;margin-top:8px;flex-wrap:wrap">
          <span style="font-size:12px;color:var(--text2)">Repo: ${esc(hfPick.repoId)} · File: ${esc(hfPick.file)}</span>
          <button class="btn" id="addModelPickChange" style="padding:2px 8px;font-size:11px">Change</button>
        </div>
        ${note ? `<div style="font-size:12px;color:var(--text2);margin-top:4px">${esc(note)}</div>` : ""}`;
      pickHost.querySelector("#addModelPickChange")?.addEventListener("click", () => {
        expandedRepo = hfPick?.repoId ?? null;
        renderPickList();
      });
    };
    const applyPick = async (repo: HfLookupRepo, q: HfLookupQuant): Promise<void> => {
      const url = q.source_url;
      const fileName = q.file.split("/").pop() ?? q.file;
      hfPick = { repoId: repo.repo_id, file: q.file, url, fileName, sha256: q.shard_count === 1 ? q.sha256 : null };
      setPrefill("#addModelId", "Id", q.id_slug, "Hugging Face");
      setPrefill("#addModelUrl", "Url", url, "Hugging Face");
      setOptionalOpen(true);
      setPrefill("#addModelFile", "File", fileName, "Hugging Face");
      if (q.quant) setPrefill("#addModelQuant", "Quant", q.quant, "Hugging Face");
      if (repo.params_b != null) setPrefill("#addModelParams", "Params", trim1(repo.params_b), "Hugging Face");
      renderPickSummary("");
      // The pick-time header read (P2-D4): geometry and the
      // MTP signal prefill the capability declarations as
      // proposals. Any failure leaves the form as the record
      // prefill left it — the add is never blocked by it.
      const hr = await modelHfHeader(repo.repo_id, q.file);
      if (!hr.live || hr.error || !hr.data.header) {
        renderPickSummary("File header not read — MoE/MTP not prefilled.");
        return;
      }
      if (hr.data.catalog) {
        // A catalog file's pins govern; no fetched value is
        // prefilled over them (§P2.8 rule 1).
        renderPickSummary("Catalog model — its figures come from the curated catalog.");
        return;
      }
      const hdr = hr.data.header;
      let note = "";
      if (hdr.moe_layers != null) {
        const moeBox = fieldOf("#addModelMoe");
        if (moeBox && !moeBox.checked) {
          moeBox.checked = true;
          setChip("Moe", "File header");
        }
        fieldOf("#addModelMoeLayers")?.toggleAttribute("disabled", false);
        fieldOf("#addModelMoeDefault")?.toggleAttribute("disabled", false);
        setPrefill("#addModelMoeLayers", "MoeLayers", String(hdr.moe_layers), "File header");
        if (hdr.suggested_default_offload_layers != null) {
          setPrefill("#addModelMoeDefault", "MoeDefault", String(hdr.suggested_default_offload_layers), "Suggested");
        }
        if (hdr.needs_split) note = "Needs a tensor split to fit one GPU.";
      }
      if (hdr.mtp) {
        const mtpBox = fieldOf("#addModelMtp");
        if (mtpBox && !mtpBox.checked) {
          mtpBox.checked = true;
          setChip("Mtp", "File header");
        }
      }
      renderPickSummary(note);
    };
    const runLookup = async (query: string, expandRepo?: string): Promise<void> => {
      if (!pickHost) return;
      if (!isTauri()) {
        pickHost.innerHTML = `<div style="font-size:12px;color:var(--text2);margin-top:8px">Sample mode: the Hugging Face lookup runs in the installed app.</div>`;
        return;
      }
      pickHost.innerHTML = `<div style="font-size:12px;color:var(--text2);margin-top:8px">Looking up…</div>`;
      const r = await modelHfLookup(query);
      if (r.error) {
        pickHost.innerHTML = `<div style="font-size:12px;color:var(--err);margin-top:8px">Lookup failed: ${esc(r.error)}</div>`;
        return;
      }
      lastLookup = r.data.repos;
      expandedRepo = expandRepo ?? (lastLookup.length === 1 ? lastLookup[0].repo_id : null);
      if (!lastLookup.length) {
        pickHost.innerHTML = `<div style="font-size:12px;color:var(--text2);margin-top:8px">No repositories matched.</div>`;
        return;
      }
      renderPickList();
    };
    addModelHost.querySelector("#addModelLookupBtn")?.addEventListener("click", () => {
      const q = nameInput?.value.trim() ?? "";
      if (!q) {
        if (pickHost) pickHost.innerHTML = `<div style="font-size:12px;color:var(--err);margin-top:8px">Enter a model name to look up.</div>`;
        return;
      }
      hideSuggest();
      void runLookup(q);
    });
    pickHost?.addEventListener("click", (ev) => {
      const t = ev.target as HTMLElement;
      const quantBtn = t.closest("[data-hf-quant]") as HTMLElement | null;
      if (quantBtn) {
        const repo = lastLookup.find((r) => r.repo_id === quantBtn.getAttribute("data-hf-repo"));
        const q = repo?.quants.find((x) => x.file === quantBtn.getAttribute("data-hf-quant"));
        if (repo && q) void applyPick(repo, q);
        return;
      }
      const repoBtn = t.closest("[data-hf-repo]") as HTMLElement | null;
      if (repoBtn) {
        const rid = repoBtn.getAttribute("data-hf-repo");
        expandedRepo = expandedRepo === rid ? null : rid;
        renderPickList();
      }
    });
    /* Type-ahead on the name field: repo-name suggestions
     * from the same shallow search the lookup runs (P2-D2
     * depth), one search per 300 ms debounce window.
     * Selecting a suggestion — pointer or keyboard — runs
     * the full lookup for that repo. No results or a failed
     * search simply show no dropdown; typing is never
     * interrupted. */
    let suggestTimer: number | null = null;
    let suggestSeq = 0;
    let suggestActive = -1;
    let suggestRepos: { repo_id: string; downloads: number }[] = [];
    const hideSuggest = (): void => {
      if (suggestHost) suggestHost.style.display = "none";
      suggestActive = -1;
    };
    const renderSuggest = (): void => {
      if (!suggestHost) return;
      if (!suggestRepos.length) { hideSuggest(); return; }
      suggestHost.innerHTML = suggestRepos.map((r, i) => `<div data-hf-suggest="${esc(r.repo_id)}" style="padding:6px 8px;font-size:12px;cursor:pointer;${i === suggestActive ? "background:var(--border)" : ""}"><span style="font-family:var(--mono)">${esc(r.repo_id)}</span> <span style="color:var(--text2)">· Downloads: ${fmtCount(r.downloads)}</span></div>`).join("");
      suggestHost.style.display = "";
    };
    const chooseSuggest = (repoId: string): void => {
      if (nameInput) nameInput.value = repoId;
      hideSuggest();
      void runLookup(repoId, repoId);
    };
    nameInput?.addEventListener("input", () => {
      if (suggestTimer !== null) window.clearTimeout(suggestTimer);
      const q = nameInput.value.trim();
      if (q.length < 2 || !isTauri()) { suggestRepos = []; hideSuggest(); return; }
      suggestTimer = window.setTimeout(async () => {
        const seq = ++suggestSeq;
        const r = await modelHfSuggest(q);
        if (seq !== suggestSeq) return;
        if (r.error || !r.live) { suggestRepos = []; hideSuggest(); return; }
        suggestRepos = r.data.repos;
        suggestActive = -1;
        renderSuggest();
      }, 300);
    });
    nameInput?.addEventListener("keydown", (ev) => {
      if (!suggestRepos.length || (suggestHost && suggestHost.style.display === "none")) return;
      if (ev.key === "ArrowDown") {
        ev.preventDefault();
        suggestActive = Math.min(suggestActive + 1, suggestRepos.length - 1);
        renderSuggest();
      } else if (ev.key === "ArrowUp") {
        ev.preventDefault();
        suggestActive = Math.max(suggestActive - 1, 0);
        renderSuggest();
      } else if (ev.key === "Enter" && suggestActive >= 0) {
        ev.preventDefault();
        chooseSuggest(suggestRepos[suggestActive].repo_id);
      } else if (ev.key === "Escape") {
        hideSuggest();
      }
    });
    suggestHost?.addEventListener("mousedown", (ev) => {
      const row = (ev.target as HTMLElement).closest("[data-hf-suggest]") as HTMLElement | null;
      if (row) {
        ev.preventDefault();
        chooseSuggest(row.getAttribute("data-hf-suggest") ?? "");
      }
    });
    nameInput?.addEventListener("blur", () => { window.setTimeout(hideSuggest, 150); });
    const addBtn = addModelHost.querySelector("#addModelBtn") as HTMLButtonElement | null;
    addBtn?.addEventListener("click", async () => {
      const msg = addModelHost.querySelector("#addModelMsg") as HTMLElement | null;
      // The message row stays hidden until its first write.
      if (msg) msg.style.display = "";
      const val = (sel: string): string => (addModelHost.querySelector(sel) as HTMLInputElement | null)?.value.trim() ?? "";
      const id = val("#addModelId");
      const url = val("#addModelUrl");
      if (!isTauri()) { if (msg) msg.textContent = "Sample mode: adding models runs in the installed app."; return; }
      if (!id || !url) {
        if (msg) { msg.style.color = "var(--err)"; msg.textContent = "Model id and source URL are required."; }
        return;
      }
      const paramsRaw = val("#addModelParams");
      let paramsB: number | undefined;
      if (paramsRaw) {
        const n = Number(paramsRaw);
        if (!Number.isFinite(n) || n <= 0) {
          if (msg) { msg.style.color = "var(--err)"; msg.textContent = "Parameters (B) must be a positive number (the model's total parameter count, e.g. 80)."; }
          return;
        }
        paramsB = n;
      }
      // The MoE pair is read only when its checkbox is checked;
      // checked requires both fields (the declaration is atomic).
      const moeChecked = (addModelHost.querySelector("#addModelMoe") as HTMLInputElement | null)?.checked ?? false;
      let moeLayers: number | undefined;
      let moeDefault: number | undefined;
      if (moeChecked) {
        const moeLayersRaw = val("#addModelMoeLayers");
        const moeDefRaw = val("#addModelMoeDefault");
        const l = Number(moeLayersRaw);
        const d = Number(moeDefRaw);
        if (!moeLayersRaw || !moeDefRaw || !Number.isInteger(l) || !Number.isInteger(d) || l < 1 || d < 0 || d > l) {
          if (msg) { msg.style.color = "var(--err)"; msg.textContent = "MoE is checked: give both fields — MoE layers of 1 or more and a default offload between 0 and the layer count."; }
          return;
        }
        moeLayers = l;
        moeDefault = d;
      }
      const mtpChecked = (addModelHost.querySelector("#addModelMtp") as HTMLInputElement | null)?.checked ?? false;
      // A pick's hash pins the add only while the form still
      // names the picked file at the picked URL (P2-D6): an
      // operator who redirected the form at another model
      // must not inherit the pick's pin.
      const fileVal = val("#addModelFile");
      let pickSha: string | undefined;
      if (hfPick && hfPick.sha256 && url === hfPick.url && (!fileVal || fileVal === hfPick.fileName)) {
        pickSha = hfPick.sha256;
      }
      addBtn.disabled = true;
      const r = await modelAdd(id, url, fileVal || undefined, val("#addModelQuant") || undefined, paramsB, moeLayers, moeDefault, mtpChecked ? true : undefined, pickSha);
      addBtn.disabled = false;
      if (r.error) {
        // Failure: the form stays open with the operator's input
        // intact and the error in the form's message row.
        if (msg) { msg.style.color = "var(--err)"; msg.textContent = `Add failed: ${r.error}`; }
        return;
      }
      // Success: clear and hide the form (emptying the host drops
      // every field with it), then repaint so the new model shows.
      closeAddModelForm();
      await load();
    });
  };
  root.querySelector("#modelAdd")?.addEventListener("click", openAddModelForm);

  await load();

  return () => {
    disposed = true;
    if (pollTimer !== null) window.clearInterval(pollTimer);
    root.removeEventListener("click", onClick);
    root.removeEventListener("click", onOvGoto);
    root.removeEventListener("input", syncRenameDraft);
    root.removeEventListener("input", syncOffloadDraft);
    root.removeEventListener("change", syncOffloadChecks);
    root.removeEventListener("keydown", onRenameKey);
  };
}
