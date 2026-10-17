/* Secondary views — all wired to the backend through api.ts.
   Each view follows the Setup wizard pattern: renderX() returns a shell
   with a loading placeholder; wireX(root) loads live data, paints it, and
   attaches handlers. Under Tauri the data is real; in a plain browser
   (live=false) views show a sample-mode banner and honest empty states
   instead of fabricated data. */

import { fmtGb, gaugeRing, sparkline, trim1 } from "../charts";
import { icon, type IconName } from "../icons";
import { esc } from "../html";
import {
  runWizardStep, wizardBusyMessage, isTauri, openUrl, markSetupComplete, getWizardSteps, runAudit,
  getWizardCommands,
  getGpuTelemetry, setTelemetryInterval, resetTelemetryPeaks, saveTelemetrySnapshot,
  getBackends, startBackend, stopBackend, restartBackend, updateBackendFlags, setBackendOffload, getBackendLaunchCommand,
  getBackendConfig, applyBackendConfig, testBackend, placementPropose, placementApply,
  addBackend, deleteBackend, modelAssign,
  getModels, downloadModel, cancelDownload, pauseModelDownload, resumeModelDownload,
  verifyModel, deleteModel, modelSetEnabled, modelAdd, modelSetShortName,
  getLogs, exportDiagnostics, openFolder, getGatewayConfig, getGatewayStatus, getGatewayYaml,
  getGatewayMasterKey, setGatewayBindHost,
  updateGatewayConfig, effectiveGatewayStrategy, gatewayStop,
  getActivationPlan, activationCancelChanges, activationComplete, quiesceWait,
  getAppConfig, getMxcPolicy, getMxcActivity, setMxcMode, mxcRunSelfTest,
} from "../api";
import type { ActivationPlanData, BackendOffload, BackendRow, GatewayConfigData, GatewayGroup, GatewayStatusData, LogRow, ModelRow, MxcActivityData, MxcPolicyData, PlacementProjectionData, PlannedCommandWire } from "../api";
import type { GpuInfo, WizardStepState } from "../types";

const C = { blue: "#4a9eff", purple: "#a371f7", teal: "#39c5cf", green: "#3fb950", amber: "#d29922", red: "#f85149", grey: "#6d7686" };

const SELECT_STYLE = "background:var(--panel);color:var(--text);border:1px solid var(--border);border-radius:8px;padding:6px 8px;font-size:12px";

function topbar(title: string, extra = ""): string {
  return `<div class="topbar"><h1>${title}</h1>
    <span class="spacer"></span>${extra}</div>`;
}

function dot(s: string): string {
  return `<span class="dot ${s === "ok" ? "ok" : s === "warn" || s === "starting" || s === "degraded" ? "warn" : s === "err" ? "err" : "idle"}"></span>`;
}

/** Map a supervisor process state onto the dot() vocabulary. */
function stateDot(state: string): string {
  return dot(state === "running" ? "ok"
    : state === "starting" || state === "stopping" ? "starting"
    : state === "failed" ? "err" : "idle");
}

/** Status/health button for an overview-list row: a colored dot and
 *  icon; clicking it scrolls to the item's card (data-ov-goto holds
 *  the card's element id). kind colors both the dot and the icon. */
function ovStatusBtn(kind: "ok" | "warn" | "err" | "idle", ic: IconName, target: string, label: string): string {
  return `<button class="ov-goto ${kind}" data-ov-goto="${esc(target)}" title="${esc(label)}">${dot(kind)}${icon(ic)}</button>`;
}

/** Shared click handler for overview status buttons: scroll the
 *  target card into view and flash its outline briefly. */
function onOvGoto(ev: Event): void {
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
function bannerHtml(r: LoadMeta, stale = false): string {
  if (r.error) return `<div class="banner" style="margin-bottom:12px">${icon("err")} Backend error \u2014 ${esc(r.error)}</div>`;
  if (!r.live) return `<div class="banner" style="margin-bottom:12px">${icon("warn")} Sample mode \u2014 open the installed app for live data from this machine.</div>`;
  if (stale) return `<div class="banner" style="margin-bottom:12px">${icon("warn")} Telemetry is stale \u2014 showing the last received sample.</div>`;
  return "";
}

function setBanner(root: HTMLElement, id: string, html: string): void {
  const el = root.querySelector(`#${id}`);
  if (el) el.innerHTML = html;
}

function loadingHtml(label: string): string {
  return `<div class="card" style="color:var(--text2)">${icon("clock")} ${esc(label)}</div>`;
}

function emptyHtml(text: string): string {
  return `<div class="card" style="color:var(--text2)">${esc(text)}</div>`;
}

function fmtBytes(n: number | null): string {
  if (n === null) return "\u2014";
  if (n <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = n;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) { v /= 1024; u++; }
  return `${v >= 100 ? v.toFixed(0) : trim1(v)} ${units[u]}`;
}

function fmtUptime(secs: number | null): string {
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

function fmtTime(ts: string): string {
  const d = new Date(ts);
  return Number.isNaN(d.getTime()) ? ts : d.toLocaleTimeString("en-GB");
}

function fmtDate(ts: string): string {
  const d = new Date(ts);
  return Number.isNaN(d.getTime()) ? ts : d.toLocaleDateString("en-GB");
}

/* ---------------- GPUs ---------------- */

/** One model resident on a GPU, for the card's read-only listing:
 *  the configured share of the model on this GPU and the measured
 *  VRAM the serving backend holds here. */
/** One resident-model row on a GPU card. `mib` is the measured VRAM
 *  held, or null when per-process usage was not measured (WDDM GPU,
 *  counter data unavailable) — the row's presence then comes from the
 *  configured split alone and the held figure renders as unmeasured. */
export interface GpuModelRow { model: string; backend: string; pct: number; mib: number | null }

function gpuCard(g: GpuInfo, expanded: Set<string>, modelRows: GpuModelRow[] = []): string {
  const vramPct = g.vram_total_gb > 0 ? Math.round((g.vram_used_gb / g.vram_total_gb) * 100) : 0;
  // pci/driver are absent from the telemetry payload; omit them when
  // unset (esc(undefined) would print the literal text "undefined").
  const pci = g.pci ? ` <span class="port" style="color:var(--text3);font-family:var(--mono);font-size:11px">${esc(g.pci)}</span>` : "";
  const driver = g.driver ? `<span style="color:var(--text3);font-size:11px;font-family:var(--mono)">${esc(g.driver)}</span>` : "";
  // Peak temperature accumulates since the last reset (or app start
  //  when never reset); the window is stated so the figure is not read
  //  as a short-term max.
  const peak = g.temp_peak_c !== undefined ? ` \u00b7 peak ${g.temp_peak_c} \u00b0C since last reset or app start` : "";
  const limit = g.power_limit_w !== undefined ? ` / ${g.power_limit_w} W limit` : "";
  const staleChip = g.status === "degraded" ? ` <span class="chip badge-warn">stale</span>` : "";
  const downChip = g.status === "down" ? ` <span class="chip badge-err">down</span>` : "";
  // A down GPU's readings are its last known values; say so on the
  // card, with the recovery fact for the fallen-off-the-bus case.
  const downNote = g.status === "down" ? `<div style="font-size:12px;color:var(--err);margin-top:8px">Down \u2014 not reported by the telemetry source in recent polls; the readings shown are the last known values. A GPU that has fallen off the bus (nvidia-smi reports it as lost) recovers only on a system reboot.</div>` : "";
  const memUtil = g.mem_util_pct !== undefined
    ? `<div class="mrow"><span class="mlabel">Memory util</span><span class="mval">${g.mem_util_pct}%</span></div>` : "";
  const fan = g.fan_pct !== undefined
    ? `<div class="mrow"><span class="mlabel">Fan</span><span class="mval">${g.fan_pct}%</span></div>` : "";
  const tempSpark = g.temp_spark && g.temp_spark.length
    ? `<div class="mrow"><span></span>${sparkline(g.temp_spark, 240, 26, C.amber)}</div>` : "";
  const powerSpark = g.power_spark && g.power_spark.length
    ? `<div class="mrow"><span></span>${sparkline(g.power_spark, 240, 26, C.green)}</div>` : "";
  // A backend process holding under 1 GiB on a GPU holds only its
  // CUDA context there, not model weights (the smallest catalog model
  // is ~4.7 GB) — label it as such. Presence in this table is process
  // attribution, not placement: a split backend legitimately appears
  // on several GPUs, and an unsplit one appears on every GPU it
  // initialized a context on while its weights sit on the main GPU.
  // A null vram_mib is unmeasured (WDDM GPU, counter data
  // unavailable): it renders as "not measured" and never earns the
  // context-only label — unknown is not a small number.
  const procRow = (p: { process: string; pid: number; vram_mib: number | null; backend: string }): string => {
    const key = `${g.id}:${p.pid}`;
    const open = expanded.has(key);
    const label = p.backend !== "—" ? p.backend : baseName(p.process);
    const measured = p.vram_mib !== null;
    const ctx = p.backend !== "—" && p.vram_mib !== null && p.vram_mib < 1024 ? ` <span class="chip">context only</span>` : "";
    const head = `<div data-proc-toggle="${esc(key)}" role="button" tabindex="0" style="display:flex;gap:8px;align-items:center;padding:5px 6px;cursor:pointer;border-bottom:1px solid #232a34">
      <span style="color:var(--text3);font-size:10px">${open ? "\u25be" : "\u25b8"}</span>
      <span style="flex:1;min-width:0;font-size:12px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap">${esc(label)}${ctx}</span>
      <span style="font-size:12px;font-family:var(--mono)">${measured ? `${p.vram_mib} MiB` : "not measured"}</span></div>`;
    const detail = open ? `<div style="padding:6px 6px 8px 24px;font-size:11px;color:var(--text2);display:grid;gap:2px;border-bottom:1px solid #232a34">
      <div>Process: ${esc(p.process)}</div>
      <div>PID: ${p.pid}</div>
      <div>VRAM on GPU ${esc(g.id)}: ${measured ? `${p.vram_mib} MiB` : "not measured — per-process accounting is unavailable for this GPU"}</div>
      <div>Backend: ${p.backend !== "—" ? esc(p.backend) : "not attributed to a backend"}</div>
      ${p.backend !== "—" && p.vram_mib !== null && p.vram_mib < 1024 ? `<div>Under 1 GiB: this process holds only its CUDA context on this GPU, not model weights.</div>` : ""}
    </div>` : "";
    return head + detail;
  };
  const procs = g.processes && g.processes.length
    ? `<div style="margin-top:10px"><div style="font-size:11px;color:var(--text3);margin-bottom:4px">PROCESSES (${g.processes.length}) \u2014 click a row for detail</div>
      <div style="max-height:176px;overflow-y:auto;border:1px solid #232a34;border-radius:6px">${g.processes.map(procRow).join("")}</div></div>` : "";
  // Resident models (live mode): read-only — the configured share
  // of the model on this GPU and the VRAM its backend holds here.
  // Sample mode keeps the mock placement lines.
  const models = modelRows.length
    ? `<div style="margin-top:10px"><div style="font-size:11px;color:var(--text3);margin-bottom:4px">MODELS ON THIS GPU</div>
        <div style="display:grid;gap:4px">${modelRows.map((r) => `<div style="font-size:12px">${icon("layers")} ${esc(r.model)} <span style="color:var(--text3)">via ${esc(r.backend)} \u00b7 ${r.pct}% of the model \u00b7 ${r.mib === null ? "VRAM held not measured" : `${fmtGb(r.mib / 1024)} GiB held`}</span></div>`).join("")}</div></div>`
    : g.models && g.models.length
    ? `<div style="margin-top:8px;font-size:11px;color:var(--text3);display:grid;gap:4px">
        ${g.models.map((m) => `<div>${icon("layers")} ${esc(m.model)} \u00b7 ${esc(m.layers)}</div>`).join("")}
      </div>` : "";
  return `<div class="card" id="card-gpu-${esc(g.id)}">
      <div class="gpu-head">${dot(g.status === "down" ? "err" : g.status)} ${esc(g.name)}${pci}${staleChip}${downChip}
        <span class="spacer" style="flex:1"></span>${driver}</div>
      ${downNote}
      <div style="display:flex;gap:18px;justify-content:center;margin:12px 0">
        ${gaugeRing(g.util_pct, 84, C.blue, String(g.util_pct), "UTIL")}
        ${gaugeRing(vramPct, 84, C.purple, fmtGb(g.vram_used_gb), "GiB VRAM")}
      </div>
      <div class="gpu-metrics">
        <div class="mrow"><span class="mlabel">UTILIZATION</span><span class="mval">${g.util_pct}%</span></div>
        <div class="mrow"><span class="mlabel">VRAM</span><span class="mval">${fmtGb(g.vram_used_gb)} / ${fmtGb(g.vram_total_gb)} GiB</span></div>
        <div class="mrow"><span class="mlabel">Temperature</span><span class="mval">${g.temp_c} \u00b0C${peak}</span></div>
        ${tempSpark}
        <div class="mrow"><span class="mlabel">Power</span><span class="mval">${g.power_w == null ? "N/A" : `${g.power_w} W${limit}`}</span></div>
        ${powerSpark}
        ${memUtil}
        ${fan}
      </div>
      ${procs}
      ${models}
    </div>`;
}

export function renderGpus(): string {
  return `${topbar("GPUs", `
    <label style="font-size:12px;color:var(--text2);display:inline-flex;align-items:center;gap:6px">Telemetry poll
      <select id="gpuPollSel" style="${SELECT_STYLE}">
        <option value="0">Off</option>
        <option value="1000">1s</option>
        <option value="5000">5s</option>
      </select></label>
    <button class="btn" id="gpuRefresh">${icon("restart")} Refresh</button>
    <button class="btn" id="gpuResetPeaks">Reset peaks</button>
    <button class="btn" id="gpuSnapshot">${icon("download")} Snapshot</button>`)}
  <div class="view"><div id="gpuBanner"></div>
  <div id="gpuMeta" style="font-size:12px;color:var(--text3);margin-bottom:8px"></div>
  <div style="font-size:11px;color:var(--text3);margin-bottom:8px">This view refreshes every 2 s. The Telemetry poll control sets the sampler cadence and applies on restart.</div>
  <div id="gpuMsg" style="font-size:12px;margin-bottom:8px"></div>
  <div id="gpuOverview" style="margin-bottom:10px"></div>
  <div style="display:flex;gap:10px;align-items:center;margin-bottom:10px;flex-wrap:wrap">
    <label style="font-size:12px;color:var(--text2)">Sort
      <select id="gpuSortSel" style="${SELECT_STYLE}">
        <option value="index">GPU index</option>
        <option value="util">Utilization</option>
        <option value="vram">VRAM used</option>
        <option value="temp">Temperature</option>
      </select></label>
    <input id="gpuFilter" type="text" placeholder="Filter by GPU name" style="background:var(--panel2);border:1px solid var(--line);border-radius:6px;color:var(--text);padding:5px 8px;font-size:12px;min-width:180px">
  </div>
  <div id="gpuBody">${loadingHtml("Loading GPU telemetry\u2026")}</div></div>`;
}

/** The backend cadence buckets: Off, anything up to 1500 ms runs at 1s,
 *  everything above runs at 5s. Used both to initialize the poll select
 *  from the configured value and to word the change confirmation with
 *  the EFFECTIVE cadence. */
function cadenceBucket(ms: number): { value: string; label: string } {
  if (ms <= 0) return { value: "0", label: "off" };
  if (ms <= 1500) return { value: "1000", label: "1s" };
  return { value: "5000", label: "5s" };
}

function baseName(p: string): string {
  return p.split(/[\\/]/).pop() ?? p;
}

/** Does a backend's model_file resolve to a model's gguf_path?
 *  model_assign writes the gguf_path verbatim, so an exact match is
 *  the assigned state; a basename match covers configs written with
 *  a different directory prefix. */
function sameModelFile(modelFile: string, ggufPath: string): boolean {
  return modelFile !== "" && (modelFile === ggufPath || baseName(modelFile) === baseName(ggufPath));
}

export async function wireGpus(root: HTMLElement): Promise<() => void> {
  let disposed = false;
  let loading = false;
  /** Latest live GPU list (index order as reported), for placement math. */
  let lastGpus: GpuInfo[] = [];
  /** Age anchor for the placement snapshot label when the projection
   *  itself carries no snapshot_age_ms. */
  let lastAtMs: number | null = null;
  /** Expanded process rows (key "gpuId:pid"), sort, and filter state
   *  for the GPU cards — held outside the repainted DOM so the 2 s
   *  telemetry repaint cannot collapse a row the operator opened. */
  const expandedProcs = new Set<string>();
  let gpuSort = "index";
  let gpuFilter = "";
  const paintBody = (): void => {
    const ov = root.querySelector("#gpuOverview");
    if (ov) {
      ov.innerHTML = lastGpus.length ? `<div class="ovlist ov-g">
        <div class="ov-head"><span></span><span>GPU</span><span>Util</span><span>VRAM</span><span>Temp</span><span>Power</span><span>Processes</span></div>
        ${lastGpus.map((g) => {
          const btn = g.status === "ok" ? ovStatusBtn("ok", "check", `card-gpu-${g.id}`, `${g.name}: ok — go to card`)
            : g.status === "down" ? ovStatusBtn("err", "err", `card-gpu-${g.id}`, `${g.name}: down — go to card`)
            : g.status === "degraded" || g.status === "starting" ? ovStatusBtn("warn", "warn", `card-gpu-${g.id}`, `${g.name}: ${g.status} — go to card`)
            : ovStatusBtn("idle", "pause", `card-gpu-${g.id}`, `${g.name}: ${g.status} — go to card`);
          return `<div class="ov-row">${btn}<span class="ov-name">${esc(g.name)}</span><span class="ov-dim">${g.util_pct}%</span><span class="ov-dim">${fmtGb(g.vram_used_gb)} / ${fmtGb(g.vram_total_gb)} GiB</span><span class="ov-dim">${g.temp_c} \u00b0C</span><span class="ov-dim">${g.power_w == null ? "N/A" : `${g.power_w} W`}</span><span class="ov-dim">${(g.processes ?? []).length}</span></div>`;
        }).join("")}</div>` : "";
    }
    const b = root.querySelector("#gpuBody");
    if (!b) return;
    const f = gpuFilter.trim().toLowerCase();
    let list = lastGpus.filter((g) => !f || g.name.toLowerCase().includes(f));
    if (gpuSort === "util") list = [...list].sort((a, z) => z.util_pct - a.util_pct);
    else if (gpuSort === "vram") list = [...list].sort((a, z) => z.vram_used_gb - a.vram_used_gb);
    else if (gpuSort === "temp") list = [...list].sort((a, z) => z.temp_c - a.temp_c);
    b.innerHTML = list.length
      ? `<div class="cards3">${list.map((g) => gpuCard(g, expandedProcs, modelRowsFor(g))).join("")}</div>`
      : emptyHtml("No GPUs match the filter.");
  };

  const msg = (text: string, isErr = false): void => {
    const m = root.querySelector("#gpuMsg");
    if (m) m.innerHTML = text ? `<span style="color:${isErr ? "var(--err)" : "var(--text2)"}">${esc(text)}</span>` : "";
  };

  const load = async (): Promise<void> => {
    if (loading) return;
    loading = true;
    try {
      // Backends + models for the read-only resident-model listing;
      // failures keep the previous fetch.
      const [br, mr] = await Promise.all([getBackends(), getModels()]);
      if (disposed) return;
      if (br.live && !br.error) gpuBackends = br.data;
      if (mr.live && !mr.error) gpuModels = mr.data;
      const r = await getGpuTelemetry();
      if (disposed) return;
      const srcErr = r.data.sourceError;
      // A failing telemetry source outranks the generic banners: with
      // samples it means "showing the last received data", without any
      // it is an error state distinct from an empty GPU list.
      if (srcErr && !r.error) {
        setBanner(root, "gpuBanner", `<div class="banner" style="margin-bottom:12px">${icon("err")} Telemetry source failing: ${esc(srcErr)}${r.data.gpus.length ? " \u2014 showing the last received samples." : ""}</div>`);
      } else {
        setBanner(root, "gpuBanner", bannerHtml(r, r.data.stale));
      }
      const meta = root.querySelector("#gpuMeta");
      if (meta) {
        if (r.live && r.data.source) {
          const ageS = r.data.atMs !== null ? Math.max(0, Math.round((Date.now() - r.data.atMs) / 1000)) : null;
          meta.textContent = `Telemetry source: ${r.data.source} \u00b7 ${ageS === null || ageS < 2 ? "updated just now" : `updated ${ageS}s ago`}`;
        } else {
          meta.textContent = "";
        }
      }
      const b = root.querySelector("#gpuBody");
      if (!b) return;
      if (r.error) { b.innerHTML = emptyHtml(`GPU telemetry unavailable: ${r.error}`); return; }
      if (!r.data.gpus.length) {
        lastGpus = [];
        b.innerHTML = emptyHtml(srcErr
          ? `Telemetry source failing: ${srcErr}`
          : r.live && !r.data.hasPolled
            ? "Waiting for the first telemetry sample\u2026"
            : r.live
              ? "No GPUs reported by telemetry. The telemetry source returns an empty GPU list for this machine."
              : "No GPU data in sample mode.");
        return;
      }
      lastGpus = r.data.gpus;
      lastAtMs = r.data.atMs;
      paintBody();
    } finally {
      loading = false;
    }
  };

  /* ---------------- Resident models (read-only) ----------------
     Placement moved to the backend cards (Pete's direction,
     2026-10-16): the split is a per-backend setting, edited there.
     This view only reports: each GPU card lists the models present
     on it — the configured percentage from the serving backend's
     tensor split (100% when unsplit) and the measured VRAM held,
     using the Service Map's residency rule (a bare CUDA context
     under 1 GiB is not a model presence). The backends/models fetch
     rides the telemetry load; failures keep the previous data. */
  let gpuBackends: BackendRow[] = [];
  let gpuModels: ModelRow[] = [];
  const modelRowsFor = (g: GpuInfo): GpuModelRow[] => {
    const pos = lastGpus.findIndex((x) => x.id === g.id);
    const rows: GpuModelRow[] = [];
    for (const b of gpuBackends) {
      if (!b.configured || (b.state !== "running" && b.state !== "starting")) continue;
      const mdl = gpuModels.find((m) => sameModelFile(b.model_file, m.gguf_path));
      // Sum of the measured rows for this backend on this GPU; null
      // when no row was measured (WDDM, counters unavailable) — an
      // unmeasured backend must not read as holding 0, and must not
      // create a presence claim from measurement it does not have.
      const heldRows = (g.processes ?? [])
        .filter((p) => p.backend === b.id && p.vram_mib !== null)
        .map((p) => p.vram_mib as number);
      const measured: number | null = heldRows.length
        ? heldRows.reduce((s, v) => s + v, 0)
        : null;
      const split = b.tensor_split ?? [];
      const frac = pos >= 0 && split.length ? split[pos] : undefined;
      if (frac !== undefined && frac > 0) {
        rows.push({ model: mdl?.id ?? baseName(b.model_file), backend: b.id, pct: Math.round(frac * 100), mib: measured });
      } else if (!split.length && measured !== null && measured >= 1024) {
        rows.push({ model: mdl?.id ?? baseName(b.model_file), backend: b.id, pct: 100, mib: measured });
      }
    }
    return rows;
  };

  /* ---------------- Telemetry controls ---------------- */
  const refreshBtn = root.querySelector("#gpuRefresh");
  const resetBtn = root.querySelector("#gpuResetPeaks");
  const snapshotBtn = root.querySelector("#gpuSnapshot");
  const pollSel = root.querySelector("#gpuPollSel") as HTMLSelectElement | null;

  const onRefresh = (): void => { msg(""); void load(); };
  const onResetPeaks = async (ev: Event): Promise<void> => {
    const btn = ev.currentTarget as HTMLButtonElement;
    btn.disabled = true;
    const r = await resetTelemetryPeaks();
    btn.disabled = false;
    if (r.error) { msg(`Reset peaks failed: ${r.error}`, true); return; }
    msg("Peaks reset.");
    await load();
  };
  const onSnapshot = async (ev: Event): Promise<void> => {
    const btn = ev.currentTarget as HTMLButtonElement;
    btn.disabled = true;
    const r = await saveTelemetrySnapshot();
    btn.disabled = false;
    if (r.error) { msg(`Snapshot failed: ${r.error}`, true); return; }
    msg(r.data.path ? `Telemetry snapshot saved to ${r.data.path}.` : "Telemetry snapshot saved.");
  };
  const onPollChange = async (ev: Event): Promise<void> => {
    const ms = Number((ev.target as HTMLSelectElement).value);
    const r = await setTelemetryInterval(ms);
    if (r.error) { msg(`Poll interval change failed: ${r.error}`, true); return; }
    const eff = cadenceBucket(ms);
    msg(eff.value === "0"
      ? "Telemetry polling off \u2014 applies on restart."
      : `Poll interval set to ${eff.label} \u2014 applies on restart.`);
  };

  refreshBtn?.addEventListener("click", onRefresh);
  resetBtn?.addEventListener("click", onResetPeaks);
  snapshotBtn?.addEventListener("click", onSnapshot);
  pollSel?.addEventListener("change", onPollChange);
  const gpuSortSel = root.querySelector("#gpuSortSel") as HTMLSelectElement | null;
  const gpuFilterInput = root.querySelector("#gpuFilter") as HTMLInputElement | null;
  gpuSortSel?.addEventListener("change", () => { gpuSort = gpuSortSel.value; paintBody(); });
  gpuFilterInput?.addEventListener("input", () => { gpuFilter = gpuFilterInput.value; paintBody(); });
  root.addEventListener("click", (ev) => {
    const t = (ev.target as HTMLElement).closest("[data-proc-toggle]");
    if (!t) return;
    const key = t.getAttribute("data-proc-toggle") ?? "";
    if (!key) return;
    if (expandedProcs.has(key)) expandedProcs.delete(key); else expandedProcs.add(key);
    paintBody();
  });
  root.addEventListener("click", onOvGoto);

  // Initialize the poll select from the configured interval, bucketed
  // to the cadence the backend actually honors.
  if (pollSel) {
    pollSel.value = "1000";
    const cfgR = await getAppConfig();
    if (!disposed && cfgR.data) {
      const tel = cfgR.data.telemetry as { poll_interval_ms?: unknown } | undefined;
      if (typeof tel?.poll_interval_ms === "number") {
        pollSel.value = cadenceBucket(tel.poll_interval_ms).value;
      }
    }
  }

  await load();

  // Live refresh: telemetry ages even when the user does nothing.
  const timer = window.setInterval(() => {
    if (!disposed) { void load(); }
  }, 2000);

  return () => {
    disposed = true;
    window.clearInterval(timer);
    root.removeEventListener("click", onOvGoto);
    refreshBtn?.removeEventListener("click", onRefresh);
    resetBtn?.removeEventListener("click", onResetPeaks);
    snapshotBtn?.removeEventListener("click", onSnapshot);
    pollSel?.removeEventListener("change", onPollChange);
  };
}

/* ---------------- Backends ---------------- */

type CardNote = { text: string; err: boolean };

/** Quote an argv element for display when it contains whitespace or a
 *  quote, so the rendered launch command stays copy-paste accurate. */
function quoteArg(a: string): string {
  return /[\s"]/.test(a) ? `"${a.replace(/"/g, '\\"')}"` : a;
}

/** Short label for model controls: the catalog display name when
 *  known, otherwise the model id. The GGUF filename is deliberately
 *  omitted — it duplicates the identity at much greater width. */
function modelShortName(m: ModelRow): string {
  return m.name?.trim() || m.id;
}

/** Compact GPU name for the Placement split chips: the vendor
 *  prefix is dropped ("NVIDIA RTX A4000" -> "RTX A4000") so the
 *  name fits on the chip's second line. The full telemetry name
 *  stays available as the chip's tooltip. */
function gpuShortName(name: string): string {
  return name.replace(/^nvidia\s+/i, "").trim() || name;
}

/** Short label for an unregistered model file: its filename stem,
 *  without the directory or the .gguf suffix. */
function modelFileShortName(path: string): string {
  return path ? baseName(path).replace(/\.gguf$/i, "") : "no model assigned";
}

/** Options for a backend card's model dropdown: every registered
 *  model, labeled by short name only, with the one the backend
 *  currently serves selected. When the backend's model_file matches
 *  no registered model (a custom path), a leading option carries the
 *  current file's short name so the control shows the true state;
 *  choosing it is a no-op. */
function modelOptions(b: BackendRow, models: ModelRow[]): string {
  const matched = models.some((m) => sameModelFile(b.model_file, m.gguf_path));
  const lead = matched ? "" : `<option value="" selected>${esc(modelFileShortName(b.model_file))} (current)</option>`;
  return lead + models.map((m) =>
    `<option value="${esc(m.id)}"${sameModelFile(b.model_file, m.gguf_path) ? " selected" : ""}>${esc(modelShortName(m))}</option>`).join("");
}

/* ---------------- Gateway activation plan ----------------
 * Saved configuration changes that are not live yet — gateway
 * changes waiting on a gateway restart, backend changes (flags,
 * tensor split, model assignment) waiting on that backend's
 * (re)start. The card renders from the server-side plan (a
 * persisted journal plus live config drift), so it survives
 * navigation and app restarts until the operator executes, defers,
 * or cancels it — the pre-v0.1.22 behavior (a transient card on
 * the add-backend form, nothing after dismissal) is what Pete
 * reported on 2026-10-15. The same plan drives the app shell's
 * global banner and the dedicated Activation Plan view. */

interface PlanUiState {
  dismissed: boolean;
  status: string;
  statusErr: boolean;
  confirmCancel: boolean;
  acting: boolean;
  /** Manual-command block from the last failed start-gateway
   * outcome, rendered under the status line. */
  manualCommand: string;
}

function newPlanUi(): PlanUiState {
  return { dismissed: false, status: "", statusErr: false, confirmCancel: false, acting: false, manualCommand: "" };
}

/** The manual-command code block shared by the plan card surfaces:
 * the block's own first line says what it is, so it renders bare. */
function manualCommandHtml(cmd: string): string {
  return `<pre style="margin:6px 0;font-family:var(--mono);font-size:11px;white-space:pre-wrap;word-break:break-all;background:var(--panel);border:1px solid var(--border);border-radius:6px;padding:8px;text-align:left">${esc(cmd)}</pre>`;
}

/** Scope + activation label for one plan change, e.g.
 *  "Backend: coder-fast · restart required". */
function planChangeMeta(c: ActivationPlanData["changes"][number]): string {
  const scope = c.scope === "backend"
    ? `Backend${c.targets && c.targets.length ? `: ${c.targets.join(", ")}` : ""}`
    : "Gateway";
  const act = c.activation === "restart" || c.activation === "gateway_restart"
    ? "restart required"
    : "applies at next start";
  return `${scope} · ${act}`;
}

function activationPlanHtml(plan: ActivationPlanData | null, ui: PlanUiState): string {
  if (!plan || !plan.pending) return "";
  const n = plan.changes.length;
  const dis = ui.acting ? " disabled" : "";
  if (ui.dismissed) {
    return `<div class="card" style="margin-bottom:12px;border-color:var(--amber)">
      <div style="font-size:12px">${icon("warn")} <b>Activation pending</b> — ${n ? `${n} saved change${n === 1 ? "" : "s"}` : "Saved changes"} not live yet.
      <button class="btn" data-plan-act="review"${dis}>Review plan</button></div></div>`;
  }
  const changeList = n
    ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">${plan.changes.map((c) => `<div>${esc(c.summary)} <span style="color:var(--text3)">${esc(planChangeMeta(c))} · ${esc(fmtTime(c.ts))}</span></div>`).join("")}</div>`
    : `<div style="font-size:12px;color:var(--text2);margin-top:6px">The running gateway's configuration differs from the saved configuration.</div>`;
  const stateLine = plan.actionable
    ? "Apply restarts what is running (backends first, then the gateway); changes whose process is stopped take effect at its next start."
    : "Nothing running holds the old state — these changes take effect at the next start of the listed backends or the gateway.";
  const cancelBtn = plan.snapshot_available
    ? (ui.confirmCancel
      ? `<button class="btn" data-plan-act="cancel-confirm"${dis}>Confirm cancel changes</button>`
      : `<button class="btn" data-plan-act="cancel"${dis}>Cancel changes</button>`)
    : "";
  const restartBtns = !plan.actionable
    ? ""
    : plan.gateway_pending && plan.gateway_up
      ? `<button class="btn" data-plan-act="quiesce"${dis}>${icon("pause")} Quiesce, then apply</button>
       <button class="btn" data-plan-act="restart"${dis}>${icon("restart")} Apply now</button>`
      : `<button class="btn" data-plan-act="start"${dis}>${icon("play")} Apply plan</button>`;
  return `<div class="card" style="margin-bottom:12px;border-color:var(--amber)">
    <h3>${icon("warn")} Activation plan — saved changes are not live</h3>
    ${changeList}
    <div style="font-size:12px;color:var(--text2);margin-top:6px">${stateLine}</div>
    <div class="card-actions">${restartBtns}
      <button class="btn" data-plan-act="later"${dis}>Decide later</button>
      ${cancelBtn}</div>
    ${ui.confirmCancel && !ui.status ? `<div style="font-size:12px;margin-top:6px;color:var(--text2)">Cancelling restores the configuration from before these changes and clears the plan.</div>` : ""}
    ${ui.status ? `<div style="font-size:12px;margin-top:6px;color:${ui.statusErr ? "var(--err)" : "var(--text2)"}">${esc(ui.status)}</div>` : ""}
    ${ui.manualCommand ? manualCommandHtml(ui.manualCommand) : ""}
  </div>`;
}

/** Execute the restart side of an activation plan with the
 *  existing primitives: the quiesce wait (optional), gateway stop,
 *  the start-gateway wizard step, then mark the plan complete.
 *  `setManual` receives the failed start-gateway outcome's
 *  manual-command block (or null to clear a previous one), rendered
 *  as a code block under the failure message. */
async function executeActivationRestart(
  mode: "quiesce" | "restart" | "start",
  setStatus: (text: string, err?: boolean) => void,
  setManual?: (cmd: string | null) => void,
): Promise<boolean> {
  setManual?.(null);
  if (mode === "quiesce") {
    setStatus("Waiting for in-flight gateway requests to finish (up to 120 seconds)…");
    const q = await quiesceWait(120);
    if (q.error) { setStatus(`Quiesce check failed: ${q.error}`, true); return false; }
    if (!q.data.drained) {
      setStatus(`${q.data.in_flight} request(s) still in flight after the wait — restarting now.`);
    }
  }
  if (mode !== "start") {
    const st = await getGatewayStatus();
    if (st.data.up) {
      setStatus("Stopping the running gateway…");
      const sr = await gatewayStop();
      if (sr.error) { setStatus(`Gateway stop failed: ${sr.error}`, true); return false; }
    }
  }
  setStatus("Running the start-gateway step…");
  const r = await runWizardStep("start-gateway");
  if (r.error) { setStatus(`Gateway start failed: ${r.error}`, true); return false; }
  if (r.data.busy || !r.data.outcome) {
    setStatus(wizardBusyMessage(r.data, "A setup step is already running — wait for it to finish, then execute the plan again."), true);
    return false;
  }
  if (r.data.outcome.state === "failed") {
    setStatus(`Start-gateway step failed — ${r.data.outcome.message}`, true);
    if (r.data.outcome.manual_command) setManual?.(r.data.outcome.manual_command);
    return false;
  }
  await activationComplete();
  setStatus("Gateway restarted — the saved changes are live.");
  return true;
}

/** Apply a whole activation plan across scopes: restart each
 *  running backend the plan names (its saved flags, split, or
 *  model take effect), then the gateway side when a gateway change
 *  is pending (quiesce wait, stop, start-gateway step, mark the
 *  gateway scope complete). Backend entries for stopped backends
 *  are left alone — they apply at that backend's next start and
 *  resolve in the plan on their own. Exported for the app shell's
 *  global activation banner, whose Apply button runs the same
 *  execution as the plan card and the Activation Plan view. */
export async function applyActivationPlan(
  plan: ActivationPlanData,
  mode: "quiesce" | "restart" | "start",
  setStatus: (text: string, err?: boolean) => void,
  setManual?: (cmd: string | null) => void,
): Promise<boolean> {
  for (const id of plan.backend_restart_ids ?? []) {
    setStatus(`Restarting backend ${id}…`);
    const r = await restartBackend(id);
    if (r.error) { setStatus(`Backend ${id} restart failed: ${r.error}`, true); return false; }
  }
  if (plan.gateway_pending) {
    return executeActivationRestart(mode, setStatus, setManual);
  }
  setStatus("Backends restarted — the saved changes are live.");
  return true;
}

/** Shared click handling for the plan card's buttons, used by the
 *  Backends and Gateway views. `repaint` re-renders the card in
 *  place; `reload` refreshes the whole view afterwards. */
async function handlePlanAction(
  act: string,
  ui: PlanUiState,
  repaint: () => void,
  reload: () => Promise<void>,
  plan: ActivationPlanData | null,
): Promise<void> {
  if (act === "review") { ui.dismissed = false; repaint(); return; }
  if (act === "later") {
    ui.dismissed = true; ui.confirmCancel = false; ui.status = "";
    repaint();
    return;
  }
  if (act === "cancel") { ui.confirmCancel = true; repaint(); return; }
  if (ui.acting) return;
  if (act === "cancel-confirm") {
    ui.acting = true;
    repaint();
    const r = await activationCancelChanges();
    ui.acting = false;
    ui.confirmCancel = false;
    if (r.error) {
      ui.status = `Cancel failed: ${r.error}`;
      ui.statusErr = true;
      repaint();
      return;
    }
    ui.status = r.data.cancelled
      ? "Changes cancelled — the previous configuration was restored."
      : "Nothing to cancel — the plan had already resolved.";
    ui.statusErr = false;
    repaint();
    await reload();
    return;
  }
  if (act === "quiesce" || act === "restart" || act === "start") {
    if (!plan) return;
    ui.acting = true;
    const setStatus = (text: string, err = false): void => {
      ui.status = text;
      ui.statusErr = err;
      repaint();
    };
    const setManual = (cmd: string | null): void => {
      ui.manualCommand = cmd ?? "";
      repaint();
    };
    await applyActivationPlan(plan, act, setStatus, setManual);
    ui.acting = false;
    repaint();
    await reload();
  }
}

/** The dedicated Activation Plan view: the shared plan card (with
 *  its Apply / Decide later / Cancel actions) over the full
 *  pending-changes table — every change with its scope, targets,
 *  and how it goes live — plus the live gateway state. The global
 *  banner's Review button navigates here. */
export function renderActivationPlan(): string {
  return `${topbar("Activation Plan")}
  <div class="view"><div id="activationBody">${loadingHtml("Loading activation plan…")}</div></div>`;
}

export async function wireActivationPlan(root: HTMLElement): Promise<() => void> {
  const planUi = newPlanUi();
  let lastPlan: ActivationPlanData | null = null;
  let disposed = false;
  let timer: number | null = null;

  const paint = (): void => {
    const b = root.querySelector("#activationBody");
    if (!b) return;
    if (!lastPlan || !lastPlan.pending) {
      b.innerHTML = `<div class="card"><h3>${icon("check")} No activation plan pending</h3>
        <div style="font-size:12px;color:var(--text2);margin-top:6px">Every saved configuration change is live. A change that needs a restart to take effect — gateway routes, a backend's flags, tensor split, or model assignment — appears here, and in the banner at the top of the window, until it is applied.</div>
        ${planUi.status ? `<div style="font-size:12px;margin-top:6px;color:${planUi.statusErr ? "var(--err)" : "var(--text2)"}">${esc(planUi.status)}</div>` : ""}
        ${planUi.manualCommand ? manualCommandHtml(planUi.manualCommand) : ""}
        <div class="card-actions"><button class="btn" data-plan-refresh>Refresh</button></div></div>`;
      return;
    }
    const p = lastPlan;
    const rows = p.changes.map((c) => `<tr><td>${esc(fmtTime(c.ts))}</td><td>${esc(c.summary)}</td><td>${c.scope === "backend" ? "Backend" : "Gateway"}</td><td>${esc((c.targets ?? []).join(", ") || "\u2014")}</td><td>${c.activation === "next_start" ? "Next start" : "Restart"}</td></tr>`).join("");
    const detail = `<div class="card"><h3>${icon("logs")} Pending changes</h3>
      <table class="dtab" style="margin-top:8px"><thead><tr><th>Saved</th><th>Change</th><th>Scope</th><th>Targets</th><th>Activation</th></tr></thead><tbody>${rows || `<tr><td colspan="5">The running gateway's configuration differs from the saved configuration (drift).</td></tr>`}</tbody></table>
      <div style="font-size:12px;color:var(--text2);margin-top:8px">Gateway: ${p.gateway_up ? "running" : "not running"}${p.drift ? " · the saved gateway configuration differs from the running one (drift)" : ""}</div>
      <div class="card-actions"><button class="btn" data-plan-refresh>Refresh</button></div></div>`;
    b.innerHTML = activationPlanHtml(p, planUi) + detail;
  };

  const load = async (): Promise<void> => {
    const r = await getActivationPlan();
    if (disposed) return;
    lastPlan = r.data;
    paint();
  };

  const onClick = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLElement;
    if (t.closest("[data-plan-refresh]")) { await load(); return; }
    const pt = t.closest("[data-plan-act]") as HTMLElement | null;
    if (!pt) return;
    await handlePlanAction(pt.getAttribute("data-plan-act") ?? "", planUi, paint, load, lastPlan);
  };
  root.addEventListener("click", onClick);
  timer = window.setInterval(() => { void load(); }, 10000);
  await load();

  return () => {
    disposed = true;
    if (timer !== null) window.clearInterval(timer);
    root.removeEventListener("click", onClick);
  };
}

/** Context-length presets offered by the flags editor (tokens). */
const CTX_PRESETS = [8192, 16384, 32768, 65536, 131072, 262144, 524288, 1048576];
/** Batch-size presets offered by the flags editor. */
const BATCH_PRESETS = [128, 256, 512, 1024, 2048, 4096];

/** 131072 -> "128K"; values under 1024 stay plain. */
function ctxShort(n: number): string {
  return n >= 1024 ? `${n / 1024}K` : String(n);
}

/** Expert-offload weight figures for a slider position, priced
 *  with the same byte arithmetic as the backend memory requirement
 *  (manager_models::memory_requirement): the offloaded share of
 *  the routed experts sits in system RAM, the remaining weights
 *  on the GPU. Weights only — the KV cache is offload-independent
 *  and shown on the card. */
function offloadFigures(o: BackendOffload, layers: number): { gpuMib: number; sysMib: number } {
  const sysBytes = Math.floor((o.expert_bytes * layers) / o.moe_layers);
  const mib = (bytes: number): number => Math.ceil(bytes / 1048576);
  return { gpuMib: mib(o.total_bytes - sysBytes), sysMib: mib(sysBytes) };
}

/** The flag tokens a slider position writes — the canonical forms
 *  the app itself writes (backend_set_offload). */
function offloadTokenText(o: BackendOffload, layers: number): string {
  if (layers <= 0) return "no offload tokens";
  if (layers >= o.moe_layers) return "-ot exps=CPU";
  return `--n-cpu-moe ${layers}`;
}

/** The offload slider track's green-to-yellow-to-red gradient for
 *  a model's band boundaries (fractions of the offloaded range,
 *  computed by the backend from the catalog green limit). */
function offloadGradient(o: BackendOffload): string {
  const pct = (f: number): string => `${(f * 100).toFixed(1)}%`;
  return `linear-gradient(90deg, ${C.green} 0%, ${C.green} ${pct(o.green_end)}, ${C.amber} ${pct(o.green_end)}, ${C.amber} ${pct(o.yellow_end)}, ${C.red} ${pct(o.yellow_end)}, ${C.red} 100%)`;
}

/** Draft state for the in-card flags form, non-null only for the
 *  backend whose form is open. ctxMax/modelId describe the backend's
 *  model (its rated context from the catalog), for annotating
 *  presets beyond the rating. offloadLayers is the offload slider's
 *  draft position (MoE layers on system RAM), null when the
 *  backend's model is not offload-capable. */
export interface FlagsDraft { ctx: number; batch: number; ctxMax: number | null; modelId: string | null; offloadLayers: number | null }

/** State for the in-card Placement form (tensor split editing,
 *  moved here from the GPUs page — Pete's direction, 2026-10-16),
 *  non-null only for the backend whose form is open. gpus mirrors
 *  the latest telemetry GPU list for the include/exclude
 *  checkboxes; excluded holds the unchecked GPU indices;
 *  projection is the last successful propose result for this
 *  backend; busy disables the form's buttons while a propose or
 *  apply invoke is in flight. */
export interface PlaceCardState {
  gpus: { index: number; name: string }[];
  excluded: Set<number>;
  modelId: string | null;
  projection: PlacementProjectionData | null;
  busy: boolean;
}

/** Color class for a backend state tag: green while running, red
 *  when stopped or failed, amber while changing state, grey for
 *  any other state. */
function stateTagClass(state: string): string {
  if (state === "running") return "status-on";
  if (state === "stopped" || state === "failed") return "status-bad";
  if (state === "starting" || state === "stopping") return "status-transition";
  return "status-off";
}

function backendCard(
  b: BackendRow,
  openArgv: Set<string>,
  openMenus: Set<string>,
  argvCache: Map<string, string[]>,
  msgs: Map<string, CardNote>,
  logTails: Map<string, LogRow[]>,
  inFlight: Set<string>,
  models: ModelRow[],
  confirmRemove: Set<string>,
  residency: Map<string, { gpu: number; name: string; mib: number | null }[]>,
  gpuNames: Map<number, string>,
  flags: FlagsDraft | null,
  place: PlaceCardState | null,
): string {
  const active = b.state === "running" || b.state === "starting";
  // Buttons render disabled while this card's action is in flight.
  // The set lives outside the repainted DOM, so the disabled state
  // survives the repaints a poll or a log-tail fetch triggers.
  const dis = inFlight.has(b.id) ? " disabled" : "";
  const stopBtn = `<button class="btn" data-back-act="stop" data-back-id="${esc(b.id)}"${dis}>${icon("pause")} Stop</button>`;
  const restartBtn = `<button class="btn" data-back-act="restart" data-back-id="${esc(b.id)}"${dis}>${icon("restart")} Restart</button>`;
  const startBtn = `<button class="btn" data-back-act="start" data-back-id="${esc(b.id)}"${dis}>${icon("play")} Start</button>`;
  const testBtn = `<button class="btn" data-back-act="test" data-back-id="${esc(b.id)}"${dis}>${icon("bolt")} Test</button>`;
  const enableBtn = `<button class="btn" data-back-act="enable" data-back-id="${esc(b.id)}"${dis}>${icon("check")} Enable</button>`;
  const disableBtn = `<button class="btn icon-only" data-back-act="disable" data-back-id="${esc(b.id)}" title="Disable backend" aria-label="Disable ${esc(b.id)}"${dis}>${icon("ban")}</button>`;
  const removeBtn = `<button class="btn icon-only danger${confirmRemove.has(b.id) ? " armed" : ""}" data-back-act="remove" data-back-id="${esc(b.id)}" title="${confirmRemove.has(b.id) ? "Confirm remove" : "Remove backend"}" aria-label="${confirmRemove.has(b.id) ? "Confirm remove" : "Remove"} ${esc(b.id)}"${dis}>${icon("trash")}</button>`;
  // Offer only what the command layer will accept:
  // - Stop whenever a live process exists (pid present, running or
  //   starting), whether the backend is configured, enabled, or an
  //   orphan — stopping is always allowed.
  // - Start/Restart only for configured, enabled backends; no Start
  //   while a stop is still draining ("stopping").
  // - Enable for configured disabled backends; Disable only while
  //   stopped/failed (a running backend is stopped, not disabled).
  // - Test only while running.
  const parts: string[] = [];
  if (b.pid !== null && active) parts.push(stopBtn);
  if (b.configured && b.enabled && active) parts.push(restartBtn);
  if (b.configured && b.enabled && !active && b.state !== "stopping") parts.push(startBtn);
  if (b.configured && !b.enabled) parts.push(enableBtn);
  if (b.configured && b.enabled && (b.state === "stopped" || b.state === "failed")) parts.push(disableBtn);
  if (b.state === "running") parts.push(testBtn);
  // Removal is refused by the command layer while running; offer it
  // only for configured backends with no live process.
  if (b.configured && !active && b.state !== "stopping") parts.push(removeBtn);
  const actions = parts.join("\n        ");
  // Advanced, infrequently used options live behind one hamburger
  // menu so the card's primary action row stays within the card.
  // Menu-open state lives outside the repainted DOM, like the other
  // card state, so a poll repaint cannot close an open menu.
  const menuOpen = openMenus.has(b.id);
  const advancedMenu = `<div class="advanced-menu-wrap" data-back-menu-wrap>
      <button class="btn icon-only" data-back-menu="${esc(b.id)}" aria-haspopup="menu" aria-expanded="${menuOpen}" title="Advanced options" aria-label="Advanced options for ${esc(b.id)}"${dis}>${icon("menu")}</button>
      ${menuOpen ? `<div class="advanced-menu" role="menu" aria-label="Advanced options for ${esc(b.id)}">
        ${b.configured ? `<button class="menu-item" role="menuitem" data-back-menu-item data-back-act="flags" data-back-id="${esc(b.id)}"${dis}>${icon("gear")} Edit flags</button>
        <button class="menu-item" role="menuitem" data-back-menu-item data-back-act="placement" data-back-id="${esc(b.id)}"${dis}>${icon("layers")} Placement</button>` : ""}
        <button class="menu-item" role="menuitem" data-back-menu-item data-back-act="argv" data-back-id="${esc(b.id)}"${dis}>${icon("logs")} Launch command</button>
      </div>` : ""}
    </div>`;
  const argvOpen = openArgv.has(b.id);
  const argv = argvOpen
    ? `<div style="margin-top:10px;font-size:11px;color:var(--text3)">Launch command (next start)</div>
      <pre style="margin-top:4px;padding:10px 12px;background:var(--panel2);border:1px solid var(--border);border-radius:8px;font-family:var(--mono);font-size:11px;white-space:pre-wrap;word-break:break-all">${esc((argvCache.get(b.id) ?? []).map(quoteArg).join(" "))}</pre>`
    : "";
  const note = msgs.get(b.id);
  const msgHtml = note
    ? `<span style="color:${note.err ? "var(--err)" : "var(--text2)"}">${esc(note.text)}</span>` : "";
  // The orphan note follows the process state — a failed or stopping
  // orphan is not "still running".
  const orphanNote = !b.configured
    ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">${b.state === "failed"
      ? "It failed, and its entry was removed from the configuration. It cannot be restarted."
      : active
        ? "Still running, but its entry was removed from the configuration. It can be stopped here; it cannot be restarted."
        : "Its entry was removed from the configuration. It cannot be restarted."}</div>`
    : "";
  // An adopted backend was started by a previous launch of the app
  // (its processes survive the app closing) and taken over by pid:
  // it can be stopped/restarted here, but its earlier output is not
  // in this session's log ring.
  const adoptedNote = b.adopted
    ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">Started by a previous launch and adopted by this session. Output from before adoption is not in the Logs view.</div>`
    : "";
  let logsBlock = "";
  if (b.state === "failed") {
    const tail = logTails.get(b.id);
    const lines = tail && tail.length
      ? `<div class="logstream" style="margin-top:8px">${tail.map((r) => `<div class="ln"><span class="t">${esc(fmtTime(r.ts))}</span> <span class="chip">${esc(r.stream)}</span> ${esc(r.line)}</div>`).join("")}</div>`
      : tail
        ? `<div style="margin-top:8px;font-size:12px;color:var(--text3)">No log lines captured for this backend.</div>`
        : "";
    const caption = tail !== undefined
      ? `<div style="margin-top:8px;font-size:11px;color:var(--text3)">Latest output (stderr preferred)</div>` : "";
    logsBlock = `${caption}${lines}<div style="margin-top:6px;font-size:12px"><a href="#/logs?scope=${esc(encodeURIComponent(b.id))}" style="color:var(--accent)">Open in Logs</a></div>`;
  }
  // GPU residency for this backend. Measured per-process telemetry
  // is authoritative while the backend is active: which GPUs its
  // process actually holds memory on (under 1 GiB is a CUDA context
  // only, labeled as such). When the backend is inactive there is no
  // measured residency, so the list falls back to the configured
  // tensor split — where the model will load at its next start —
  // with a segmented bar of the split fractions.
  const res = residency.get(b.id) ?? [];
  const gpuColors = ["#4a9eff", "#a371f7", "#39c5cf", "#d29922", "#3fb950", "#f85149"];
  const splitEntries = (b.tensor_split ?? [])
    .map((frac, index) => ({ index, frac }))
    .filter((e) => e.frac > 0);
  const gpuLabel = (index: number): string => {
    const measured = res.find((r) => r.gpu === index);
    return `GPU ${index}${(measured?.name ?? gpuNames.get(index)) ? ` · ${esc(measured?.name ?? gpuNames.get(index) ?? "")}` : ""}`;
  };
  const splitSummary = splitEntries.length
    ? splitEntries.map((e) => `${gpuLabel(e.index)} — ${Math.round(e.frac * 100)}%`).join(" · ")
    : "";
  const splitBar = splitEntries.length > 1
    ? `<div class="split-bar" aria-hidden="true">${splitEntries.map((e) => `<i style="width:${e.frac * 100}%;background:${gpuColors[e.index % gpuColors.length]}"></i>`).join("")}</div>`
    : "";
  let gpuRows: string;
  let gpuBasis: string;
  // Measured rows carry a figure; unmeasured rows (mib null — WDDM
  // GPU, counter data unavailable) prove presence only. They render
  // as "not measured": no figure, no context-only label, and they do
  // not make the section claim a measured basis on their own.
  const knownRes = res.filter((r) => r.mib !== null);
  if (knownRes.length) {
    gpuBasis = "measured";
    gpuRows = res.map((r) => r.mib !== null
      ? `<div class="backend-gpu-row"><span>${gpuLabel(r.gpu)}</span><span>${fmtGb(r.mib / 1024)} GiB${r.mib < 1024 ? " · context only" : ""}</span></div>`
      : `<div class="backend-gpu-row"><span>${gpuLabel(r.gpu)}</span><span>not measured</span></div>`).join("");
  } else if (res.length) {
    gpuBasis = "not measured";
    gpuRows = res.map((r) => `<div class="backend-gpu-row"><span>${gpuLabel(r.gpu)}</span><span>not measured</span></div>`).join("")
      + `<div class="backend-gpu-empty">Per-process usage is not measured on this GPU — placement follows the configured split.</div>`;
  } else if (active) {
    gpuBasis = "measured";
    gpuRows = `<div class="backend-gpu-empty">Not measured — no process attributed in the latest telemetry sample</div>`;
  } else if (splitEntries.length) {
    gpuBasis = "configured split";
    gpuRows = splitBar + splitEntries.map((e) => `<div class="backend-gpu-row"><span>${gpuLabel(e.index)}</span><span>${Math.round(e.frac * 100)}%</span></div>`).join("");
  } else {
    gpuBasis = "";
    gpuRows = `<div class="backend-gpu-empty">Not loaded</div>`;
  }
  const gpuHtml = b.configured ? `<div class="backend-gpus">
      <div class="backend-gpus-title">GPUs${gpuBasis ? ` (${gpuBasis})` : ""}</div>
      ${gpuRows}
      ${res.length && splitEntries.length ? `<div class="backend-gpu-configured">Configured split: ${splitSummary}</div>` : ""}
    </div>` : "";
  // Gateway statistics for this backend (trailing hour, from the
  // LiteLLM metrics window store). Absent until a window exists.
  const statsHtml = b.requests_1h !== undefined || b.errors_1h !== undefined || b.throughput_toks !== undefined
    ? `<div style="display:grid;grid-template-columns:repeat(3,1fr);gap:10px;margin-top:8px">
        ${[["Requests (1h)", b.requests_1h !== undefined ? b.requests_1h.toLocaleString() : "—"],
           ["Errors (1h)", b.errors_1h !== undefined ? String(b.errors_1h) : "—"],
           ["Throughput", b.throughput_toks !== undefined ? `${trim1(b.throughput_toks)} tok/s` : "—"]]
          .map(([k, v]) => `<div class="kv"><span class="k">${k}</span><span class="v" style="font-size:13px">${esc(v)}</span></div>`).join("")}
      </div>`
    : (b.configured && b.enabled ? `<div style="font-size:11px;color:var(--text3);margin-top:6px">Gateway stats: no metrics window yet for this backend.</div>` : "");
  // The flags editor renders inside this card (Pete's request,
  // 2026-10-16): preset dropdowns built from the model's rated
  // context, with a Custom numeric input when the value is not a
  // preset. Field events sync to the wire-level draft state, so the
  // poll repaint re-renders the form without losing input.
  // The expert-offload slider rides the same panel when the
  // backend's model is offload-capable: its position is the number
  // of MoE layers whose routed experts sit in system RAM. The
  // track's green band ends at the model's green limit (its
  // catalog default offload — the most offload that stays fast);
  // yellow and red split the range past it. Sliding updates the
  // readout in place; releasing the slider saves through
  // backend_set_offload, which rewrites only the offload tokens in
  // the backend's raw flags.
  const offloadHtml = flags && b.offload ? (() => {
    const o = b.offload;
    const n = flags.offloadLayers ?? o.layers;
    const figs = offloadFigures(o, n);
    return `<div style="margin-top:12px">
      <label style="font-size:12px;color:var(--text2);display:block">Expert offload to system RAM — <span data-offload-layers-label>${n} of ${o.moe_layers} MoE layers</span><span class="info-tip" title="Routed-expert weights for the first N MoE layers run from system RAM instead of the GPU, freeing GPU memory at some speed cost. Green is this model's catalog default — the most offload that stays fast; past it, CPU expert computation increasingly dominates generation speed. Figures are weights only; the KV cache stays on the GPU. The slider saves when released and applies on the backend's next start.">${icon("info")}</span></label>
      <input type="range" class="offload-range" min="0" max="${o.moe_layers}" step="1" value="${n}" data-offload-slider data-offload-backend="${esc(b.id)}" style="background:${offloadGradient(o)}" aria-label="Expert offload layers for ${esc(b.id)}"${dis}>
      <div style="font-size:12px;color:var(--text2);margin-top:4px"><span data-offload-token style="font-family:var(--mono)">${esc(offloadTokenText(o, n))}</span> · GPU weights <span data-offload-gpu>${fmtGb(figs.gpuMib / 1024)}</span> GiB · System RAM <span data-offload-sys>${fmtGb(figs.sysMib / 1024)}</span> GiB</div>
    </div>`;
  })() : "";
  const flagsHtml = flags ? `<div class="backend-form-panel flags-panel">
      <div style="font-size:12px;font-weight:600">${icon("gear")} Flags for ${esc(b.id)}</div>
      <div style="display:grid;grid-template-columns:1fr 1fr;gap:10px;margin-top:8px">
        <label style="font-size:12px;color:var(--text2)">Context length (-c)
          <span style="display:flex;gap:6px;margin-top:4px">
            <select data-flags-field="ctx-preset" style="${SELECT_STYLE};flex:1;min-width:0"${dis}>
              ${CTX_PRESETS.map((v) => `<option value="${v}"${v === flags.ctx ? " selected" : ""}>${v.toLocaleString()} (${ctxShort(v)})${v === 32768 ? " \u2014 default" : ""}${flags.ctxMax && v > flags.ctxMax ? " \u2014 beyond rated" : ""}</option>`).join("")}
              <option value="custom"${CTX_PRESETS.includes(flags.ctx) ? "" : " selected"}>Custom\u2026</option>
            </select>
            ${CTX_PRESETS.includes(flags.ctx) ? "" : `<input data-flags-field="ctx-custom" type="number" value="${flags.ctx}" min="1024" max="1048576" style="${SELECT_STYLE};width:110px"${dis}>`}
          </span></label>
        <label style="font-size:12px;color:var(--text2)">Batch size (-b)
          <span style="display:flex;gap:6px;margin-top:4px">
            <select data-flags-field="batch-preset" style="${SELECT_STYLE};flex:1;min-width:0"${dis}>
              ${BATCH_PRESETS.map((v) => `<option value="${v}"${v === flags.batch ? " selected" : ""}>${v.toLocaleString()}${v === 512 ? " \u2014 default" : ""}</option>`).join("")}
              <option value="custom"${BATCH_PRESETS.includes(flags.batch) ? "" : " selected"}>Custom\u2026</option>
            </select>
            ${BATCH_PRESETS.includes(flags.batch) ? "" : `<input data-flags-field="batch-custom" type="number" value="${flags.batch}" min="1" style="${SELECT_STYLE};width:110px"${dis}>`}
          </span></label>
      </div>
      ${offloadHtml}
      <div style="font-size:12px;color:var(--text2);margin-top:6px">${flags.modelId && flags.ctxMax
        ? `Model ${esc(flags.modelId)} is rated to ${ctxShort(flags.ctxMax)} context (${flags.ctxMax.toLocaleString()} tokens); larger values are accepted but exceed the rating. `
        : "Accepted context range: 1,024 to 1,048,576. "}Saved to the configuration immediately; applies on the backend's next start. Tensor split is set by the Placement form on this card.</div>
      <div class="card-actions"><button class="btn" data-back-act="flags-save" data-back-id="${esc(b.id)}"${dis}>Save flags</button>
        <button class="btn" data-back-act="flags-cancel" data-back-id="${esc(b.id)}">Cancel</button></div>
    </div>` : "";
  // The Placement form renders inside this card (moved from the
  // GPUs page, Pete's direction, 2026-10-16): the tensor split is a
  // per-backend setting, so it is edited here like the flags. The
  // model is derived from the backend's configuration; the
  // projection renders in the card; Apply is scoped to this
  // backend. Form state lives in the wire layer (checkbox changes
  // sync to wire state), so poll repaints keep the form intact.
  const placeDis = dis || (place && place.busy) ? " disabled" : "";
  const projHtml = place && place.projection ? (() => {
    const p = place.projection;
    // When the running backend's current allocation is credited,
    // the table shows the credit basis: what it already holds on
    // each GPU and the additional allocation that must fit in free
    // VRAM. Without a credit the two extra columns carry no
    // information and are omitted.
    const credited = (p.current_mib ?? []).some((v) => v > 0);
    const rows = p.split.map((frac, i) => {
      const gi = p.gpu_indices && p.gpu_indices[i] !== undefined ? p.gpu_indices[i] : i;
      const gname = place.gpus.find((g) => g.index === gi)?.name ?? `GPU ${gi}`;
      const free = p.free_mib && p.free_mib[i] !== undefined ? `${p.free_mib[i]} MiB` : "\u2014";
      const fit = frac === 0 ? "not used" : p.per_gpu_fits[i] ? "fits" : "does not fit";
      const creditCells = credited ? `<td>${p.current_mib?.[i] ?? 0} MiB</td><td>${p.additional_mib?.[i] ?? p.per_gpu_mib[i]} MiB</td>` : "";
      return `<tr><td>${esc(gname)}</td><td>${Math.round(frac * 100)}%</td><td>${p.per_gpu_mib[i]} MiB</td>${creditCells}<td>${free}</td><td>${fit}</td></tr>`;
    }).join("");
    const ageMs = p.snapshot_age_ms ?? null;
    return `<div style="margin-top:10px">
      ${p.down_indices && p.down_indices.length ? `<div style="font-size:12px;color:var(--err);margin-bottom:6px">${p.down_indices.map((i) => `GPU ${i}`).join(", ")} ${p.down_indices.length === 1 ? "is" : "are"} down and ${p.down_indices.length === 1 ? "is" : "are"} treated as having no free VRAM.</div>` : ""}
      <div style="font-size:12px">${p.fits ? "The model fits the selected GPUs with the projected split." : "The model does not fit the available VRAM with this split."}</div>
      <table class="dtab" style="margin-top:8px"><thead><tr><th>GPU</th><th>Fraction</th><th>Required</th>${credited ? `<th title="The weight allocation this backend already holds on the GPU. Applying the split restarts the backend, freeing it first.">Current</th><th title="Required minus current: what must fit in free VRAM.">Additional</th>` : ""}<th>Free (current)</th><th>Fit</th></tr></thead><tbody>${rows}</tbody></table>
      ${p.sys_mib ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">System RAM: ${fmtGb(p.sys_mib / 1024)} GiB — routed experts under expert offload. The per-GPU figures cover the GPU-resident weights only.</div>` : ""}
      <div style="font-size:12px;color:var(--text2);margin-top:6px">Projection uses model weights only; run-time allocations add overhead.</div>
      <div style="font-size:12px;color:var(--text2);margin-top:2px">Based on ${ageMs === null ? "the latest telemetry snapshot" : `a telemetry snapshot ${Math.round(ageMs / 1000)} seconds old`}.</div></div>`;
  })() : "";
  const placeHtml = place ? `<div class="backend-form-panel placement-panel">
      <div style="font-size:12px;font-weight:600">${icon("layers")} Placement for ${esc(b.id)}</div>
      ${place.modelId
        ? ""
        : `<div style="font-size:12px;color:var(--err);margin-top:4px">This backend's model file matches no configured model, so a split cannot be proposed or applied for it.</div>`}
      ${place.gpus.length ? `<div style="margin-top:8px">
        <span style="font-size:12px;color:var(--text2)">GPUs in the split:</span><span class="info-tip" title="The split divides the model's weights across the selected GPUs, in nvidia-smi index order. Uncheck a GPU to leave it out \u2014 for example the display GPU, whose memory Windows already uses. The split is saved to the backend's configuration and applies on its next start.">${icon("info")}</span>
        <div class="gpu-chip-row">${place.gpus.map((g) => {
          const on = !place.excluded.has(g.index);
          return `<label class="gpu-chip${on ? " on" : ""}${placeDis ? " is-disabled" : ""}" title="${esc(g.name)}"><span class="gpu-chip-top"><input type="checkbox" data-place-gpu="${g.index}" data-place-backend="${esc(b.id)}"${on ? " checked" : ""}${placeDis}> GPU ${g.index}</span><span class="gpu-chip-name">${esc(gpuShortName(g.name))}</span></label>`;
        }).join("")}</div>
      </div>`
      : `<div style="font-size:12px;color:var(--text2);margin-top:6px">No GPU telemetry yet \u2014 a split cannot be proposed until telemetry reports the GPUs.</div>`}
      ${projHtml}
      <div class="card-actions"><button class="btn" data-back-act="place-propose" data-back-id="${esc(b.id)}"${placeDis || !place.modelId || !place.gpus.length ? " disabled" : ""}>Propose split</button>
        <button class="btn" data-back-act="place-apply" data-back-id="${esc(b.id)}"${place.projection && !place.busy && !dis ? "" : " disabled"}>Apply split</button>
        <button class="btn" data-back-act="place-cancel" data-back-id="${esc(b.id)}">Close</button></div>
    </div>` : "";
  return `<div class="card backend-card" id="card-backend-${esc(b.id)}">
      <h3 class="backend-card-head">${stateDot(b.state)} ${esc(b.id)} <span class="port">:${b.port}</span>
        <span class="spacer" style="flex:1"></span>
        ${b.configured ? "" : `<span class="chip badge-warn">not in config</span>`}
        ${b.configured ? `<span class="chip status-tag ${b.enabled ? "status-on" : "status-off"}">${b.enabled ? "enabled" : "disabled"}</span>` : ""}
        ${b.adopted ? `<span class="chip">adopted</span>` : ""}
        <span class="chip status-tag ${stateTagClass(b.state)}">${esc(b.state)}</span></h3>
      ${b.configured ? `<div class="backend-model-row">Model:
        <select class="backend-model-select" data-back-model="${esc(b.id)}" style="${SELECT_STYLE}"${dis}>${modelOptions(b, models)}</select></div>`
      : `<div class="backend-model-row">Model: <span class="backend-model-file" title="${esc(b.model_file)}">${esc(b.model_file)}</span></div>`}
      ${orphanNote}
      ${adoptedNote}
      ${b.configured && b.n_ctx !== undefined ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">Context ${b.n_ctx} · Batch ${b.n_batch ?? "—"}</div>` : ""}
      ${b.configured && b.gpu_ram_mib !== undefined ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">GPU RAM required: ${fmtGb(b.gpu_ram_mib / 1024)} GiB · System RAM required: ${fmtGb((b.sys_ram_mib ?? 0) / 1024)} GiB${b.kv_ram_mib != null ? ` · KV cache ${fmtGb(b.kv_ram_mib / 1024)} GiB at the configured context` : ""}${b.memory_basis === "estimate" ? ` · estimate` : ""}</div>` : ""}
            ${gpuHtml}
      ${statsHtml}
      <div style="display:grid;grid-template-columns:repeat(3,1fr);gap:10px;margin:12px 0">
        ${[["State", b.state], ["PID", b.pid !== null ? String(b.pid) : "\u2014"],
           ["Uptime", fmtUptime(b.uptime_secs)]]
          .map(([k, v]) => `<div class="kv"><span class="k">${k}</span><span class="v" style="font-size:13px">${esc(v)}</span></div>`).join("")}
      </div>
      ${flagsHtml}
      ${placeHtml}
      <div class="card-actions backend-actions">${actions}
        ${advancedMenu}</div>
      <div class="back-msg" style="margin-top:8px;font-size:12px">${msgHtml}</div>
      ${logsBlock}
      ${argv}
    </div>`;
}

export function renderBackends(): string {
  return `${topbar("Backends", `<button class="btn" id="backAdd">${icon("server")} Add backend</button> <button class="btn" id="backRefresh">${icon("restart")} Refresh</button>`)}
  <div class="view"><div id="backBanner"></div>
  <div id="backOverview"></div>
  <div style="display:flex;gap:10px;align-items:center;margin-bottom:10px;flex-wrap:wrap">
    <label style="font-size:12px;color:var(--text2)">Sort
      <select id="backSortSel" style="${SELECT_STYLE}">
        <option value="name">Name</option>
        <option value="port">Port</option>
        <option value="state">State</option>
        <option value="requests">Requests (1h)</option>
      </select></label>
    <label style="font-size:12px;color:var(--text2)">State
      <select id="backStateSel" style="${SELECT_STYLE}">
        <option value="all">All</option>
        <option value="running">Running</option>
        <option value="stopped">Stopped</option>
        <option value="failed">Failed</option>
        <option value="starting">Starting</option>
      </select></label>
    <input id="backFilter" type="text" placeholder="Filter by name, group, or model file" style="background:var(--panel2);border:1px solid var(--line);border-radius:6px;color:var(--text);padding:5px 8px;font-size:12px;min-width:200px">
  </div>
  <div id="backPlan"></div>
  <div id="backAddForm"></div>
  <div id="backBody">${loadingHtml("Loading backends\u2026")}</div></div>`;
}

export async function wireBackends(root: HTMLElement): Promise<() => void> {
  const openArgv = new Set<string>();
  /** Backend whose advanced-options menu is open (at most one). */
  const openMenus = new Set<string>();
  const argvCache = new Map<string, string[]>();
  const msgs = new Map<string, CardNote>();
  /** Recent log lines for failed backends, fetched lazily after paint. */
  const logTails = new Map<string, LogRow[]>();
  /** Ids whose log tail has been fetched (or is in flight) this mount. */
  const tailsWanted = new Set<string>();
  /** Backend ids with an action invoke in flight; their card buttons
   *  render disabled (the set outlives any single repaint). */
  const inFlight = new Set<string>();
  let lastRows: BackendRow[] = [];
  /** Measured GPU residency by backend id (telemetry attribution). */
  let residency = new Map<string, { gpu: number; name: string; mib: number | null }[]>();
  /** Backends list controls (sort / state filter / text filter),
   *  held outside the repainted DOM. */
  let backSort = "name";
  let backFilter = "";
  let backState = "all";
  /** Registered models for the per-card model dropdowns. */
  let lastModels: ModelRow[] = [];
  /** Gateway groups + port, for the add-backend form. */
  let lastGroups: GatewayGroup[] = [];
  let gatewayPort = 4000;
  /** Backend ids whose Remove button is armed for confirmation. */
  const confirmRemove = new Set<string>();
  /** Gateway activation plan (saved changes awaiting a gateway
   *  restart), painted into #backPlan above the add form. */
  let lastPlan: ActivationPlanData | null = null;
  const planUi = newPlanUi();
  const paintPlan = (): void => {
    const host = root.querySelector("#backPlan");
    if (host) host.innerHTML = activationPlanHtml(lastPlan, planUi);
  };
  let actionInFlight = false;
  let disposed = false;
  /** Load sequence: only the most recently started load may paint. */
  let loadSeq = 0;

  const paint = (): void => {
    const b = root.querySelector("#backBody");
    if (!b) return;
    const sorted = (list: BackendRow[]): BackendRow[] => {
      const c = [...list];
      if (backSort === "port") c.sort((a, z) => a.port - z.port);
      else if (backSort === "requests") c.sort((a, z) => (z.requests_1h ?? -1) - (a.requests_1h ?? -1));
      else if (backSort === "state") c.sort((a, z) => a.state.localeCompare(z.state) || a.id.localeCompare(z.id));
      else c.sort((a, z) => a.id.localeCompare(z.id));
      return c;
    };
    // Overview list: one line per backend (all rows, ignoring the
    // list filter), with column headings, divider lines, and a
    // status button that scrolls to the backend's card. The GPUs
    // column names only GPUs where the backend holds at least 1 GiB
    // measured — under that it holds only a CUDA context there.
    const ov = root.querySelector("#backOverview");
    if (ov) {
      ov.innerHTML = lastRows.length ? `<div class="ovlist ov-b">
        <div class="ov-head"><span></span><span>Backend</span><span>State</span><span>Group</span><span>Port</span><span>GPUs (measured)</span><span>Requests (1h)</span><span>Errors (1h)</span></div>
        ${sorted(lastRows).map((row) => {
          const res = residency.get(row.id) ?? [];
          // Unmeasured rows (mib null) are neither residency nor
          // context: the column says "not measured" rather than
          // claiming either.
          const resident = res.filter((r) => r.mib !== null && r.mib >= 1024);
          const gpuTxt = resident.length ? resident.map((r) => `GPU ${r.gpu}`).join(", ")
            : res.some((r) => r.mib !== null) ? "context only"
            : res.length ? "not measured" : "\u2014";
          const btn = row.state === "running" ? ovStatusBtn("ok", "check", `card-backend-${row.id}`, `${row.id}: running — go to card`)
            : row.state === "failed" ? ovStatusBtn("err", "err", `card-backend-${row.id}`, `${row.id}: failed — go to card`)
            : row.state === "starting" || row.state === "stopping" ? ovStatusBtn("warn", "clock", `card-backend-${row.id}`, `${row.id}: ${row.state} — go to card`)
            : ovStatusBtn("idle", "pause", `card-backend-${row.id}`, `${row.id}: ${row.state} — go to card`);
          return `<div class="ov-row">${btn}<span class="ov-name">${esc(row.id)}</span><span class="ov-dim">${esc(row.state)}</span><span class="ov-dim">${esc(row.group ?? "ungrouped")}</span><span class="ov-dim">${row.port}</span><span class="ov-dim">${esc(gpuTxt)}</span><span class="ov-dim">${row.requests_1h !== undefined ? row.requests_1h.toLocaleString() : "\u2014"}</span><span class="ov-dim">${row.errors_1h !== undefined ? String(row.errors_1h) : "\u2014"}</span></div>`;
        }).join("")}</div>` : "";
    }
    const f = backFilter.trim().toLowerCase();
    const viewRows = lastRows.filter((row) =>
      (backState === "all" || row.state === backState) &&
      (!f || row.id.toLowerCase().includes(f) || (row.group ?? "").toLowerCase().includes(f) || row.model_file.toLowerCase().includes(f)));
    // Cards are grouped by gateway group (Pete's request, 2026-10-15):
    // a section per group in the order groups first appear across
    // the rows (backend config order), ungrouped rows and orphans
    // last under "Ungrouped".
    const sections: { name: string | null; rows: BackendRow[] }[] = [];
    const byKey = new Map<string, { name: string | null; rows: BackendRow[] }>();
    for (const row of viewRows) {
      const key = row.group ?? "";
      let sec = byKey.get(key);
      if (!sec) {
        sec = { name: row.group ?? null, rows: [] };
        byKey.set(key, sec);
        sections.push(sec);
      }
      sec.rows.push(row);
    }
    const ordered = [
      ...sections.filter((s) => s.name !== null),
      ...sections.filter((s) => s.name === null),
    ];
    b.innerHTML = ordered.map((sec) => `
      <div class="group-box">
      <div class="sec-title" style="margin:0 0 10px">${sec.name !== null ? `Group: ${esc(sec.name)}` : "Ungrouped"} <span style="color:var(--text3);font-weight:400">— ${sec.rows.length} backend${sec.rows.length === 1 ? "" : "s"}</span></div>
      <div class="grid2">${sorted(sec.rows).map((row) => {
        const mdl = flagsFor === row.id ? lastModels.find((m) => sameModelFile(row.model_file, m.gguf_path)) : undefined;
        const flagsArg: FlagsDraft | null = flagsFor === row.id
          ? { ctx: flagsCtx, batch: flagsBatch, ctxMax: mdl?.ctx_max ? mdl.ctx_max : null, modelId: mdl?.id ?? null, offloadLayers: flagsOffloadLayers }
          : null;
        const pmdl = placeFor === row.id ? lastModels.find((m) => sameModelFile(row.model_file, m.gguf_path)) : undefined;
        const placeArg: PlaceCardState | null = placeFor === row.id
          ? {
            gpus: lastGpuList.map((g) => ({ index: Number(g.id), name: g.name })),
            excluded: placeExcluded,
            modelId: pmdl?.id ?? null,
            projection: placeProjection && placeProjection.backendId === row.id ? placeProjection.data : null,
            busy: placeBusy,
          }
          : null;
        return backendCard(row, openArgv, openMenus, argvCache, msgs, logTails, inFlight, lastModels, confirmRemove, residency, new Map(lastGpuList.map((g) => [Number(g.id), g.name])), flagsArg, placeArg);
      }).join("")}</div>
      </div>`).join("");
    if (!viewRows.length && lastRows.length) b.innerHTML = emptyHtml("No backends match the current filter.");
  };

  const backSortSel = root.querySelector("#backSortSel") as HTMLSelectElement | null;
  const backStateSel = root.querySelector("#backStateSel") as HTMLSelectElement | null;
  const backFilterInput = root.querySelector("#backFilter") as HTMLInputElement | null;
  backSortSel?.addEventListener("change", () => { backSort = backSortSel.value; paint(); });
  backStateSel?.addEventListener("change", () => { backState = backStateSel.value; paint(); });
  backFilterInput?.addEventListener("input", () => { backFilter = backFilterInput.value; paint(); });

  /** Lazily fetch the last log lines for failed backends and repaint.
   *  get_logs takes the bare process id (the same scope the Logs view
   *  sends); records arrive newest-first. Stderr lines are preferred —
   *  that is where llama-server reports load failures. */
  const fetchLogTails = async (): Promise<void> => {
    const rows = lastRows;
    let fetched = false;
    for (const row of rows) {
      if (row.state !== "failed" || tailsWanted.has(row.id)) continue;
      tailsWanted.add(row.id);
      const r = await getLogs(row.id, { limit: 50 });
      if (disposed) return;
      if (r.error) { tailsWanted.delete(row.id); continue; }
      const stderr = r.data.lines.filter((x) => x.stream === "stderr");
      const source = stderr.length ? stderr : r.data.lines;
      logTails.set(row.id, source.slice(0, 3).reverse());
      fetched = true;
    }
    // Repaint only if these are still the current rows (a reload or an
    // error paint may have superseded them while fetches were in flight).
    if (fetched && !disposed && lastRows === rows) paint();
  };

  const load = async (): Promise<void> => {
    const seq = ++loadSeq;
    const [r, mr, gr, pr, tr] = await Promise.all([getBackends(), getModels(), getGatewayConfig(), getActivationPlan(), getGpuTelemetry()]);
    if (disposed || seq !== loadSeq) return; // a newer load supersedes this one
    if (!pr.error && pr.data) { lastPlan = pr.data; paintPlan(); }
    if (!tr.error && tr.data) {
      lastGpuList = tr.data.gpus;
      const m = new Map<string, { gpu: number; name: string; mib: number | null }[]>();
      for (const g of tr.data.gpus) {
        for (const p of g.processes ?? []) {
          if (p.backend === "\u2014") continue;
          const arr = m.get(p.backend) ?? [];
          arr.push({ gpu: Number(g.id), name: g.name, mib: p.vram_mib });
          m.set(p.backend, arr);
        }
      }
      residency = m;
    }
    if (!mr.error && mr.data) lastModels = mr.data;
    if (!gr.error && gr.data) {
      lastGroups = gr.data.groups;
      if (gr.data.port) gatewayPort = gr.data.port;
    }
    setBanner(root, "backBanner", bannerHtml(r));
    const b = root.querySelector("#backBody");
    if (!b) return;
    if (r.error) { lastRows = []; b.innerHTML = emptyHtml(`Backends unavailable: ${r.error}`); return; }
    if (!r.data.length) {
      lastRows = [];
      b.innerHTML = emptyHtml(r.live
        ? "No backends are configured. Backends are defined in the app configuration."
        : "No backend data in sample mode.");
      return;
    }
    const prevStates = new Map(lastRows.map((row) => [row.id, row.state]));
    lastRows = r.data;
    for (const row of lastRows) {
      // A note belongs to the state it was produced in; when the
      // backend's state changes, the stale note goes away.
      const prev = prevStates.get(row.id);
      if (prev !== undefined && prev !== row.state) { msgs.delete(row.id); confirmRemove.delete(row.id); }
      // A backend that is no longer failed gets a fresh tail fetch if it
      // fails again.
      if (row.state !== "failed") { logTails.delete(row.id); tailsWanted.delete(row.id); }
    }
    // While the offload slider is focused (mid-drag), a repaint
    // would recreate the range input and break the drag; the
    // draft is synced to wire state, so skipping one repaint
    // loses nothing (same rule as the model rename input).
    const sliderFocused = flagsFor !== null
      && (document.activeElement as HTMLElement | null)?.hasAttribute?.("data-offload-slider");
    if (!sliderFocused) paint();
    void fetchLogTails();
  };

  const refreshBtn = root.querySelector("#backRefresh");
  const onRefresh = (): void => { void load(); };
  refreshBtn?.addEventListener("click", onRefresh);

  /* ---------------- Flags editor ----------------
   * Rendered INSIDE the backend's own card (backendCard, from the
   * draft state below) — the previous placement above the list read
   * as if nothing had happened when Edit flags was clicked. The
   * draft values live outside the repainted DOM, so the poll repaint
   * cannot clobber an open form: paint() re-renders the form from
   * this state, and field events sync back into it. */
  let flagsFor: string | null = null;
  let flagsCtx = 32768;
  let flagsBatch = 512;
  /** Offload slider draft position for the open flags form (MoE
   *  layers on system RAM); null when the backend's model is not
   *  offload-capable. */
  let flagsOffloadLayers: number | null = null;
  /** True while a slider commit (backend_set_offload) is in
   *  flight; the slider ignores further commits until it lands. */
  let offloadCommitting = false;
  const closeFlagsForm = (): void => {
    flagsFor = null;
    paint();
  };
  const openFlagsForm = (row: BackendRow): void => {
    if (flagsFor === row.id) { closeFlagsForm(); return; }
    flagsFor = row.id;
    flagsCtx = row.n_ctx ?? 32768;
    flagsBatch = row.n_batch ?? 512;
    flagsOffloadLayers = row.offload ? row.offload.layers : null;
    paint();
  };
  /** Keep the draft in step with the in-card form fields. Preset
   *  selects repaint (to show or hide the Custom numeric input);
   *  custom inputs only update the draft. Registered for both
   *  "change" and "input" so typing is captured before any repaint. */
  const syncFlagsDraft = (ev: Event): void => {
    const t = ev.target as HTMLElement;
    const field = t.getAttribute("data-flags-field");
    if (!field) return;
    const val = (t as HTMLInputElement).value;
    if (field === "ctx-custom") flagsCtx = Number(val);
    else if (field === "batch-custom") flagsBatch = Number(val);
    else if (ev.type === "change" && field === "ctx-preset") {
      if (val !== "custom") flagsCtx = Number(val);
      paint();
    } else if (ev.type === "change" && field === "batch-preset") {
      if (val !== "custom") flagsBatch = Number(val);
      paint();
    }
  };
  root.addEventListener("change", syncFlagsDraft);
  root.addEventListener("input", syncFlagsDraft);

  /* ---------------- Offload slider ----------------
   * Part of the flags form when the backend's model is
   * offload-capable. "input" events move the draft and update the
   * readout in place — no repaint, so the drag is never disturbed;
   * the "change" event (slider release) commits through
   * backend_set_offload, which rewrites only the offload tokens
   * in the backend's raw flags. A rejected commit reverts the
   * draft to the saved position and keeps the form open with the
   * error on the card. */
  const syncOffloadDraft = (ev: Event): void => {
    const t = ev.target as HTMLInputElement;
    if (!t.hasAttribute("data-offload-slider")) return;
    const id = t.getAttribute("data-offload-backend") ?? "";
    if (id !== flagsFor) return;
    const row = lastRows.find((r) => r.id === id);
    if (!row?.offload) return;
    const n = Number(t.value);
    flagsOffloadLayers = n;
    const card = t.closest(".backend-card");
    if (!card) return;
    const o = row.offload;
    const figs = offloadFigures(o, n);
    const setText = (sel: string, txt: string): void => {
      const el = card.querySelector(sel);
      if (el) el.textContent = txt;
    };
    setText("[data-offload-layers-label]", `${n} of ${o.moe_layers} MoE layers`);
    setText("[data-offload-token]", offloadTokenText(o, n));
    setText("[data-offload-gpu]", fmtGb(figs.gpuMib / 1024));
    setText("[data-offload-sys]", fmtGb(figs.sysMib / 1024));
  };
  const commitOffload = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLInputElement;
    if (!t.hasAttribute("data-offload-slider")) return;
    const id = t.getAttribute("data-offload-backend") ?? "";
    const row = lastRows.find((r) => r.id === id);
    if (!row?.offload || offloadCommitting) return;
    const n = Number(t.value);
    flagsOffloadLayers = n;
    if (n === row.offload.layers) return; // released where it started
    offloadCommitting = true;
    try {
      const r = await setBackendOffload(id, n);
      if (disposed) return;
      if (r.error) {
        msgs.set(id, { text: `Offload not saved: ${r.error}`, err: true });
        flagsOffloadLayers = row.offload.layers;
        paint();
      } else {
        msgs.set(id, { text: `Offload saved (${offloadTokenText(row.offload, n)}) — applies on next start.`, err: false });
        await load();
      }
    } finally {
      offloadCommitting = false;
      if (!disposed) paint();
    }
  };
  root.addEventListener("input", syncOffloadDraft);
  root.addEventListener("change", commitOffload);

  /* ---------------- Placement form ----------------
   * Rendered INSIDE the backend's own card (backendCard), like the
   * flags editor; the GPUs page shows placement read-only. State
   * lives outside the repainted DOM: which backend's form is open,
   * the unchecked GPU indices, and the last projection. Checkbox
   * changes sync into the state and invalidate the projection (it
   * no longer matches the selection). */
  let placeFor: string | null = null;
  const placeExcluded = new Set<number>();
  let placeProjection: { backendId: string; modelId: string; data: PlacementProjectionData } | null = null;
  let placeBusy = false;
  /** Latest telemetry GPU list (checkbox source), refreshed by load(). */
  let lastGpuList: GpuInfo[] = [];
  const closePlaceForm = (): void => {
    placeFor = null;
    placeProjection = null;
    placeExcluded.clear();
    paint();
  };
  const openPlaceForm = (row: BackendRow): void => {
    if (placeFor === row.id) { closePlaceForm(); return; }
    placeFor = row.id;
    placeProjection = null;
    placeExcluded.clear();
    // Default the selection to the backend's CURRENT placement, not
    // every GPU: the configured tensor split's positive entries when
    // a split is configured; otherwise the GPUs the loaded process
    // measurably holds weights on (>= 1 GiB — under that is a bare
    // CUDA context); otherwise, for an unsplit backend that is not
    // loaded, the first GPU, which is where an unsplit launch loads.
    const allIdx = lastGpuList.map((g) => Number(g.id));
    const cfgIdx = (row.tensor_split ?? [])
      .map((f, p) => (f > 0 && p < allIdx.length ? allIdx[p] : -1))
      .filter((i) => i >= 0);
    let current = cfgIdx;
    if (!current.length) {
      // Unmeasured rows (mib null) never select a GPU: unknown usage
      // is not residency. The default then falls back to the first
      // GPU, as for an unloaded backend.
      const resIdx = (residency.get(row.id) ?? [])
        .filter((r) => r.mib !== null && r.mib >= 1024)
        .map((r) => r.gpu)
        .filter((i) => allIdx.includes(i));
      current = resIdx.length ? resIdx : allIdx.slice(0, 1);
    }
    for (const ix of allIdx) if (!current.includes(ix)) placeExcluded.add(ix);
    paint();
  };
  const syncPlaceDraft = (ev: Event): void => {
    const t = ev.target as HTMLElement;
    if (!t.hasAttribute("data-place-gpu")) return;
    if (t.getAttribute("data-place-backend") !== placeFor) return;
    const ix = Number(t.getAttribute("data-place-gpu"));
    if ((t as HTMLInputElement).checked) placeExcluded.delete(ix);
    else placeExcluded.add(ix);
    placeProjection = null;
    paint();
  };
  root.addEventListener("change", syncPlaceDraft);

  /* ---------------- Add backend form ----------------
   * Lives in #backAddForm, outside the repainted #backBody, so the
   * 5-second poll never clobbers typed input. */
  const addHost = root.querySelector("#backAddForm");
  const closeAddForm = (): void => { if (addHost) addHost.innerHTML = ""; };
  const nextFreePort = (): number => {
    const used = new Set<number>(lastRows.map((r) => r.port));
    used.add(gatewayPort);
    used.add(8090); // the Windows ML backend's conventional port
    let p = 8081;
    while (used.has(p)) p += 1;
    return p;
  };
  const openAddForm = (): void => {
    if (!addHost) return;
    if (addHost.innerHTML) { closeAddForm(); return; }
    const modelOpts = lastModels.map((m) => `<option value="${esc(m.id)}">${esc(modelShortName(m))}</option>`).join("");
    const groupOpts = lastGroups.map((g) => `<option value="${esc(g.name)}">${esc(g.name)}</option>`).join("")
      + `<option value="__new__">New group\u2026</option>`;
    addHost.innerHTML = `<div class="card"><h3>${icon("server")} Add a backend</h3>
    <div style="display:grid;grid-template-columns:1fr 1fr;gap:10px;margin-top:10px">
      <label style="font-size:12px;color:var(--text2)">Name (lowercase, digits, hyphens)<br>
        <input id="addBackName" type="text" placeholder="nemotron" style="${SELECT_STYLE};width:100%"></label>
      <label style="font-size:12px;color:var(--text2)">Model<br>
        <select id="addBackModel" style="${SELECT_STYLE};width:100%">${modelOpts}</select></label>
      <label style="font-size:12px;color:var(--text2)">Port<br>
        <input id="addBackPort" type="number" value="${nextFreePort()}" style="${SELECT_STYLE};width:100%"></label>
      <label style="font-size:12px;color:var(--text2)">Gateway group<br>
        <select id="addBackGroup" style="${SELECT_STYLE};width:100%">${groupOpts}</select></label>
    </div>
    <label id="addBackNewGroupWrap" style="font-size:12px;color:var(--text2);display:none;margin-top:10px">New group name (defaults to the backend name)<br>
      <input id="addBackNewGroup" type="text" placeholder="nemotron" style="${SELECT_STYLE};width:100%"></label>
    <label style="font-size:12px;color:var(--text2);display:block;margin-top:10px">
      <input id="addBackEnabled" type="checkbox" checked> Enabled (the gateway routes enabled backends; it can be started right away)</label>
    <div class="card-actions"><button class="btn" id="addBackSubmit"${lastModels.length ? "" : " disabled"}>Add backend</button>
      <button class="btn" id="addBackCancel">Cancel</button></div>
    <div style="font-size:12px;color:var(--text2);margin-top:6px">${lastModels.length
      ? "The backend is created with the standard defaults (32K context; a tensor split is assigned from live GPU telemetry when available). Joining an existing group makes the gateway share that group's traffic across its members; a new group gives the backend its own model name."
      : "No models are registered yet \u2014 add one on the Models page first."}</div>
    <div id="addBackMsg" style="font-size:12px;margin-top:6px"></div>
  </div>`;
    const groupSelEl = addHost.querySelector("#addBackGroup") as HTMLSelectElement | null;
    const newWrap = addHost.querySelector("#addBackNewGroupWrap") as HTMLElement | null;
    groupSelEl?.addEventListener("change", () => {
      if (newWrap) newWrap.style.display = groupSelEl.value === "__new__" ? "block" : "none";
    });
    addHost.querySelector("#addBackCancel")?.addEventListener("click", closeAddForm);
    const submit = addHost.querySelector("#addBackSubmit") as HTMLButtonElement | null;
    submit?.addEventListener("click", async () => {
      const val = (sel: string): string => (addHost.querySelector(sel) as HTMLInputElement | null)?.value.trim() ?? "";
      const name = val("#addBackName").toLowerCase();
      const modelId = (addHost.querySelector("#addBackModel") as HTMLSelectElement | null)?.value ?? "";
      const port = Number(val("#addBackPort"));
      const groupSel = (addHost.querySelector("#addBackGroup") as HTMLSelectElement | null)?.value ?? "";
      const createGroup = groupSel === "__new__";
      const group = createGroup ? (val("#addBackNewGroup").toLowerCase() || name) : groupSel;
      const enabled = (addHost.querySelector("#addBackEnabled") as HTMLInputElement | null)?.checked ?? true;
      const msg = addHost.querySelector("#addBackMsg") as HTMLElement | null;
      if (!name || !modelId || !group || !Number.isInteger(port) || port <= 0) {
        if (msg) msg.innerHTML = `<span style="color:var(--err)">Name, model, port, and group are required.</span>`;
        return;
      }
      submit.disabled = true;
      const r = await addBackend({ id: name, modelId, port, group, createGroup, enabled });
      if (disposed) return;
      if (r.error) {
        if (msg) msg.innerHTML = `<span style="color:var(--err)">Add failed: ${esc(r.error)}</span>`;
        submit.disabled = false;
        return;
      }
      addHost.innerHTML = `<div class="card"><h3>${icon("check")} Backend added</h3>
        <div style="font-size:12px;color:var(--text2);margin-top:6px">Backend <b>${esc(name)}</b> was created in group <b>${esc(group)}</b>. The change is recorded in the activation plan at the top of this page — the gateway routes the new backend after the plan is executed.</div>
        <div class="card-actions"><button class="btn" id="addBackDone">Done</button></div></div>`;
      addHost.querySelector("#addBackDone")?.addEventListener("click", closeAddForm);
      await load();
    });
  };
  const addBtn = root.querySelector("#backAdd");
  addBtn?.addEventListener("click", openAddForm);

  const onClick = async (ev: Event): Promise<void> => {
    const target = ev.target as HTMLElement;
    const menuToggle = target.closest("[data-back-menu]") as HTMLElement | null;
    if (menuToggle) {
      const menuId = menuToggle.getAttribute("data-back-menu") ?? "";
      const wasOpen = openMenus.has(menuId);
      openMenus.clear();
      if (!wasOpen && menuId) openMenus.add(menuId);
      paint();
      return;
    }
    const menuItem = target.closest("[data-back-menu-item]") as HTMLElement | null;
    if (menuItem) openMenus.delete(menuItem.getAttribute("data-back-id") ?? "");
    const hadOpenMenu = openMenus.size > 0;
    if (!menuItem && !target.closest("[data-back-menu-wrap]") && hadOpenMenu) {
      openMenus.clear();
    }
    const pt = target.closest("[data-plan-act]") as HTMLElement | null;
    if (pt) {
      if (hadOpenMenu) paint();
      await handlePlanAction(pt.getAttribute("data-plan-act") ?? "", planUi, paintPlan, load, lastPlan);
      return;
    }
    const t = target.closest("[data-back-act]") as HTMLElement | null;
    if (!t) {
      if (hadOpenMenu && !target.closest("[data-back-menu-wrap]")) paint();
      return;
    }
    const id = t.getAttribute("data-back-id") ?? "";
    const act = t.getAttribute("data-back-act") ?? "";

    if (act === "flags") {
      const row = lastRows.find((r) => r.id === id);
      if (row) openFlagsForm(row);
      return;
    }

    if (act === "flags-cancel") {
      closeFlagsForm();
      return;
    }

    if (act === "placement") {
      const row = lastRows.find((r) => r.id === id);
      if (row) openPlaceForm(row);
      return;
    }

    if (act === "place-cancel") {
      closePlaceForm();
      return;
    }

    if (act === "place-propose") {
      if (placeBusy) return;
      const row = lastRows.find((r) => r.id === id);
      const model = lastModels.find((m) => row !== undefined && sameModelFile(row.model_file, m.gguf_path));
      if (!row || !model) return;
      if (!lastGpuList.length) {
        msgs.set(id, { text: "No GPU telemetry yet \u2014 a split cannot be proposed.", err: true });
        paint();
        return;
      }
      // Compose the split: preserve the backend's configured split
      // when it covers exactly the selected set of GPUs (the norm
      // right after loading); otherwise an even split across the
      // selection. Positions follow the telemetry GPU order, which
      // is nvidia-smi index order — the same order tensor_split
      // uses.
      const usedPos = lastGpuList
        .map((g, p) => (!placeExcluded.has(Number(g.id)) ? p : -1))
        .filter((p) => p >= 0);
      if (!usedPos.length) {
        msgs.set(id, { text: "Select at least one GPU for the split.", err: true });
        paint();
        return;
      }
      const cfgSplit = row.tensor_split ?? [];
      const nz = cfgSplit.map((f, i) => (f > 0 ? i : -1)).filter((i) => i >= 0);
      const sameSet = cfgSplit.length === lastGpuList.length
        && nz.length === usedPos.length
        && nz.every((i) => usedPos.includes(i));
      const split = sameSet
        ? [...cfgSplit]
        : lastGpuList.map((_, p) => (usedPos.includes(p) ? 1 / usedPos.length : 0));
      placeBusy = true;
      paint();
      try {
        const r = await placementPropose(model.id, split, id);
        if (disposed) return;
        if (r.error || !r.data) {
          msgs.set(id, { text: `Propose failed: ${r.error ?? "the command returned no projection"}`, err: true });
          placeProjection = null;
        } else {
          placeProjection = { backendId: id, modelId: model.id, data: r.data };
        }
      } finally {
        placeBusy = false;
        if (!disposed) paint();
      }
      return;
    }

    if (act === "place-apply") {
      if (placeBusy || !placeProjection || placeProjection.backendId !== id) return;
      const proj = placeProjection;
      placeBusy = true;
      paint();
      try {
        const r = await placementApply(proj.modelId, proj.data.split, id);
        if (disposed) return;
        if (r.error) {
          // Keep the form and projection open with the error on the
          // card — closing would read as if the apply had happened.
          msgs.set(id, { text: `Split not saved: ${r.error}`, err: true });
          paint();
        } else {
          msgs.set(id, { text: `Split saved to ${id}. Restart the backend to use it.`, err: false });
          placeFor = null;
          placeProjection = null;
          placeExcluded.clear();
          await load();
        }
      } finally {
        placeBusy = false;
      }
      return;
    }

    if (act === "flags-save") {
      const r = await updateBackendFlags(id, flagsCtx, flagsBatch);
      if (disposed) return;
      if (r.error) {
        // Keep the form open with the error on the card — closing on
        // a rejected value would read as if the save had happened.
        msgs.set(id, { text: `Flags not saved: ${r.error}`, err: true });
        paint();
      } else {
        msgs.set(id, { text: `Flags saved (context ${flagsCtx.toLocaleString()}, batch ${flagsBatch}) — applies on next start.`, err: false });
        flagsFor = null;
        await load();
      }
      return;
    }

    if (act === "argv") {
      if (openArgv.has(id)) { openArgv.delete(id); paint(); return; }
      if (!argvCache.has(id)) {
        // Only successful fetches are cached (per mount); a failure is
        // shown in the message area and the next toggle retries.
        actionInFlight = true;
        inFlight.add(id);
        paint();
        try {
          const r = await getBackendLaunchCommand(id);
          if (disposed) return;
          if (r.error) msgs.set(id, { text: `Launch command failed: ${r.error}`, err: true });
          else argvCache.set(id, r.data.argv);
        } finally {
          actionInFlight = false;
          inFlight.delete(id);
        }
        if (!argvCache.has(id)) { paint(); return; } // fetch failed: show the note, stay closed
      }
      openArgv.add(id);
      paint();
      return;
    }

    inFlight.add(id);
    actionInFlight = true;
    paint();
    try {
      if (act === "enable" || act === "disable") {
        const r = await applyBackendConfig(id, { enabled: act === "enable" }, "OnNextRestart");
        if (r.error) msgs.set(id, { text: `${act} failed: ${r.error}`, err: true });
        else msgs.delete(id);
        await load();
        return;
      }
      if (act === "remove") {
        if (!confirmRemove.has(id)) {
          confirmRemove.add(id);
          msgs.set(id, { text: "Click the highlighted remove icon again to delete this backend and its group memberships.", err: false });
          paint();
          return;
        }
        confirmRemove.delete(id);
        const r = await deleteBackend(id);
        if (r.error) msgs.set(id, { text: `Remove failed: ${r.error}`, err: true });
        await load();
        return;
      }
      if (act === "test") {
        const r = await testBackend(id, "Reply with the single word: ready");
        if (disposed) return;
        if (r.error) {
          msgs.set(id, { text: `Test failed: ${r.error}`, err: true });
        } else if (r.data.content) {
          const reply = r.data.content.trim();
          const excerpt = reply.length > 120 ? `${reply.slice(0, 120)}…` : reply;
          msgs.set(id, { text: `Test reply (${r.data.latency_ms} ms): ${excerpt}`, err: false });
        } else {
          msgs.set(id, { text: `Test failed: HTTP ${r.data.status} — no reply content.`, err: true });
        }
        paint();
        return;
      }
      const fn = act === "start" ? startBackend : act === "stop" ? stopBackend : act === "restart" ? restartBackend : null;
      if (!fn) return;
      const r = await fn(id);
      if (r.error) msgs.set(id, { text: `${act} failed: ${r.error}`, err: true });
      else msgs.delete(id);
      await load();
    } finally {
      actionInFlight = false;
      inFlight.delete(id);
      if (!disposed) paint();
    }
  };
  /** Model dropdown change: assign the chosen model to the backend
   *  through model_assign, carrying the backend's current tensor
   *  split so the assignment changes the model only. The card note
   *  tells the operator a restart applies it to a running backend. */
  const onModelChange = async (ev: Event): Promise<void> => {
    const sel = (ev.target as HTMLElement).closest("[data-back-model]") as HTMLSelectElement | null;
    if (!sel || sel.disabled) return;
    const backendId = sel.getAttribute("data-back-model") ?? "";
    const modelId = sel.value;
    if (!backendId || !modelId) return;
    const row = lastRows.find((b) => b.id === backendId);
    const model = lastModels.find((m) => m.id === modelId);
    if (!row || !model || sameModelFile(row.model_file, model.gguf_path)) return;
    inFlight.add(backendId);
    paint();
    try {
      const cfgR = await getBackendConfig(backendId);
      const flags = cfgR.data?.flags as { tensor_split?: unknown } | undefined;
      const split = Array.isArray(flags?.tensor_split)
        && flags.tensor_split.every((x) => typeof x === "number" && Number.isFinite(x))
        ? (flags.tensor_split as number[]) : [];
      const r = await modelAssign(backendId, modelId, split);
      if (disposed) return;
      if (r.error) {
        msgs.set(backendId, { text: `Model change failed: ${r.error}`, err: true });
      } else {
        msgs.set(backendId, {
          text: row.state === "running" || row.state === "starting"
            ? `Model set to ${modelId}. Restart the backend to use it.`
            : `Model set to ${modelId}.`,
          err: false,
        });
      }
    } finally {
      inFlight.delete(backendId);
      if (!disposed) await load();
    }
  };
  const onKeyDown = (ev: KeyboardEvent): void => {
    if (ev.key === "Escape" && openMenus.size) {
      openMenus.clear();
      paint();
    }
  };
  root.addEventListener("click", onClick);
  root.addEventListener("click", onOvGoto);
  root.addEventListener("change", onModelChange);
  root.addEventListener("keydown", onKeyDown);

  await load();

  // Keep process state fresh (a backend can die outside this view); skip
  // a tick while an action request is in flight.
  const timer = window.setInterval(() => {
    if (!actionInFlight && !disposed) void load();
  }, 5000);

  return () => {
    disposed = true;
    window.clearInterval(timer);
    root.removeEventListener("click", onClick);
    root.removeEventListener("click", onOvGoto);
    root.removeEventListener("change", onModelChange);
    root.removeEventListener("keydown", onKeyDown);
    root.removeEventListener("change", syncFlagsDraft);
    root.removeEventListener("input", syncFlagsDraft);
    root.removeEventListener("input", syncOffloadDraft);
    root.removeEventListener("change", commitOffload);
    root.removeEventListener("change", syncPlaceDraft);
    refreshBtn?.removeEventListener("click", onRefresh);
  };
}

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
    return `<span class="chip badge-warn">${icon("warn")} hash not pinned</span>`;
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
    return `<div class="ov-row">${btn}<span class="ov-name" title="${esc(m.id)}">${esc(modelShortName(m))}</span><span class="ov-dim">${esc(m.quant)}</span><span class="ov-dim">${esc(size)}</span><span class="ov-dim">${esc(stateTxt)}</span><span class="ov-dim">${st ? String(st.backends) : "0"}</span><span class="ov-dim">${st?.req !== undefined ? st.req.toLocaleString() : "\u2014"}</span></div>`;
  }).join("");
  return `<div class="ovlist ov-m"><div class="ov-head"><span></span><span>Model</span><span>Quant</span><span>Size</span><span>State</span><span>Serving backends</span><span>Requests (1h)</span></div>${body}</div>`;
}

function modelCard(m: ModelRow, downloading: Set<string>, notes: Map<string, ModelNote>, stats?: { backends: number; req?: number; err?: number; toks?: number }, rename?: { draft: string } | null): string {
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
  let actions: string;
  if (inFlight && m.downloading && m.paused) {
    actions = `<span style="display:inline-flex;align-items:center;gap:6px;font-size:12px;color:var(--text2)">${icon("pause")} Paused${m.partial_bytes !== null ? ` \u00b7 ${esc(fmtBytes(m.partial_bytes))}` : ""}</span>
      <button class="btn" data-model-act="resume" data-model-id="${esc(m.id)}">${icon("play")} Resume</button>
      <button class="btn" data-model-act="cancel" data-model-id="${esc(m.id)}">Cancel</button>`;
  } else if (inFlight) {
    actions = `<span style="display:inline-flex;align-items:center;gap:6px;font-size:12px;color:var(--text2)">${icon("clock")} Downloading\u2026 \u00b7 ${m.partial_bytes !== null ? esc(fmtBytes(m.partial_bytes)) : "started"}</span>
      <button class="btn" data-model-act="pause" data-model-id="${esc(m.id)}"${spawnDis}>${icon("pause")} Pause</button>
      <button class="btn" data-model-act="cancel" data-model-id="${esc(m.id)}"${spawnDis}>Cancel</button>`;
  } else if (m.verifying) {
    // The transfer finished; the backend watcher is hashing the file
    // against the pinned SHA-256. Not a plain complete state yet.
    actions = `<span style="display:inline-flex;align-items:center;gap:6px;font-size:12px;color:var(--text2)">${icon("shield")} Verifying checksum\u2026 \u2014 the download finished and the file is being checked against the pinned SHA-256.</span>`;
  } else if (partial > 0) {
    actions = `<span style="display:inline-flex;align-items:center;gap:6px;font-size:12px;color:var(--text2)">${icon("warn")} Partial download · ${esc(fmtBytes(partial))}${m.catalog_size_bytes !== null ? ` of ~${esc(fmtBytes(m.catalog_size_bytes))} total (estimate)` : ""} — the file is incomplete and cannot be used yet</span>
      <button class="btn" data-model-act="download" data-model-id="${esc(m.id)}">${icon("play")} Resume</button>
      <button class="btn" data-model-act="delete" data-model-id="${esc(m.id)}">${icon("err")} Delete</button>`;
  } else if (!m.on_disk) {
    actions = `<button class="btn" data-model-act="download" data-model-id="${esc(m.id)}">${icon("download")} Download</button>
      <span style="font-size:12px;color:var(--text2)">not downloaded${m.catalog_size_bytes !== null ? ` · ~${esc(fmtBytes(m.catalog_size_bytes))} (estimate)` : ""}</span>`;
  } else {
    // Verify is only offered with a pinned hash — against an unpinned
    // placeholder it could never succeed (the badge explains the state).
    actions = `${hashPinned(m) ? `<button class="btn" data-model-act="verify" data-model-id="${esc(m.id)}">${icon("shield")} Verify checksum</button>` : ""}
      <button class="btn" data-model-act="delete" data-model-id="${esc(m.id)}">${icon("err")} Delete</button>`;
  }
  // Enable/disable is always offered: it controls whether the setup
  // wizard's download step includes this model.
  actions += ` <button class="btn" data-model-act="toggle-enabled" data-model-id="${esc(m.id)}">${m.enabled ? "Disable" : "Enable"}</button>`;
  // Rename is always offered: the short name is the operator's own
  // label for the model, independent of its download state.
  actions += ` <button class="btn" data-model-act="rename" data-model-id="${esc(m.id)}">${icon("wrench")} Rename</button>`;
  const renameHtml = rename ? `<div class="backend-form-panel flags-panel">
      <div style="font-size:12px;font-weight:600">Rename short name</div>
      <div style="display:flex;gap:6px;margin-top:6px;min-width:0">
        <input data-model-rename-input="${esc(m.id)}" type="text" value="${esc(rename.draft)}" maxlength="64" style="${SELECT_STYLE};flex:1;min-width:0">
      </div>
      <div class="card-actions"><button class="btn" data-model-act="rename-save" data-model-id="${esc(m.id)}">Save name</button>
        <button class="btn" data-model-act="rename-cancel" data-model-id="${esc(m.id)}">Cancel</button></div>
    </div>` : "";
  const sizeText = m.on_disk
    ? esc(fmtBytes(m.size_bytes))
    : partial > 0 ? `${esc(fmtBytes(partial))} (partial)` : "not downloaded";
  return `<div class="card" id="card-model-${esc(m.id)}">
    <h3>${icon("box")} ${esc(modelShortName(m))} <span class="chip">${esc(m.quant)}</span>${m.enabled ? "" : ` <span class="chip badge-warn">disabled</span>`}
      <span class="spacer" style="flex:1"></span>
      ${modelShaBadge(m)}</h3>
    ${modelShortName(m) !== m.id ? `<div style="font-size:11px;color:var(--text3);font-family:var(--mono);margin-top:2px">${esc(m.id)}</div>` : ""}
    <div style="display:grid;grid-template-columns:1fr 1fr 1fr;gap:12px;margin:12px 0;align-items:center">
      <div class="kv"><span class="k">File</span><span class="v" style="font-size:12px">${esc(m.gguf_path)}</span></div>
      <div class="kv"><span class="k">Size</span><span class="v" style="font-size:12px">${sizeText}</span></div>
      ${m.params_b > 0 ? `<div class="kv"><span class="k">Parameters</span><span class="v" style="font-size:12px">${m.params_b}B</span></div>` : ""}
    </div>
    <div style="font-size:11px;color:var(--text3);font-family:var(--mono)">sha256: ${esc(m.sha256 ? `${m.sha256.slice(0, 16)}…` : "—")}</div>
    ${stats && stats.backends > 0 ? `<div style="font-size:12px;color:var(--text2);margin-bottom:8px">Gateway (1h, ${stats.backends} backend${stats.backends === 1 ? "" : "s"}): ${stats.req !== undefined ? `${stats.req.toLocaleString()} requests · ${stats.err ?? 0} errors · ${trim1(stats.toks ?? 0)} tok/s` : "no metrics window yet"}</div>` : ""}
    <div class="card-actions">${actions}</div>
    ${renameHtml}
    ${dlErrHtml}
    ${noteHtml}
  </div>`;
}

export function renderModels(): string {
  return `${topbar("Models")}
  <div class="view"><div id="modelBanner"></div>
  <div id="modelOverview"></div>
  <div id="modelBody">${loadingHtml("Loading models…")}</div>
  <div class="card"><h3>${icon("box")} Add a model</h3>
    <div style="display:grid;grid-template-columns:1fr 1fr;gap:10px;margin-top:10px">
      <label style="font-size:12px;color:var(--text2)">Model id<br>
        <input id="addModelId" type="text" placeholder="my-model-7b" style="${SELECT_STYLE};width:100%"></label>
      <label style="font-size:12px;color:var(--text2)">Quant label (optional)<br>
        <input id="addModelQuant" type="text" placeholder="Q4_K_M" style="${SELECT_STYLE};width:100%"></label>
    </div>
    <label style="font-size:12px;color:var(--text2);display:block;margin-top:10px">GGUF source URL (https)<br>
      <input id="addModelUrl" type="text" placeholder="Model download URL (.gguf)" style="${SELECT_STYLE};width:100%"></label>
    <label style="font-size:12px;color:var(--text2);display:block;margin-top:10px">File name (optional — defaults to the URL's file name)<br>
      <input id="addModelFile" type="text" placeholder="model.gguf" style="${SELECT_STYLE};width:100%"></label>
    <div class="card-actions"><button class="btn" id="addModelBtn">Add model</button></div>
    <div style="font-size:12px;color:var(--text2);margin-top:6px">The model is added enabled, with no pinned SHA-256 — its downloads run unverified until a hash is recorded in the configuration.</div>
    <div id="addModelMsg" style="font-size:12px;margin-top:6px"></div>
  </div></div>`;
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

  /** Poll every 2s while any download is in flight (or verifying) so
   *  progress and the terminal state (complete, verified, failed)
   *  reach the view; stop when nothing is in flight. */
  const syncPoll = (): void => {
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
    if (!disposed && !br.error) {
      const m = new Map<string, { backends: number; req?: number; err?: number; toks?: number }>();
      for (const model of r.data) {
        const serving = br.data.filter((b) => b.configured && sameModelFile(b.model_file, model.gguf_path));
        if (!serving.length) continue;
        const withWin = serving.filter((b) => b.requests_1h !== undefined);
        m.set(model.id, withWin.length
          ? {
              backends: serving.length,
              req: withWin.reduce((a, b) => a + (b.requests_1h ?? 0), 0),
              err: withWin.reduce((a, b) => a + (b.errors_1h ?? 0), 0),
              toks: withWin.reduce((a, b) => a + (b.throughput_toks ?? 0), 0),
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
        ? "No models are configured. Add one with the form below."
        : "No model data in sample mode.");
      syncPoll();
      return;
    }
    lastRows = r.data;
    // While the rename input has focus, a poll repaint would drop
    // the caret mid-word; the draft is synced to wire state, so
    // skipping one repaint loses nothing.
    const renameFocused = renameFor !== null
      && (document.activeElement as HTMLElement | null)?.hasAttribute?.("data-model-rename-input");
    if (!renameFocused) {
      if (mov) mov.innerHTML = modelOverviewHtml(lastRows, modelStats);
      b.innerHTML = `<div class="cards-fluid">${lastRows.map((m) => modelCard(m, downloading, notes, modelStats.get(m.id), renameFor === m.id ? { draft: renameDraft } : null)).join("")}</div>`;
    }
    syncPoll();
  };

  const onClick = async (ev: Event): Promise<void> => {
    const t = (ev.target as HTMLElement).closest("[data-model-act]") as HTMLElement | null;
    if (!t) return;
    const id = t.getAttribute("data-model-id") ?? "";
    const act = t.getAttribute("data-model-act") ?? "";

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
      if (!confirm(`Cancel the download of ${id}? The downloaded progress will be discarded — the partial file is deleted.`)) return;
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
      if (!confirm(`Delete model ${id}? This removes the downloaded file and its config entry. The delete is refused while a backend references the model.`)) return;
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

  const addBtn = root.querySelector("#addModelBtn") as HTMLButtonElement | null;
  addBtn?.addEventListener("click", async () => {
    const msg = root.querySelector("#addModelMsg") as HTMLElement | null;
    const val = (sel: string): string => (root.querySelector(sel) as HTMLInputElement | null)?.value.trim() ?? "";
    const id = val("#addModelId");
    const url = val("#addModelUrl");
    if (!isTauri()) { if (msg) msg.textContent = "Sample mode: adding models runs in the installed app."; return; }
    if (!id || !url) {
      if (msg) { msg.style.color = "var(--err)"; msg.textContent = "Model id and source URL are required."; }
      return;
    }
    addBtn.disabled = true;
    const r = await modelAdd(id, url, val("#addModelFile") || undefined, val("#addModelQuant") || undefined);
    addBtn.disabled = false;
    if (msg) {
      msg.style.color = r.error ? "var(--err)" : "var(--text2)";
      msg.textContent = r.error ? `Add failed: ${r.error}` : `Added ${id}.`;
    }
    if (!r.error) await load();
  });

  await load();

  return () => {
    disposed = true;
    if (pollTimer !== null) window.clearInterval(pollTimer);
    root.removeEventListener("click", onClick);
    root.removeEventListener("click", onOvGoto);
    root.removeEventListener("input", syncRenameDraft);
    root.removeEventListener("keydown", onRenameKey);
  };
}

/* ---------------- Logs ---------------- */

function fmtDateTime(ts: string): string {
  const d = new Date(ts);
  return Number.isNaN(d.getTime()) ? ts : d.toLocaleString("en-GB");
}

/** One log line: visible time stays HH:MM:SS, the full date-time is on
 *  the title attribute. In the merged "all" scope the source process is
 *  chipped per line; previous-run (archived) lines are marked. */
function logRow(r: LogRow, showSource: boolean): string {
  const src = showSource && r.source ? ` <span class="chip">${esc(r.source)}</span>` : "";
  const arch = r.archived ? ` <span class="chip badge-warn">previous run</span>` : "";
  return `<div class="ln"><span class="t" title="${esc(fmtDateTime(r.ts))}">${esc(fmtTime(r.ts))}</span>${src} <span class="chip">${esc(r.stream)}</span>${arch} ${esc(r.line)}</div>`;
}

export function renderLogs(): string {
  return `${topbar("Logs")}
  <div class="view"><div id="logBanner"></div>
  <div class="card" style="margin-bottom:12px"><div style="display:flex;gap:10px;align-items:center;flex-wrap:wrap">
    <label style="font-size:12px;color:var(--text2);display:inline-flex;align-items:center;gap:6px">Scope
      <select id="logScope" style="${SELECT_STYLE}"><option value="all">all</option></select></label>
    <label style="font-size:12px;color:var(--text2);display:inline-flex;align-items:center;gap:6px" title="Level matches line text \u2014 supervised processes do not emit structured levels">Level
      <select id="logLevel" style="${SELECT_STYLE}">
        <option value="">any</option>
        <option value="error">error</option>
        <option value="warn">warn</option>
        <option value="info">info</option>
        <option value="debug">debug</option>
      </select></label>
    <label style="font-size:12px;color:var(--text2);display:inline-flex;align-items:center;gap:6px">Search
      <input id="logSearch" type="text" placeholder="substring\u2026" style="${SELECT_STYLE};min-width:160px"/></label>
    <label style="font-size:12px;color:var(--text2);display:inline-flex;align-items:center;gap:6px">From
      <input id="logFrom" type="datetime-local" style="${SELECT_STYLE}"/></label>
    <label style="font-size:12px;color:var(--text2);display:inline-flex;align-items:center;gap:6px">To
      <input id="logTo" type="datetime-local" style="${SELECT_STYLE}"/></label>
    <label style="font-size:12px;color:var(--text2);display:inline-flex;align-items:center;gap:6px">Sort
      <select id="logSort" style="${SELECT_STYLE}">
        <option value="desc" selected>Newest first</option>
        <option value="asc">Oldest first</option>
      </select></label>
    <label style="font-size:12px;color:var(--text2);display:inline-flex;align-items:center;gap:6px">Limit
      <select id="logLimit" style="${SELECT_STYLE}">
        <option value="50">50</option>
        <option value="200" selected>200</option>
        <option value="500">500</option>
      </select></label>
    <label style="font-size:12px;color:var(--text2);display:inline-flex;align-items:center;gap:6px">
      <input id="logFollow" type="checkbox"/> Follow</label>
    <button class="btn" id="logRefresh">${icon("restart")} Refresh</button>
    <span class="spacer" style="flex:1"></span>
    <button class="btn" id="logExport">${icon("download")} Export diagnostics bundle</button>
    <button class="btn" id="logOpenFolder" style="display:none">Open folder</button>
  </div>
  <div style="font-size:11px;color:var(--text3);margin-top:8px">The bundle contains raw, unredacted process logs. Review it before sharing.</div>
  <div id="logMsg" style="font-size:12px;margin-top:6px"></div></div>
  <div id="logMeta" style="font-size:12px;color:var(--text2);margin-bottom:8px"></div>
  <div style="display:flex;gap:10px;align-items:center;margin-bottom:8px">
    <button class="btn" id="logPrev">Prev</button>
    <button class="btn" id="logNext">Next</button>
    <span id="logPageInfo" style="font-size:12px;color:var(--text2)"></span>
  </div>
  <div id="logBody">${loadingHtml("Loading logs\u2026")}</div></div>`;
}

export async function wireLogs(root: HTMLElement, initialScope?: string): Promise<() => void> {
  let disposed = false;
  let loading = false;
  let followTimer: number | null = null;
  let searchTimer: number | null = null;
  /** 1-based page into the filtered result set; the limit select is
   *  the page size. Any filter change resets this to 1; Refresh and
   *  the Follow re-poll preserve it. */
  let page = 1;
  const scopeSel = root.querySelector("#logScope") as HTMLSelectElement | null;
  const levelSel = root.querySelector("#logLevel") as HTMLSelectElement | null;
  const searchInput = root.querySelector("#logSearch") as HTMLInputElement | null;
  const fromInput = root.querySelector("#logFrom") as HTMLInputElement | null;
  const toInput = root.querySelector("#logTo") as HTMLInputElement | null;
  const sortSel = root.querySelector("#logSort") as HTMLSelectElement | null;
  const limitSel = root.querySelector("#logLimit") as HTMLSelectElement | null;
  const followChk = root.querySelector("#logFollow") as HTMLInputElement | null;
  const refreshBtn = root.querySelector("#logRefresh");
  const prevBtn = root.querySelector("#logPrev") as HTMLButtonElement | null;
  const nextBtn = root.querySelector("#logNext") as HTMLButtonElement | null;
  const pageInfo = root.querySelector("#logPageInfo");
  const exportBtn = root.querySelector("#logExport") as HTMLButtonElement | null;

  /** A datetime-local value as epoch ms (local time); empty or
   *  unparseable means unbounded. */
  const msOf = (v: string | undefined): number | undefined => {
    if (!v) return undefined;
    const ms = new Date(v).getTime();
    return Number.isNaN(ms) ? undefined : ms;
  };

  const msg = (text: string, isErr = false): void => {
    const m = root.querySelector("#logMsg");
    if (m) m.innerHTML = text ? `<span style="color:${isErr ? "var(--err)" : "var(--text2)"}">${esc(text)}</span>` : "";
  };

  /** Merge scope options: "all", backend ids, and the payload's scope
   *  list (scopes with a previous-run ring labeled as such). Existing
   *  options get their labels refreshed — a backend that gains an
   *  archived ring mid-session must gain the suffix, not keep the bare
   *  label from its first insert. */
  const mergeScopeOptions = (entries: { value: string; label: string }[]): void => {
    if (!scopeSel) return;
    const byValue = new Map(Array.from(scopeSel.options).map((o) => [o.value, o]));
    for (const e of entries) {
      const existing = byValue.get(e.value);
      if (existing) {
        if (existing.textContent !== e.label) existing.textContent = e.label;
        continue;
      }
      scopeSel.insertAdjacentHTML("beforeend", `<option value="${esc(e.value)}">${esc(e.label)}</option>`);
      byValue.set(e.value, scopeSel.options[scopeSel.options.length - 1]);
    }
  };

  const filtersActive = (): boolean => Boolean(
    levelSel?.value || searchInput?.value.trim() || fromInput?.value || toInput?.value,
  );

  const load = async (): Promise<void> => {
    if (loading) return;
    loading = true;
    try {
      const scope = scopeSel?.value ?? "all";
      const limit = Number(limitSel?.value ?? 200);
      const level = levelSel?.value || undefined;
      const query = searchInput?.value.trim() || undefined;
      const fetchPage = () => getLogs(scope, {
        limit,
        level,
        query,
        sinceMs: msOf(fromInput?.value),
        untilMs: msOf(toInput?.value),
        order: sortSel?.value === "asc" ? "asc" : "desc",
        offset: (page - 1) * limit,
      });
      let r = await fetchPage();
      if (disposed) return;
      // The filtered set may have shrunk below the current page
      // (e.g. a Follow refresh after lines aged out): clamp to the
      // last page and refetch once, so the page shown is real.
      const lastPage = Math.max(1, Math.ceil(r.data.total / limit));
      if (page > lastPage) {
        page = lastPage;
        r = await fetchPage();
        if (disposed) return;
      }
      setBanner(root, "logBanner", bannerHtml(r));
      mergeScopeOptions(r.data.scopes.map((s) => ({
        value: s.id,
        label: s.previous ? `${s.id} (incl. previous run)` : s.id,
      })));
      const pages = Math.max(1, Math.ceil(r.data.total / limit));
      if (pageInfo) pageInfo.textContent = `Page ${page} of ${pages} \u00b7 ${r.data.total} matching records`;
      if (prevBtn) prevBtn.disabled = page <= 1;
      if (nextBtn) nextBtn.disabled = page >= pages;
      const meta = root.querySelector("#logMeta");
      if (meta) {
        meta.textContent = `Backend logs are kept in memory for this session only \u2014 up to 10,000 lines per process; one previous run per process is kept. The gateway log is file-backed (under the data directory's logs folder) and survives restarts.`;
      }
      const b = root.querySelector("#logBody");
      if (!b) return;
      if (r.error) { b.innerHTML = emptyHtml(`Logs unavailable: ${r.error}`); return; }
      if (!r.data.lines.length) {
        b.innerHTML = emptyHtml(filtersActive() || r.data.total_matching > 0
          ? "No lines match the current filters."
          : "No log lines yet for this scope. Lines appear here once a supervised process writes output.");
        return;
      }
      b.innerHTML = `<div class="logstream">${r.data.lines.map((row) => logRow(row, scope === "all")).join("")}</div>`;
    } finally {
      loading = false;
    }
  };

  // Scope options: "all" plus every backend id (configured or not — a
  // retained ring can outlive its config entry).
  const backs = await getBackends();
  if (!disposed && backs.live && !backs.error) {
    mergeScopeOptions(backs.data.map((bk) => ({ value: bk.id, label: bk.id })));
  }
  // Deep link (#/logs?scope=<id>): select it, adding the option when it
  // is not a known backend id (e.g. a previous-run scope).
  if (scopeSel && initialScope) {
    mergeScopeOptions([{ value: initialScope, label: initialScope }]);
    scopeSel.value = initialScope;
  }

  /** Any filter change (scope, level, text, time bounds, sort, page
   *  size) restarts paging at page 1. */
  const onFilterChange = (): void => { page = 1; void load(); };
  /** Manual Refresh reloads the current page as-is. */
  const onRefresh = (): void => { void load(); };
  const onSearch = (): void => {
    if (searchTimer !== null) window.clearTimeout(searchTimer);
    searchTimer = window.setTimeout(() => { searchTimer = null; page = 1; void load(); }, 300);
  };
  const onPrev = (): void => { if (page > 1) { page -= 1; void load(); } };
  const onNext = (): void => { page += 1; void load(); };
  const onFollow = (): void => {
    if (followTimer !== null) { window.clearInterval(followTimer); followTimer = null; }
    if (followChk?.checked) {
      followTimer = window.setInterval(() => { if (!disposed) void load(); }, 2000);
      void load();
    }
  };
  const openFolderBtn = root.querySelector("#logOpenFolder") as HTMLButtonElement | null;
  let lastExportPath = "";
  const onExport = async (): Promise<void> => {
    if (exportBtn) exportBtn.disabled = true;
    const r = await exportDiagnostics();
    if (exportBtn) exportBtn.disabled = false;
    if (disposed) return;
    if (r.error) { msg(`Export failed: ${r.error}`, true); return; }
    msg(r.data.path ? `Diagnostics bundle written to ${r.data.path}.` : "Diagnostics bundle written.");
    if (r.data.path && openFolderBtn) {
      lastExportPath = r.data.path;
      openFolderBtn.style.display = "";
    }
  };
  openFolderBtn?.addEventListener("click", () => { if (lastExportPath) void openFolder(lastExportPath); });
  scopeSel?.addEventListener("change", onFilterChange);
  levelSel?.addEventListener("change", onFilterChange);
  fromInput?.addEventListener("change", onFilterChange);
  toInput?.addEventListener("change", onFilterChange);
  sortSel?.addEventListener("change", onFilterChange);
  limitSel?.addEventListener("change", onFilterChange);
  searchInput?.addEventListener("input", onSearch);
  followChk?.addEventListener("change", onFollow);
  refreshBtn?.addEventListener("click", onRefresh);
  prevBtn?.addEventListener("click", onPrev);
  nextBtn?.addEventListener("click", onNext);
  exportBtn?.addEventListener("click", onExport);

  await load();

  return () => {
    disposed = true;
    if (followTimer !== null) window.clearInterval(followTimer);
    if (searchTimer !== null) window.clearTimeout(searchTimer);
    scopeSel?.removeEventListener("change", onFilterChange);
    levelSel?.removeEventListener("change", onFilterChange);
    fromInput?.removeEventListener("change", onFilterChange);
    toInput?.removeEventListener("change", onFilterChange);
    sortSel?.removeEventListener("change", onFilterChange);
    limitSel?.removeEventListener("change", onFilterChange);
    searchInput?.removeEventListener("input", onSearch);
    followChk?.removeEventListener("change", onFollow);
    refreshBtn?.removeEventListener("click", onRefresh);
    prevBtn?.removeEventListener("click", onPrev);
    nextBtn?.removeEventListener("click", onNext);
    exportBtn?.removeEventListener("click", onExport);
  };
}
/* ---------------- Gateway ---------------- */

function gatewayPaint(
  status: GatewayStatusData,
  cfg: GatewayConfigData,
  backends: BackendRow[],
  yamlOpen: boolean,
  yamlText: string | null,
  yamlErr: string,
  starting: boolean,
  stopping: boolean,
  backendsError: string | null,
  masterKey: string | null,
  keyRevealed: boolean,
  plan: ActivationPlanData | null,
  planUi: PlanUiState,
): string {
  const port = status.port || cfg.port;
  const backendById = new Map(backends.map((b) => [b.id, b]));
  /** A group member that names a disabled backend is dropped when the
   *  config is generated — say so where it is listed. */
  const memberHtml = (name: string): string => {
    const b = backendById.get(name);
    return b && !b.enabled
      ? `${esc(name)} <span class="chip badge-warn">excluded \u2014 disabled</span>`
      : esc(name);
  };
  /** Is this member dropped at generation time? A backend member is
   *  dropped when disabled; the openrouter member when its tier is
   *  disabled. A member naming neither (an unknown name) cannot be
   *  verified as excluded and is treated as active. */
  const memberExcluded = (name: string): boolean => {
    if (name === "openrouter") return cfg.openrouter ? !cfg.openrouter.enabled : false;
    const b = backendById.get(name);
    return b ? !b.enabled : false;
  };
  const groupByName = new Map(cfg.groups.map((g) => [g.name, g]));
  const fallbackHtml = (name: string): string => {
    if (name === "openrouter" && cfg.openrouter && !cfg.openrouter.enabled) {
      return `${esc(name)} <span class="chip badge-warn">excluded \u2014 tier disabled</span>`;
    }
    // A fallback naming a group whose members are ALL excluded (or
    // which has no members) produces no deployments — the generated
    // config drops the fallback reference itself; chip it as such.
    const g = groupByName.get(name);
    if (g && (g.members.length === 0 || g.members.every(memberExcluded))) {
      return `${esc(name)} <span class="chip badge-warn">excluded \u2014 no active members</span>`;
    }
    return memberHtml(name);
  };

  // The exclusion chips above are computed from the backend list; when
  // that list failed to load, say so instead of silently rendering
  // members as included.
  const backendsNote = backendsError
    ? `<div class="banner" style="margin-bottom:12px">${icon("warn")} Backend list unavailable (${esc(backendsError)}) \u2014 exclusion states in this view may be incomplete.</div>`
    : "";

  const withWin = backends.filter((b) => b.requests_1h !== undefined);
  const gwStats = withWin.length
    ? `<div style="display:grid;grid-template-columns:repeat(4,1fr);gap:10px;margin-top:10px">
        ${[["Requests (1h, all backends)", String(withWin.reduce((a, b) => a + (b.requests_1h ?? 0), 0))],
           ["Errors (1h)", String(withWin.reduce((a, b) => a + (b.errors_1h ?? 0), 0))],
           ["Throughput (tok/s)", trim1(withWin.reduce((a, b) => a + (b.throughput_toks ?? 0), 0))],
           ["Hottest backend", (() => { const h = [...withWin].sort((a, z) => (z.requests_1h ?? 0) - (a.requests_1h ?? 0))[0]; return h ? `${h.id} (${h.requests_1h ?? 0} req)` : "\u2014"; })()]]
          .map(([k, v]) => `<div class="kv"><span class="k">${k}</span><span class="v" style="font-size:13px">${esc(v)}</span></div>`).join("")}
      </div>`
    : `<div style="font-size:12px;color:var(--text2);margin-top:8px">No gateway metrics window yet \u2014 per-backend statistics appear after the gateway has been restarted through the app with metrics enabled and about 30 s of polling.</div>`;
  const summary = `<div class="card" style="margin-bottom:12px"><div class="sec-title">${dot(status.up ? "ok" : "err")} Gateway \u2014 LiteLLM \u00b7 ${status.up ? "up" : "down"}</div>
    <div style="display:grid;grid-template-columns:repeat(4,1fr);gap:10px">
      ${[["Port", `:${port}`], ["Groups", String(status.groups)], ["Routing rules", String(status.routing_rules)], ["Routing test traces", `${status.routing_log_len} recorded`]]
        .map(([k, v]) => `<div class="kv"><span class="k">${k}</span><span class="v" style="font-size:13px">${esc(v)}</span></div>`).join("")}
    </div>
    ${gwStats}
    <div style="font-size:12px;color:var(--text2);margin-top:8px">Routing test traces are dry-run decisions recorded from routing tests this session \u2014 live requests are not logged.</div></div>`;

  const endpoint = `<div class="card" style="margin-bottom:12px"><div class="sec-title">${icon("gateway")} Client endpoint</div>
    <div style="font-size:12px;color:var(--text2);display:grid;gap:4px">
      <div>Base URL: <span style="font-family:var(--mono);color:var(--text)">http://127.0.0.1:${port}/v1</span></div>
      <div>Network access: <select id="gwBindSelect" class="input" style="width:auto;display:inline-block;padding:2px 6px">
          <option value="0.0.0.0"${status.bind_host !== "127.0.0.1" ? " selected" : ""}>Local network (all interfaces)</option>
          <option value="127.0.0.1"${status.bind_host === "127.0.0.1" ? " selected" : ""}>This computer only (127.0.0.1)</option>
        </select></div>
      ${status.bind_host === "127.0.0.1"
        ? `<div>Reachable from this machine only.</div>`
        : status.lan_ip
          ? `<div>LAN base URL: <span style="font-family:var(--mono);color:var(--text)">http://${esc(status.lan_ip)}:${port}/v1</span> \u2014 other machines use this address with the master key. The VS Code LiteLLM extension takes it without /v1.</div>`
          : `<div>Bound to all interfaces; this machine's LAN address could not be determined.</div>`}
      <div>Clients authenticate with the master key below \u2014 LiteLLM requires one. The VS Code LiteLLM extension takes the base URL without /v1 (<span style="font-family:var(--mono);color:var(--text)">http://127.0.0.1:${port}</span>); it appends /v1 itself.</div>
      <div>Master key: <span style="font-family:var(--mono);color:var(--text)">${masterKey === null ? "not loaded \u2014 Reveal loads it" : keyRevealed ? esc(masterKey) : "\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022"}</span>
        <button class="btn" id="gwKeyRevealBtn">${keyRevealed ? "Hide" : "Reveal"}</button>
        <button class="btn" id="gwKeyCopyBtn">Copy</button></div>
      <div>LiteLLM admin UI: sign in as <span style="font-family:var(--mono);color:var(--text)">admin</span> with the master key as the password. The admin UI requires a database \u2014 run the setup wizard's Install PostgreSQL step if sign-in reports \"Not connected to DB\".</div>
    </div>
    <div class="card-actions">
      <button class="btn" id="gwStartBtn"${starting || stopping ? " disabled" : ""}>${icon("restart")} ${status.up ? "Restart gateway" : "Start gateway"}</button>
      <button class="btn" id="gwStopBtn"${status.up && !starting && !stopping ? "" : " disabled"}>${icon("pause")} Stop gateway</button>
      <a class="btn" href="#/wizard">${icon("wrench")} Setup Wizard</a>
      <button class="btn" id="gwAdminUiBtn">${icon("gateway")} LiteLLM admin UI</button>
    </div>
    <div style="font-size:12px;color:var(--text2);margin-top:6px">Runs the Setup Wizard's start-gateway step: LiteLLM binds the configured address, and the step reconciles the Windows firewall rule with it (one elevation prompt when the rule state changes). When the OpenRouter tier is enabled, the fallback is armed from Windows Credential Manager.</div></div>`;

  const or = cfg.openrouter;
  const orRows = or ? [
    ["Enabled", or.enabled ? "yes" : "no"],
    ["Credential reference", or.cred_ref ?? "not set"],
    ["Daily cap", `$${or.daily_cap_usd.toFixed(2)} per day`],
    ["Model allowlist", or.model_allowlist.length ? or.model_allowlist.join(", ") : "none"],
  ] : [];
  // Credential presence comes from the gateway_status probe payload
  // (a presence check only — the value is never sent to the UI). When
  // the backend reports no presence (null) and a reference IS set,
  // fall back to the standing explanation of how the key is read.
  const credHtml = or && or.cred_ref
    ? (status.openrouter_cred_present === true
      ? `<div style="font-size:12px;color:var(--text2);margin-top:8px">Credential present in Windows Credential Manager.</div>`
      : status.openrouter_cred_present === false
        ? `<div style="font-size:12px;color:var(--err);margin-top:8px">Credential not found in Windows Credential Manager \u2014 the fallback cannot authenticate.</div>`
        : `<div style="font-size:12px;color:var(--text2);margin-top:8px">When the gateway starts, the key is read from Windows Credential Manager (reference above) and passed to LiteLLM as OPENROUTER_API_KEY. The key itself is never stored in the configuration.</div>`)
    : "";
  const openrouter = `<div class="card" style="margin-bottom:12px"><div class="sec-title">${icon("gateway")} OpenRouter fallback tier</div>
    ${or === null
      ? `<div style="font-size:12px;color:var(--text2)">The OpenRouter fallback tier is not configured.</div>`
      : `<div class="kv" style="gap:10px">${orRows.map(([k, v]) => `<div style="display:flex;justify-content:space-between;gap:16px"><span class="k">${esc(k)}</span><span class="v" style="font-size:12px;text-align:right">${esc(v)}</span></div>`).join("")}</div>
        ${or.enabled
          ? (or.cred_ref
            ? credHtml
            : `<div style="font-size:12px;color:var(--err);margin-top:8px">No credential reference is set \u2014 the fallback cannot authenticate until one is configured.</div>`)
          : `<div style="font-size:12px;color:var(--text2);margin-top:8px">The tier is disabled \u2014 fallback entries naming openrouter are dropped when the gateway config is generated, and no traffic is sent to OpenRouter.</div>${credHtml}`}
        <div class="card-actions"><button class="btn" id="gwOrToggle">${or.enabled ? "Disable tier" : "Enable tier"}</button></div>`}
  </div>`;

  const effStrategy = effectiveGatewayStrategy(cfg);
  const strategyNote = effStrategy
    ? `<div style="font-size:12px;color:var(--text2);margin-top:8px">LiteLLM applies ONE global routing strategy \u2014 the strategy chips on the groups are labels, not per-group settings. Effective strategy: <b>${esc(effStrategy)}</b>.</div>`
    : "";

  const flow = cfg.groups.length
    ? `<div class="card" style="margin-bottom:12px"><div class="sec-title">${icon("gateway")} Routing flow</div>
      <div style="font-family:var(--mono);font-size:12px;color:var(--text2);line-height:2">
        ${cfg.groups.map((g) => `<span class="chip">${esc(g.name)}</span>${g.enabled ? "" : ` <span class="chip status-tag status-off">disabled</span>`} <span style="color:var(--text3)">\u2192</span> ${g.members.map(memberHtml).join(", ") || "\u2014"}${g.fallbacks.length ? `<br/><span style="color:var(--text3)">fallback - - \u2192</span> ${g.fallbacks.map(fallbackHtml).join(", ")}` : ""}`).join("<br/>")}
      </div>${strategyNote}</div>`
    : "";

  const groups = cfg.groups.length
    ? `<div class="grid2">${cfg.groups.map((g) => `<div class="card"${g.enabled ? "" : ` style="opacity:.65"`}>
      <h3><span class="chip" style="border-color:var(--accent)">${esc(g.name)}</span>
        <span class="chip status-tag ${g.enabled ? "status-on" : "status-off"}">${g.enabled ? "enabled" : "disabled"}</span>
        <span class="spacer" style="flex:1"></span><span class="chip">${esc(g.strategy)}</span></h3>
      <div style="font-size:12px;color:var(--text2);margin-top:8px">${g.enabled
        ? `${g.enabled_members} of ${g.members.length} member backend${g.members.length === 1 ? "" : "s"} enabled`
        : "No enabled backends \u2014 omitted from the generated gateway config"}</div>
      <div style="font-size:12px;color:var(--text2);margin-top:8px">Members</div>
      <div style="font-family:var(--mono);font-size:12px">${g.members.map(memberHtml).join(", ") || "\u2014"}</div>
      <div style="font-size:12px;color:var(--text2);margin-top:8px">Fallbacks</div>
      <div style="font-family:var(--mono);font-size:12px">${g.fallbacks.map(fallbackHtml).join(", ") || "none"}</div>
    </div>`).join("")}</div>`
    : emptyHtml("No model groups are configured for the gateway.");

  const rules = cfg.routing_rules.length
    ? `<div class="card" style="margin-top:12px"><div class="sec-title">Routing rules</div>
      ${cfg.routing_rules.map((r) => `<div style="display:flex;gap:8px;align-items:center;padding:6px 0;border-bottom:1px solid var(--border)">
        <span style="font-size:12px">${esc(r.description)}</span>
        <span class="spacer" style="flex:1"></span>
        <span style="font-family:var(--mono);font-size:11px;color:var(--text3)">${esc(r.id)}</span>
        <span class="chip">${esc(r.source)}</span></div>`).join("")}
    </div>`
    : "";

  const yaml = `<div class="card" style="margin-top:12px"><div class="sec-title">Generated config (preview)</div>
    <div style="font-size:12px;color:var(--text2)">The running gateway reads the config file written when it was last (re)started from the Setup Wizard. This preview is regenerated from the current configuration \u2014 after edits, the two can differ until the gateway is restarted.</div>
    <div class="card-actions" style="margin-top:8px"><button class="btn" id="gwYamlBtn">${yamlOpen ? "Hide YAML" : "Preview LiteLLM YAML"}</button></div>
    ${yamlOpen ? (yamlErr
      ? `<div style="margin-top:10px;font-size:12px;color:var(--err)">Could not load YAML: ${esc(yamlErr)}</div>`
      : `<pre style="margin-top:10px;padding:12px;background:var(--panel2);border:1px solid var(--border);border-radius:8px;font-family:var(--mono);font-size:11px;white-space:pre-wrap;word-break:break-word;max-height:420px;overflow:auto">${esc(yamlText ?? "")}</pre>
         <div style="font-size:11px;color:var(--text3);margin-top:6px">\`api_key: local\` in the generated config is LiteLLM's required non-empty placeholder \u2014 it is not a real key.</div>`) : ""}
  </div>`;

  return activationPlanHtml(plan, planUi) + backendsNote + summary + endpoint + openrouter + flow + groups + rules + yaml;
}

export function renderGateway(): string {
  return `${topbar("Gateway")}
  <div class="view"><div id="gwBanner"></div>
  <div id="gwMsg" style="font-size:12px;margin-bottom:8px"></div>
  <div id="gwBody">${loadingHtml("Loading gateway configuration\u2026")}</div></div>`;
}

export async function wireGateway(root: HTMLElement): Promise<() => void> {
  let yamlOpen = false;
  let yamlText: string | null = null;
  let yamlErr = "";
  let disposed = false;
  let starting = false;
  let stopping = false;
  /** The probe's up state from the latest load (false when the probe
   *  errored — the restart path keys its stop-first step to it). */
  let lastUp = false;
  /** Last loaded config (the toggle handler patches against it). */
  let lastCfg: GatewayConfigData | null = null;
  /** Gateway port from the latest load (the admin UI link target). */
  let lastPort = 4000;
  /** Master key, fetched on first Reveal/Copy; kept in the closure
   *  so repaints do not re-read the credential store. */
  let masterKey: string | null = null;
  let keyRevealed = false;
  /** Gateway activation plan, painted at the top of the view. */
  let lastPlan: ActivationPlanData | null = null;
  const planUi = newPlanUi();

  const gwMsg = (text: string, isErr = false): void => {
    const m = root.querySelector("#gwMsg");
    if (m) m.innerHTML = text ? `<span style="color:${isErr ? "var(--err)" : "var(--text2)"}">${esc(text)}</span>` : "";
  };

  /** Append a manual-command block under the gateway message (a
   * failed start-gateway outcome carries one: the exact commands
   * to start the gateway by hand). #gwMsg lives outside #gwBody,
   * so the block survives the load() repaint that follows. */
  const gwShowManual = (cmd: string): void => {
    const m = root.querySelector("#gwMsg");
    if (m) m.insertAdjacentHTML("beforeend", manualCommandHtml(cmd));
  };

  /** Fetch the master key on first use and cache it in the closure. */
  const ensureMasterKey = async (): Promise<string | null> => {
    if (masterKey !== null) return masterKey;
    const r = await getGatewayMasterKey();
    if (r.error || !r.data.master_key) {
      gwMsg(`Master key unavailable: ${r.error ?? "empty response"}`, true);
      return null;
    }
    masterKey = r.data.master_key;
    return masterKey;
  };

  const load = async (): Promise<void> => {
    const [st, cfg, backs, pr] = await Promise.all([getGatewayStatus(), getGatewayConfig(), getBackends(), getActivationPlan()]);
    if (disposed) return;
    if (!pr.error && pr.data) lastPlan = pr.data;
    const err = st.error ?? cfg.error;
    setBanner(root, "gwBanner", bannerHtml({ live: st.live && cfg.live, error: err }));
    const b = root.querySelector("#gwBody");
    if (!b) return;
    if (err) { b.innerHTML = emptyHtml(`Gateway configuration unavailable: ${err}`); return; }
    if (!cfg.data) {
      b.innerHTML = emptyHtml(st.live
        ? "The gateway returned no configuration."
        : "No gateway data in sample mode.");
      return;
    }
    lastCfg = cfg.data;
    lastUp = st.data.up;
    lastPort = st.data.port || cfg.data.port;
    b.innerHTML = gatewayPaint(st.data, cfg.data, backs.data, yamlOpen, yamlText, yamlErr, starting, stopping, backs.error ?? null, masterKey, keyRevealed, lastPlan, planUi);
  };

  const onClick = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLElement;

    const pt = t.closest("[data-plan-act]") as HTMLElement | null;
    if (pt) {
      await handlePlanAction(pt.getAttribute("data-plan-act") ?? "", planUi, () => { void load(); }, load, lastPlan);
      return;
    }

    if (t.closest("#gwYamlBtn")) {
      if (yamlOpen) {
        yamlOpen = false;
        await load();
        return;
      }
      // Refetch on every Preview click: a cached copy (or a cached
      // failure) would hide config edits made since the last preview.
      yamlOpen = true;
      yamlText = null;
      yamlErr = "";
      await load();
      const r = await getGatewayYaml();
      if (disposed) return;
      if (r.error) yamlErr = r.error;
      else yamlText = r.data.yaml;
      await load();
      return;
    }

    if (t.closest("#gwKeyRevealBtn")) {
      if (keyRevealed) {
        keyRevealed = false;
        await load();
        return;
      }
      if ((await ensureMasterKey()) === null) return;
      if (disposed) return;
      keyRevealed = true;
      await load();
      return;
    }

    if (t.closest("#gwKeyCopyBtn")) {
      const key = await ensureMasterKey();
      if (key === null || disposed) return;
      try {
        await navigator.clipboard.writeText(key);
        gwMsg("Master key copied to the clipboard.");
      } catch {
        gwMsg("Copy failed \u2014 Reveal the key and copy it manually.", true);
      }
      return;
    }

    if (t.closest("#gwAdminUiBtn")) {
      const r = await openUrl(`http://127.0.0.1:${lastPort}/ui`);
      if (disposed) return;
      if (r.error) gwMsg(`Could not open the admin UI: ${r.error}`, true);
      return;
    }

    if (t.closest("#gwStopBtn")) {
      if (stopping || starting) return;
      stopping = true;
      await load(); // repaint with both actions disabled
      const r = await gatewayStop();
      stopping = false;
      if (disposed) return;
      if (r.error) {
        gwMsg(`Gateway stop failed: ${r.error}`, true);
      } else {
        gwMsg(r.data.stopped ? "Gateway stopped." : "Gateway was not running.");
      }
      await load();
      return;
    }

    if (t.closest("#gwStartBtn")) {
      if (starting || stopping) return;
      starting = true;
      try {
        // Restart is stop-then-start: the wizard step spawns a fresh
        // LiteLLM, which cannot take the port while the old process
        // holds it. If the stop fails, surface it and do not start.
        if (lastUp) {
          gwMsg("Stopping the running gateway, then running the start-gateway step\u2026");
          await load();
          const sr = await gatewayStop();
          if (disposed) return;
          if (sr.error) {
            gwMsg(`Gateway restart failed \u2014 the running gateway could not be stopped: ${sr.error}`, true);
            return;
          }
        } else {
          gwMsg("Running the start-gateway step\u2026");
          await load();
        }
        const r = await runWizardStep("start-gateway");
        if (disposed) return;
        if (r.error) {
          gwMsg(`Gateway start failed: ${r.error}`, true);
        } else if (r.data.busy || !r.data.outcome) {
          gwMsg(wizardBusyMessage(r.data, "A setup step is already running \u2014 wait for it to finish, then start the gateway again."), true);
        } else {
          const o = r.data.outcome;
          const tail = o.log_tail.length ? ` ${o.log_tail[o.log_tail.length - 1]}` : "";
          // A successful (re)start makes any pending activation
          // plan live; clear it promptly instead of waiting for the
          // journal's mtime resolution.
          if (o.state !== "failed") await activationComplete();
          gwMsg(`Start-gateway step ${o.state} \u2014 ${o.message}${tail}`, o.state === "failed");
          if (o.state === "failed" && o.manual_command) gwShowManual(o.manual_command);
        }
      } finally {
        starting = false;
        if (!disposed) await load();
      }
      return;
    }

    if (t.closest("#gwOrToggle")) {
      const or = lastCfg?.openrouter;
      if (!or) return;
      const btn = t.closest("#gwOrToggle") as HTMLButtonElement;
      btn.disabled = true;
      const r = await updateGatewayConfig({ openrouter: { enabled: !or.enabled } });
      if (disposed) return;
      if (r.error) {
        gwMsg(`OpenRouter tier change failed: ${r.error}`, true);
      } else if (!r.data.persisted) {
        gwMsg("The change was applied to the in-memory configuration only \u2014 it was not saved and will be lost when the app restarts.", true);
      } else {
        gwMsg(or.enabled ? "OpenRouter tier disabled and saved." : "OpenRouter tier enabled and saved.");
      }
      await load();
    }
  };
  const onBindChange = async (ev: Event): Promise<void> => {
    const sel = ev.target as HTMLElement;
    if (!sel.closest("#gwBindSelect")) return;
    const host = (sel as HTMLSelectElement).value;
    const r = await setGatewayBindHost(host);
    if (r.error) {
      gwMsg(`Network access not saved: ${r.error}`, true);
      return;
    }
    gwMsg("Network access saved — applies at the next gateway restart; the start-gateway step also reconciles the Windows firewall rule.");
    await load();
  };
  root.addEventListener("click", onClick);
  root.addEventListener("change", onBindChange);

  await load();

  // Status poll: a gateway that dies while the view is open must show
  // as down without a manual refresh.
  const timer = window.setInterval(() => {
    if (!disposed && !starting && !stopping) void load();
  }, 5000);

  return () => {
    disposed = true;
    window.clearInterval(timer);
    root.removeEventListener("click", onClick);
    root.removeEventListener("change", onBindChange);
  };
}

/* ---------------- Settings ---------------- */

function asRecord(v: unknown): Record<string, unknown> | null {
  return v !== null && typeof v === "object" && !Array.isArray(v)
    ? (v as Record<string, unknown>)
    : null;
}

function primStr(v: unknown): string | null {
  if (typeof v === "string") return v;
  if (typeof v === "number" || typeof v === "boolean") return String(v);
  return null;
}

/** Defensive one-line summary of a config value: primitives verbatim,
    arrays/objects as counts — never invented detail. */
function summarizeValue(v: unknown): string {
  const p = primStr(v);
  if (p !== null) return p;
  if (Array.isArray(v)) return `${v.length} item${v.length === 1 ? "" : "s"}`;
  const r = asRecord(v);
  if (r) return `{ ${Object.keys(r).length} fields }`;
  return "\u2014";
}

function summarizeEntries(rec: Record<string, unknown>, limit = 8): { key: string; value: string }[] {
  return Object.entries(rec).slice(0, limit).map(([key, v]) => ({ key, value: summarizeValue(v) }));
}

function settingsRows(rows: { key: string; value: string }[]): string {
  if (!rows.length) return `<div style="font-size:12px;color:var(--text2)">Nothing reported.</div>`;
  return `<div class="kv" style="gap:10px">${rows.map((r) => `<div style="display:flex;justify-content:space-between;gap:16px"><span class="k">${esc(r.key)}</span><span class="v" style="font-size:12px;text-align:right">${esc(r.value)}</span></div>`).join("")}</div>`;
}

function settingsCard(title: string, iconName: "gear" | "shield" | "wrench", inner: string): string {
  return `<div class="card" style="margin-bottom:12px"><div class="sec-title">${icon(iconName)} ${esc(title)}</div>${inner}</div>`;
}

const MXC_MODES: { mode: string; label: string }[] = [
  { mode: "learning", label: "Learning" },
  { mode: "enforced", label: "Enforced" },
  { mode: "disabled", label: "Disabled" },
];

/** Cap a rule list for display, noting how many were elided. */
function capRows<T>(rows: T[], limit: number): { rows: T[]; more: number } {
  return rows.length > limit
    ? { rows: rows.slice(0, limit), more: rows.length - limit }
    : { rows, more: 0 };
}

function moreNote(more: number): string {
  return more > 0 ? `<div style="font-size:11px;color:var(--text3)">+ ${more} more</div>` : "";
}

const FS_ACCESS_LABEL: Record<string, string> = { readwrite: "read-write" };
const ACTIVITY_KIND_LABEL: Record<string, string> = {
  FileRead: "file read", FileWrite: "file write", Network: "network",
};

/** The Active policy card body: the policy's actual rules, with the
 *  provenance stated — a fetch failure is "unavailable — <error>",
 *  never conflated with a genuinely absent policy. */
function mxcPolicyInner(policyR: { data: MxcPolicyData | null; error?: string; live: boolean }): string {
  if (policyR.error) {
    return `<div style="font-size:12px;color:var(--err);margin-top:10px">Policy unavailable \u2014 ${esc(policyR.error)}</div>`;
  }
  const data = policyR.data;
  if (!data || !data.policy) {
    return `<div style="font-size:12px;color:var(--text2);margin-top:10px">${policyR.live ? "Policy unavailable." : "Policy is available in the installed app."}</div>`;
  }
  const p = data.policy;
  const srcLine = data.source === "default"
    ? "No policy file exists \u2014 the built-in default policy is in effect."
    : "Loaded from the configured policy file.";
  const basics = [
    { key: "Policy version", value: String(p.version) },
    { key: "Default", value: p.default_deny ? "deny \u2014 anything not explicitly allowed is denied" : "allow" },
    { key: "UI access", value: p.ui_access ? "enabled" : "disabled" },
  ];
  const fs = capRows(p.filesystem ?? [], 12);
  const fsRows = fs.rows.map((r) => ({ key: r.path, value: FS_ACCESS_LABEL[r.access] ?? r.access }));
  const net = capRows(p.network ?? [], 12);
  const netRows = net.rows.map((r) => ({
    key: r.ports && r.ports.length ? `${r.host}:${r.ports.join(",")}` : r.host,
    value: r.decision,
  }));
  const cred = capRows(p.credentials ?? [], 12);
  const credRows = cred.rows.map((r) => ({ key: r.name, value: r.target }));
  return `<div style="font-size:12px;color:var(--text2);margin:10px 0 6px">Active policy</div>
    <div style="font-size:12px;color:var(--text2);margin-bottom:6px">${esc(srcLine)}</div>
    ${settingsRows(basics)}
    <div style="font-size:12px;color:var(--text2);margin:10px 0 6px">Filesystem rules (${(p.filesystem ?? []).length})</div>
    ${settingsRows(fsRows)}${moreNote(fs.more)}
    <div style="font-size:12px;color:var(--text2);margin:10px 0 6px">Network rules (${(p.network ?? []).length})</div>
    ${settingsRows(netRows)}${moreNote(net.more)}
    ${credRows.length ? `<div style="font-size:12px;color:var(--text2);margin:10px 0 6px">Credentials (${(p.credentials ?? []).length})</div>${settingsRows(credRows)}${moreNote(cred.more)}` : ""}`;
}

/** The Recent activity body: structured rows (time, decision, what
 *  was accessed, count), with fetch failure distinct from an empty
 *  report. */
function mxcActivityInner(activityR: { data: MxcActivityData | null; error?: string; live: boolean }): string {
  if (activityR.error) {
    return `<div style="font-size:12px;color:var(--err);margin-top:10px">Activity unavailable \u2014 ${esc(activityR.error)}</div>`;
  }
  const data = activityR.data;
  if (!data) {
    return `<div style="font-size:12px;color:var(--text2);margin-top:10px">${activityR.live ? "No recent MXC activity reported." : "Activity is available in the installed app."}</div>`;
  }
  if (!data.entries.length) {
    return `<div style="font-size:12px;color:var(--text2);margin-top:10px">${data.source === "none" ? "No MXC activity recorded yet." : "No recent MXC activity reported."}</div>`;
  }
  const rows = data.entries.slice(0, 8).map((e) => {
    const kind = e.kind ? (ACTIVITY_KIND_LABEL[e.kind] ?? e.kind) : "";
    const target = e.path_or_host ? e.path_or_host + (e.port !== null ? `:${e.port}` : "") : "";
    const what = [kind, target].filter(Boolean).join(" \u00b7 ");
    return `<div style="padding:4px 0;border-bottom:1px solid var(--border);font-size:12px">
      <span title="${esc(fmtDateTime(e.timestamp))}">${esc(fmtTime(e.timestamp))}</span>
      <span class="chip">${esc(e.decision)}</span>
      ${what ? `<span>${esc(what)}</span>` : ""}
      ${e.count > 1 ? `<span style="color:var(--text3)">\u00d7${e.count}</span>` : ""}
      ${e.description ? `<div style="color:var(--text2)">${esc(e.description)}</div>` : ""}
    </div>`;
  }).join("");
  return `<div style="font-size:12px;color:var(--text2);margin:10px 0 6px">Recent activity</div>${rows}`;
}

function settingsPaint(
  cfg: Record<string, unknown>,
  policyR: { data: MxcPolicyData | null; error?: string; live: boolean },
  activityR: { data: MxcActivityData | null; error?: string; live: boolean },
  selfTestHtml: string,
): string {
  // --- General ---
  const generalRows: { key: string; value: string }[] = [];
  const dataDir = primStr(cfg.data_dir);
  if (dataDir !== null) generalRows.push({ key: "Data directory", value: dataDir });
  const configPath = primStr(cfg.config_path);
  if (configPath !== null) generalRows.push({ key: "Configuration file", value: configPath });
  const tel = asRecord(cfg.telemetry);
  if (tel) {
    const intervalKey = ["poll_interval_ms", "interval_ms", "poll_ms", "interval"].find((k) => typeof tel[k] === "number");
    if (intervalKey) generalRows.push({ key: "Telemetry poll interval", value: `${tel[intervalKey]} ms` });
    for (const row of summarizeEntries(tel)) {
      if (intervalKey && row.key === intervalKey) continue;
      generalRows.push({ key: `Telemetry \u00b7 ${row.key}`, value: row.value });
    }
  } else if (primStr(cfg.telemetry) !== null) {
    generalRows.push({ key: "Telemetry poll interval", value: `${primStr(cfg.telemetry)} ms` });
  }
  const winml = asRecord(cfg.winml);
  if (winml && primStr(winml.enabled) !== null) {
    generalRows.push({ key: "Windows ML", value: winml.enabled ? "enabled" : "disabled" });
  }
  if (primStr(cfg.setup_completed) !== null) {
    generalRows.push({ key: "Setup completed", value: cfg.setup_completed ? "yes" : "no" });
  }
  const general = settingsCard("General", "gear", settingsRows(generalRows));

  // --- Updates ---
  const updates = asRecord(cfg.updates);
  let updatesBody: string;
  if (updates) {
    const { components, ...restUpdates } = updates;
    const comps = asRecord(components);
    const compRows = comps ? Object.entries(comps).map(([name, v]) => {
      const c = asRecord(v);
      if (!c) return { key: name, value: summarizeValue(v) };
      const pinned = primStr(c.pinned_version);
      const parts = [pinned !== null ? `pinned ${pinned}` : "unpinned", c.auto_update ? "auto-update on" : "auto-update off"];
      const checked = primStr(c.last_checked);
      if (checked !== null) parts.push(`checked ${fmtDate(checked)}`);
      return { key: name, value: parts.join(" \u00b7 ") };
    }) : [];
    const restRows = summarizeEntries(restUpdates);
    // UpdatePolicy today carries only `components`, so the rest is
    // empty and its "Nothing reported." placeholder would print
    // directly above the populated Components section. Render each
    // part only when it has rows; the placeholder appears only when
    // there is genuinely nothing — no other fields, no components.
    updatesBody = `${restRows.length ? settingsRows(restRows) : ""}
      ${compRows.length ? `<div style="font-size:12px;color:var(--text2);margin:10px 0 6px">Components</div>${settingsRows(compRows)}` : ""}
      ${!restRows.length && !compRows.length ? settingsRows([]) : ""}`;
  } else {
    updatesBody = `<div style="font-size:12px;color:var(--text2)">No update policy reported.</div>`;
  }
  const updatesInner = `${updatesBody}
    <div style="margin-top:10px;font-size:12px;color:var(--text2)">Update checking is not supported in v0.1.x.</div>
    <div class="card-actions"><a class="btn" href="#/wizard">${icon("search")} Open Setup Wizard</a>
    <button class="btn" id="setExportBtn">${icon("download")} Export diagnostics bundle</button>
    <button class="btn" id="setOpenFolder" style="display:none">Open folder</button></div>
    <div style="font-size:11px;color:var(--text3);margin-top:8px">The bundle contains raw, unredacted process logs. Review it before sharing.</div>`;
  const updatesCard = settingsCard("Updates & diagnostics", "wrench", updatesInner);

  // --- Notifications ---
  const notif = asRecord(cfg.notifications);
  const notifCard = settingsCard("Notifications", "gear",
    notif ? settingsRows(summarizeEntries(notif))
      : `<div style="font-size:12px;color:var(--text2)">No notification settings reported.</div>`);

  // --- MXC ---
  const mxc = asRecord(cfg.mxc);
  const mode = (mxc ? primStr(mxc.mode) : null) ?? "";
  const modeLc = mode.toLowerCase();
  const mxcRows: { key: string; value: string }[] = [];
  if (mode) mxcRows.push({ key: "Mode", value: mode });
  if (mxc && primStr(mxc.path) !== null) mxcRows.push({ key: "Policy file", value: primStr(mxc.path) as string });

  const policyInner = mxcPolicyInner(policyR);
  const activityInner = mxcActivityInner(activityR);

  const modeBtns = `<div class="card-actions">${MXC_MODES.map((m) =>
    `<button class="btn${modeLc === m.mode ? " primary" : ""}" data-mxc-mode="${m.mode}">${m.label}</button>`).join("")}</div>
    <div style="font-size:12px;color:var(--text2);margin-top:8px;display:grid;gap:2px">
      <div><b>Learning</b> \u2014 observe and log only; nothing is blocked.</div>
      <div><b>Enforced</b> \u2014 execution outside the policy is blocked.</div>
      <div><b>Disabled</b> \u2014 sandbox off; tool execution is not mediated.</div>
    </div>
    <div style="font-size:12px;color:var(--text2);margin-top:6px">The mode is saved configuration: this app records it, and the MXC sandbox runtime applies it when that runtime executes tools.</div>`;

  const selfTest = `<div class="card-actions"><button class="btn" data-mxc-selftest>Run self-test</button></div>
    <div id="mxcSelfTest">${selfTestHtml}</div>`;

  const mxcCard = settingsCard("MXC sandbox", "shield",
    `${settingsRows(mxcRows)}${policyInner}${activityInner}${modeBtns}${selfTest}`);

  return general + updatesCard + notifCard + mxcCard;
}

export function renderSettings(): string {
  return `${topbar("Settings")}
  <div class="view" style="max-width:960px"><div id="setBanner"></div>
  <div id="setMsg" style="font-size:12px;margin-bottom:8px"></div>
  <div id="setBody">${loadingHtml("Loading settings\u2026")}</div></div>`;
}

export async function wireSettings(root: HTMLElement): Promise<() => void> {
  let disposed = false;
  /** Self-test result HTML survives repaints (kept in the closure). */
  let selfTestHtml = "";

  let lastExportPath = "";
  const setMsg = (text: string, isErr = false): void => {
    const m = root.querySelector("#setMsg");
    if (m) m.innerHTML = text ? `<span style="color:${isErr ? "var(--err)" : "var(--text2)"}">${esc(text)}</span>` : "";
  };

  const load = async (): Promise<void> => {
    const [cfgR, polR, actR] = await Promise.all([getAppConfig(), getMxcPolicy(), getMxcActivity()]);
    if (disposed) return;
    setBanner(root, "setBanner", bannerHtml(cfgR));
    const b = root.querySelector("#setBody");
    if (!b) return;
    if (cfgR.error) { b.innerHTML = emptyHtml(`Settings unavailable: ${cfgR.error}`); return; }
    if (!cfgR.data) {
      b.innerHTML = emptyHtml(cfgR.live
        ? "The app returned no configuration."
        : "No configuration in sample mode.");
      return;
    }
    // Policy/activity CallResults go in whole: their .error renders as
    // "unavailable — <error>", distinct from genuine absence.
    b.innerHTML = settingsPaint(cfgR.data, polR, actR, selfTestHtml);
  };

  const onClick = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLElement;

    const exportBtn = t.closest("#setExportBtn") as HTMLButtonElement | null;
    if (exportBtn) {
      exportBtn.disabled = true;
      const r = await exportDiagnostics();
      exportBtn.disabled = false;
      if (disposed) return;
      if (r.error) setMsg(`Export failed: ${r.error}`, true);
      else {
        setMsg(r.data.path ? `Diagnostics bundle written to ${r.data.path}.` : "Diagnostics bundle written.");
        if (r.data.path) {
          lastExportPath = r.data.path;
          const of = root.querySelector("#setOpenFolder") as HTMLButtonElement | null;
          if (of) of.style.display = "";
        }
      }
      return;
    }

    const openFolderBtn = t.closest("#setOpenFolder") as HTMLButtonElement | null;
    if (openFolderBtn) {
      if (lastExportPath) void openFolder(lastExportPath);
      return;
    }

    const stBtn = t.closest("[data-mxc-selftest]") as HTMLButtonElement | null;
    if (stBtn) {
      stBtn.disabled = true;
      const r = await mxcRunSelfTest();
      if (disposed) return;
      if (r.error) {
        selfTestHtml = `<div style="font-size:12px;color:var(--err);margin-top:8px">Self-test failed: ${esc(r.error)}</div>`;
      } else {
        const s = r.data.summary;
        const failed = r.data.cases.filter((c) => !c.passed);
        selfTestHtml = `<div style="font-size:12px;margin-top:8px">
          <div>Self-test: ${s.passed}/${s.total} probes passed${s.failed ? ` \u2014 ${s.failed} failed` : ""}.</div>
          ${failed.map((c) => `<div style="color:var(--err)">${esc(c.name)}: expected ${esc(c.expected)}, got ${esc(c.actual)} \u2014 ${esc(c.description)}</div>`).join("")}
          ${failed.length ? `<div style="color:var(--text2);margin-top:4px">To fix a failing case, edit the policy file at the Policy file path shown above and restart the app to apply it.</div>` : ""}
        </div>`;
      }
      await load();
      return;
    }

    const modeBtn = t.closest("[data-mxc-mode]") as HTMLButtonElement | null;
    if (!modeBtn) return;
    const mode = modeBtn.getAttribute("data-mxc-mode") ?? "";
    if (mode === "disabled" && !confirm("Disable the MXC sandbox? Tool execution will no longer be mediated by the policy.")) return;
    modeBtn.disabled = true;
    const r = await setMxcMode(mode);
    if (disposed) return;
    if (r.error) {
      setMsg(`MXC mode change failed: ${r.error}`, true);
      modeBtn.disabled = false;
      return;
    }
    if (r.data.persisted === false) {
      setMsg(`MXC mode set to ${r.data.mode} for this session only \u2014 the change was not saved and will revert when the app restarts.`, true);
    } else {
      setMsg(`MXC mode set to ${r.data.mode}.`);
    }
    await load();
  };
  root.addEventListener("click", onClick);

  await load();

  return () => {
    disposed = true;
    root.removeEventListener("click", onClick);
  };
}

/* ---------------- Setup wizard ---------------- */

/** Quote one argument for PowerShell when it contains whitespace or
 * quotes; plain tokens pass through unquoted. */
function psQuoteArg(a: string): string {
  return a === "" || /[\s"]/.test(a) ? `"${a.replace(/"/g, '\\"')}"` : a;
}

/** Render a step's planned commands as a PowerShell block the user can
 * paste into a terminal to run the step manually. Paths in the planned
 * commands are absolute, so the block works from any directory.
 * Pseudo-commands are translated to their manual equivalents
 * (download → curl.exe; http-get/http-post → Invoke-RestMethod).
 * Secret env values are never printed: the gateway master key renders
 * as a self-generated value, which makes a manual gateway run
 * self-consistent (it will not match the app's stored key — that only
 * matters when the app itself probes the manual instance). */
function formatManualCommands(cmds: PlannedCommandWire[]): string {
  const lines: string[] = [];
  for (const c of cmds) {
    if (c.argv[0] === "download" && c.argv.length === 3) {
      lines.push(`curl.exe -L -o ${psQuoteArg(c.argv[2])} ${psQuoteArg(c.argv[1])}`);
      continue;
    }
    if (c.argv[0] === "http-get" && c.argv.length >= 2) {
      lines.push(`Invoke-RestMethod ${psQuoteArg(c.argv[1])}`);
      continue;
    }
    if (c.argv[0] === "http-post" && c.argv.length >= 2) {
      const body = c.argv[2] ? ` -ContentType "application/json" -Body '${c.argv[2].replace(/'/g, "''")}'` : "";
      lines.push(`Invoke-RestMethod -Method Post -Uri ${psQuoteArg(c.argv[1])}${body}`);
      continue;
    }
    if (c.cwd) lines.push(`Set-Location ${psQuoteArg(c.cwd)}`);
    for (const k of Object.keys(c.env).sort()) {
      const v = c.env[k];
      if (k === "LITELLM_MASTER_KEY" && v.includes("credential")) {
        lines.push(`$env:LITELLM_MASTER_KEY = "sk-local-" + [guid]::NewGuid().ToString("N")  # manual-run key; the app uses its stored Credential Manager key`);
      } else {
        lines.push(`$env:${k} = "${v.replace(/"/g, '\\"')}"`);
      }
    }
    const argv = c.argv.map(psQuoteArg).join(" ");
    const needsCall = c.argv.length > 0 && /[\\/]/.test(c.argv[0]);
    lines.push(needsCall ? `& ${argv}` : argv);
    if (c.stdout_log) {
      lines.push(`# the app captures this command's output to ${c.stdout_log}${c.stderr_log ? ` and ${c.stderr_log}` : ""}`);
    }
  }
  return lines.join("\n");
}

function wizardStepRow(s: WizardStepState, live: boolean): string {
  const stIcon = s.state === "done" || s.state === "manual" ? `<span style="color:var(--ok)">${icon("check")}</span>`
    : s.state === "running" ? `<span style="color:var(--accent)">${icon("clock")}</span>`
    : s.state === "failed" ? `<span style="color:var(--err)">${icon("err")}</span>`
    : `<span style="color:var(--text3)">\u25cb</span>`;
  const stateTxt = s.state === "done" ? `done \u00b7 ${esc(s.duration ?? "")}`
    : s.state === "manual" ? "manual"
    : s.state === "running" ? esc(s.note ?? "running\u2026")
    : esc(s.state);
  const helpBtn = s.state === "failed" && s.helpUrl
    ? `<button class="btn" data-help-url="${esc(s.helpUrl)}">Open driver download</button>` : "";
  const runBtn = live && (s.state === "queued" || s.state === "failed")
    ? `<button class="btn" data-run-step="${esc(s.id)}">${s.state === "failed" ? "Retry" : "Run"}</button>` : "";
  const actions = helpBtn || runBtn ? `<div style="margin-top:6px;display:flex;gap:6px">${helpBtn}${runBtn}</div>` : "";
  const remediation = s.state === "failed" && s.remediation
    ? `<div style="margin-top:4px;color:var(--text2)">${esc(s.remediation)}</div>` : "";
  const logTail = s.state === "failed" && s.logTail && s.logTail.length
    ? `<div style="margin-top:4px;font-family:var(--mono);font-size:11px;color:var(--text3)">${s.logTail.slice(-3).map((l) => esc(l)).join("<br>")}</div>` : "";
  // The outcome's own manual block wins over the generic planned
  // commands: it is built for this install and this failure (real
  // paths, the PostgreSQL pre-step when installed, the secret env
  // named), and its first line already says what it is.
  const manualText = s.state === "failed" ? (s.manualCommand ?? s.manualCommands) : undefined;
  const manual = manualText
    ? `<div style="margin-top:8px">${s.manualCommand ? "" : `<div style="font-size:11px;color:var(--text3)">Run this step manually (PowerShell):</div>`}
      <pre style="margin:4px 0;font-family:var(--mono);font-size:11px;white-space:pre-wrap;word-break:break-all;background:var(--panel);border:1px solid var(--border);border-radius:6px;padding:8px">${esc(manualText)}</pre>
      <button class="btn" data-copy-step="${esc(s.id)}">Copy commands</button></div>` : "";
  return `<div class="step ${s.state}">${stIcon}${icon("wrench")}
    <div><div class="sname">${esc(s.name)}</div><div class="sdetail">${esc(s.detail)}</div>${remediation}${logTail}${manual}${actions}</div>
    <div class="sstate">${s.note && s.state === "done" ? `<span class="chip badge-warn">${esc(s.note)}</span> ` : ""}${stateTxt}</div></div>`;
}

interface WizardPaintOpts {
  busy?: boolean;
  runningStepId?: string | null;
  statusLine?: string | null;
  /** A run or audit is in progress: steps without a result from
   *  this run (freshIds) render blank instead of the previous
   *  session's success/failure. */
  hideStale?: boolean;
  freshIds?: Set<string>;
}

function paintWizard(root: HTMLElement, steps: WizardStepState[], live: boolean, opts: WizardPaintOpts = {}): void {
  // The backend's Running state cannot be observed mid-step (the run
  // command holds the wizard lock for the whole execution, and the
  // state query needs the same lock), so the view marks the step it
  // invoked as running itself. While a step runs, its box must not
  // keep showing the previous session's result: it renders blank,
  // labeled "running", in the blue running style — whatever its prior
  // state was. During a full run or audit (hideStale), steps that
  // have not produced a result in this run render blank as well, so
  // no stale success or failure stays on screen while work is in
  // progress.
  const blank = (s: WizardStepState, state: WizardStepState["state"]): WizardStepState => ({
    ...s, state, detail: "", note: undefined, duration: undefined,
    remediation: undefined, logTail: undefined, manualCommands: undefined, manualCommand: undefined,
  });
  const eff = steps.map((s) => {
    if (opts.runningStepId && s.id === opts.runningStepId) return blank(s, "running");
    if (opts.hideStale && !(opts.freshIds && opts.freshIds.has(s.id))) return blank(s, "queued");
    return s;
  });
  const done = eff.filter((s) => s.state === "done" || s.state === "manual").length;
  const pct = eff.length ? Math.round((done / eff.length) * 100) : 0;
  const progress = root.querySelector("#wizProgress");
  if (progress) {
    progress.innerHTML = `<b>${done}</b><span style="color:var(--text3);font-size:12px">of ${eff.length} steps \u00b7 ${pct}%</span>
      <div class="segbar">${eff.map((s) => `<i class="${s.state === "done" || s.state === "manual" ? "done" : s.state === "running" ? "run" : ""}"></i>`).join("")}</div>`
      + (opts.statusLine ? `<div style="margin-top:4px;font-size:12px;color:var(--text2)">${esc(opts.statusLine)}</div>` : "");
  }
  const list = root.querySelector("#wizSteps");
  if (list) {
    list.innerHTML = `<div class="sec-title">Setup checklist <span style="color:var(--text3);font-weight:400">\u2014 runs in order</span></div>`
      + eff.map((s) => wizardStepRow(s, live)).join("");
  }
  const banner = root.querySelector("#wizBanner");
  if (banner) {
    banner.innerHTML = live
      ? `${icon("check")} Live \u2014 steps execute against this machine. A failed step pauses the run; fix the cause and retry it.`
      : `${icon("warn")} Sample mode \u2014 open the installed app to run setup against this machine.`;
  }
  // Busy state is painted, not remembered per element: every control
  // that can start work is disabled while a run is in flight, so the
  // state survives repaints and cannot be bypassed by clicking a
  // per-step button during a full run.
  const busy = !!opts.busy;
  for (const sel of ["#runBtn", "#auditBtn", "#finishBtn"]) {
    const b = root.querySelector(sel) as HTMLButtonElement | null;
    if (b) b.disabled = busy;
  }
  root.querySelectorAll("[data-run-step]").forEach((b) => { (b as HTMLButtonElement).disabled = busy; });
}

export function renderWizard(): string {
  const live = isTauri();
  return `<div class="topbar"><h1>Setup</h1><span class="spacer"></span><a class="btn" href="#/dashboard">Back to dashboard</a></div>
  <div class="view"><div class="banner" id="wizBanner">${icon("clock")} Loading setup state\u2026</div>
  <div class="wiz">
    <div class="wiz-head">${icon("wrench")}<h2>Set up Local LLM Service</h2><span class="spacer" style="flex:1"></span>
      <button class="btn" id="auditBtn">${icon("search")} Re-run audit</button>
      <button class="btn primary" id="runBtn">Run setup</button></div>
    <div class="wiz-progress" id="wizProgress"></div>
    <div class="wiz-body">
      <div class="wiz-side"><p>Steps run in order. A failed step pauses the run \u2014 retry it to continue.</p>
        <div style="margin-top:14px;display:grid;gap:6px">
          <div>${icon("shield")} checksums verified per file</div>
          <div>${icon("server")} the gateway starts at its step; backends start before the smoke test</div></div></div>
      <div class="wiz-steps" id="wizSteps"></div>
    </div>
    <div class="wiz-foot"><button class="btn" disabled>Back</button><span class="spacer"></span>
      <button class="btn primary" id="finishBtn">Finish</button></div>
    <div class="logstream" id="wizLog" style="border:0;border-top:1px solid var(--border);border-radius:0"></div>
  </div></div>`;
}

export function wireWizard(root: HTMLElement): () => void {
  // The root click listener below is delegated on the persistent #view
  // element, so it must be removed on teardown — every other wire*
  // function returns its cleanup for the same reason.
  let disposed = false;
  const logEl = root.querySelector("#wizLog");
  const logLine = (text: string, cls = "lvl-info") => {
    if (!logEl) return;
    const t = new Date().toLocaleTimeString("en-GB");
    logEl.insertAdjacentHTML("beforeend", `<div class="ln"><span class="t">${t}</span> <span class="${cls}">${esc(text)}</span></div>`);
  };

  let steps: WizardStepState[] = [];
  let liveNow = false;
  let busy = false;
  let runningStepId: string | null = null;
  let statusLine: string | null = null;
  /** The most recent busy report's message (which step is running
   * and for how long), for the status line after a busy stop. */
  let busyMsg = "";
  /** While a setup run or audit is in progress, steps that have not
   *  produced a result in this run render blank (paintWizard's
   *  hideStale); freshIds collects the steps that have. */
  let hideStale = false;
  const freshIds = new Set<string>();

  const paint = (): void => paintWizard(root, steps, liveNow, { busy, runningStepId, statusLine, hideStale, freshIds });

  /** Manual-command text per failed step id, fetched once per step
   * from setup_get_commands and rendered by the row with a copy
   * button. An empty string marks a step whose commands could not be
   * loaded, so it is not refetched on every paint. */
  const manualCmds = new Map<string, string>();
  const enrichFailed = async (): Promise<void> => {
    if (!liveNow) return;
    let changed = false;
    for (const s of steps) {
      if (s.state !== "failed") continue;
      // The outcome's own manual block (start-gateway) is specific
      // to the failure; the planned commands would only duplicate it.
      if (s.manualCommand) continue;
      const cached = manualCmds.get(s.id);
      if (cached !== undefined) {
        if (cached && s.manualCommands !== cached) { s.manualCommands = cached; changed = true; }
        continue;
      }
      const r = await getWizardCommands(s.id);
      if (disposed) return;
      if (!r.error && r.data.length) {
        const text = formatManualCommands(r.data);
        manualCmds.set(s.id, text);
        s.manualCommands = text;
        changed = true;
      } else {
        manualCmds.set(s.id, "");
      }
    }
    if (changed) paint();
  };

  const refresh = async (): Promise<void> => {
    const r = await getWizardSteps();
    if (disposed) return;
    steps = r.data;
    liveNow = r.live;
    paint();
    if (r.error) logLine(`could not load setup state: ${r.error}`, "lvl-err");
    void enrichFailed();
  };

  /** The wizard's steps run in the wizard layer, which has no handle
   *  to the backend supervisor — only the gateway has a wizard step.
   *  Before the smoke test, start every enabled backend that is not
   *  already running: the smoke test probes through the gateway to
   *  the backends, so they must be up (the step itself then waits for
   *  their health endpoints while the models load). A backend that
   *  fails to start is logged; the smoke test reports the consequence. */
  const startEnabledBackends = async (): Promise<void> => {
    const r = await getBackends();
    if (r.error || !r.data) { logLine(`could not list backends before the smoke test: ${r.error ?? "no data"}`, "lvl-warn"); return; }
    for (const b of r.data) {
      if (!b.configured || !b.enabled) continue;
      if (b.state === "running" || b.state === "starting") continue;
      logLine(`starting backend: ${b.id}\u2026`);
      const sr = await startBackend(b.id);
      if (sr.error) logLine(`backend ${b.id} did not start: ${sr.error}`, "lvl-err");
      else logLine(`backend ${b.id} starting`);
    }
  };

  const runOne = async (stepId: string): Promise<"ok" | "failed" | "busy"> => {
    if (stepId === "smoke-test") await startEnabledBackends();
    logLine(`running step: ${stepId}`);
    const r = await runWizardStep(stepId);
    if (r.error) { freshIds.add(stepId); logLine(`step ${stepId} error: ${r.error}`, "lvl-err"); await refresh(); return "failed"; }
    if (r.data.busy || !r.data.outcome) {
      busyMsg = wizardBusyMessage(r.data, "A setup step is already running — wait for it to finish.");
      logLine(busyMsg, "lvl-warn");
      await refresh();
      return "busy";
    }
    const o = r.data.outcome;
    freshIds.add(stepId);
    logLine(`step ${stepId}: ${o.state} \u2014 ${o.message}`, o.state === "done" || o.state === "manual" ? "lvl-info" : "lvl-err");
    await refresh();
    return o.state === "done" || o.state === "manual" || o.state === "skipped" ? "ok" : "failed";
  };

  const runBtn = root.querySelector("#runBtn") as HTMLButtonElement | null;
  runBtn?.addEventListener("click", async () => {
    if (!isTauri()) { alert("Sample mode: setup runs in the installed app."); return; }
    if (busy) return;
    busy = true;
    hideStale = true;
    freshIds.clear();
    paint();
    try {
      await refresh();
      // This run skips already-complete steps entirely, so their
      // state is final rather than stale: seed freshIds with them so
      // paintWizard keeps rendering them as done instead of blanking
      // them to "queued" for the whole run (which read as the run
      // resetting every completed step). Only steps that will
      // actually execute blank until they produce a result.
      for (const s of steps) {
        if (s.state === "done" || s.state === "manual" || s.state === "skipped") freshIds.add(s.id);
      }
      let paused: string | null = null;
      let busyStop = false;
      for (const s of [...steps]) {
        if (s.state === "done" || s.state === "manual" || s.state === "skipped") continue;
        runningStepId = s.id;
        statusLine = `Running step: ${s.name}\u2026`;
        paint();
        const res = await runOne(s.id);
        runningStepId = null;
        if (res === "busy") { busyStop = true; break; }
        if (res !== "ok") { paused = s.name; break; }
      }
      if (busyStop) {
        statusLine = busyMsg || "A setup step is already running \u2014 wait for it to finish.";
      } else if (paused) {
        statusLine = `Setup paused \u2014 ${paused} failed. Fix the cause and retry.`;
        logLine("setup paused \u2014 fix the failed step and retry", "lvl-warn");
      } else {
        statusLine = "Setup run finished.";
      }
    } finally {
      busy = false;
      runningStepId = null;
      hideStale = false;
      paint();
    }
  });

  const audit = root.querySelector("#auditBtn");
  audit?.addEventListener("click", async () => {
    if (!isTauri()) { alert("Sample mode: audit would re-run all checks here."); return; }
    if (busy) return;
    busy = true;
    hideStale = true;
    freshIds.clear();
    statusLine = "Running audit \u2014 re-checking all steps\u2026";
    paint();
    logLine("running audit\u2026");
    try {
      const r = await runAudit();
      if (r.error) {
        statusLine = `Audit failed: ${r.error}`;
        logLine(`audit failed: ${r.error}`, "lvl-err");
        await refresh();
        return;
      }
      if (r.data.already_running) {
        // Not a failure: an audit (often the launch-time hydration
        // audit) is already in flight. Its results land in the
        // checklist when it finishes.
        statusLine = "Audit already running \u2014 its results will appear in the checklist when it finishes.";
        logLine("audit already running");
        return;
      }
      // Paint the audit's own per-step results: refresh() would reload
      // the install wizard's state, which an audit never changes — that
      // is why the old handler appeared to do nothing.
      steps = r.data.steps;
      statusLine = `Audit complete: ${r.data.done}/${r.data.total} checks passed \u2014 the checklist shows the audit results.`;
      logLine(`audit complete: ${r.data.done}/${r.data.total} checks passed`);
      void enrichFailed();
    } finally {
      busy = false;
      hideStale = false;
      paint();
    }
  });

  root.querySelector("#finishBtn")?.addEventListener("click", async () => {
    if (busy) return;
    if (isTauri()) {
      const remaining = steps.filter((s) => !(s.state === "done" || s.state === "manual" || s.state === "skipped")).length;
      if (remaining > 0 && !confirm(`${remaining} setup step(s) are not complete. Mark setup complete anyway?`)) return;
      const r = await markSetupComplete();
      if (r.error) { alert(`Could not mark setup complete: ${r.error}`); return; }
    }
    location.hash = "#/dashboard";
  });

  const onRootClick = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLElement;
    const helpBtn = t.closest("[data-help-url]") as HTMLElement | null;
    if (helpBtn) {
      const url = helpBtn.getAttribute("data-help-url") ?? "";
      helpBtn.setAttribute("disabled", "");
      const r = await openUrl(url);
      helpBtn.removeAttribute("disabled");
      if (r.error) alert(`Could not open the download page: ${r.error}`);
      return;
    }
    const copyBtn = t.closest("[data-copy-step]") as HTMLElement | null;
    if (copyBtn) {
      const id = copyBtn.getAttribute("data-copy-step") ?? "";
      const step = steps.find((s) => s.id === id);
      const text = step?.manualCommand ?? step?.manualCommands ?? "";
      if (text) {
        try {
          await navigator.clipboard.writeText(text);
        } catch {
          // Clipboard API unavailable in this webview context: fall
          // back to a temporary textarea + execCommand.
          const ta = document.createElement("textarea");
          ta.value = text;
          document.body.appendChild(ta);
          ta.select();
          document.execCommand("copy");
          ta.remove();
        }
        statusLine = "Commands copied to the clipboard.";
        paint();
      }
      return;
    }
    const runStepBtn = t.closest("[data-run-step]") as HTMLElement | null;
    if (runStepBtn) {
      if (busy) return;
      const id = runStepBtn.getAttribute("data-run-step") ?? "";
      busy = true;
      runningStepId = id;
      statusLine = `Running step: ${steps.find((s) => s.id === id)?.name ?? id}\u2026`;
      paint();
      let res: "ok" | "failed" | "busy" = "failed";
      try {
        res = await runOne(id);
      } finally {
        busy = false;
        runningStepId = null;
        statusLine = res === "busy" ? (busyMsg || "A setup step is already running \u2014 wait for it to finish.") : null;
        paint();
      }
    }
  };
  root.addEventListener("click", onRootClick);

  void refresh();

  return () => {
    disposed = true;
    root.removeEventListener("click", onRootClick);
  };
}
