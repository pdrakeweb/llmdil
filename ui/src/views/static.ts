/* Secondary views — all wired to the backend through api.ts.
   Each view follows the Setup wizard pattern: renderX() returns a shell
   with a loading placeholder; wireX(root) loads live data, paints it, and
   attaches handlers. Under Tauri the data is real; in a plain browser
   (live=false) views show a sample-mode banner and honest empty states
   instead of fabricated data. */

import { fmtCount, fmtGb, gaugeRing, meter, sparkline, stackedBar, trim1 } from "../charts";
import { icon, type IconName } from "../icons";
import { esc } from "../html";
import { enableSwitch } from "../switch";
import {
  runWizardStep, wizardBusyMessage, isTauri, openUrl, markSetupComplete, getWizardSteps, runAudit,
  getWizardCommands,
  getGpuTelemetry, setTelemetryInterval, resetTelemetryPeaks, saveTelemetrySnapshot,
  getBackends, startBackend, stopBackend, restartBackend, updateBackendFlags, setBackendOffload, setBackendMtp, setBackendParallel, setBackendRawFlags, setBackendFlagsMode, getBackendLaunchCommand,
  measurementLookup,
  getBackendConfig, applyBackendConfig, testBackend, testBackendToolCall, placementPropose, placementApply,
  addBackend, backendAddPreview, deleteBackend, modelAssign,
  getModels, downloadModel, cancelDownload, pauseModelDownload, resumeModelDownload,
  verifyModel, deleteModel, modelSetEnabled, modelAdd, modelSetShortName,
  getLogs, exportDiagnostics, openFolder, getGatewayConfig, getGatewayStatus, getGatewayYaml,
  getGatewayMasterKey, setGatewayBindHost,
  updateGatewayConfig, effectiveGatewayStrategy, gatewayStop,
  getActivationPlan, activationCancelChanges, activationComplete, quiesceWait,
  recordActivationExecution, getActivationHistory, getActivationExecution,
  getAppConfig, getMxcPolicy, getMxcActivity, setMxcMode, mxcRunSelfTest,
  setNotificationConfig, sendTestNotification,
  getAppUpdateStatus, appUpdateCheck, appUpdateSetChecking, appUpdateSetAuto, appUpdateDownload,
  servicesList, servicesRegister, servicesUnregister, setSupervisionMode,
  adminApiGetStatus, adminApiSetEnabled, adminApiListUsers, adminApiCreateUser,
  adminApiUpdateUser, adminApiDeleteUser, adminApiResetPassword, adminApiCreateEnrollment,
  adminApiRegenerateCert, adminApiGetAudit,
} from "../api";
import type { ActivationChange, ActivationExecution, ActivationExecutionItem, ActivationExecutionSummary, ActivationPlanData, AdminApiAuditEntry, AdminApiPairing, AdminApiStatus, AdminApiUser, AppUpdateInfo, BackendAddPreview, BackendMtp, BackendOffload, BackendParallel, BackendRow, GatewayConfigData, GatewayGroup, GatewayRule, GatewayRuleKind, GatewayStatusData, LogRow, MeasurementLookupRequest, ModelRow, MxcActivityData, MxcPolicyData, PlacementProjectionData, PlannedCommandWire, PredictedThroughput, ServicesInfo } from "../api";
import { promptForUnarmedMcpCredentials } from "../mcp-prompt";
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
  const down = g.status === "down";
  // pci/driver are absent from the telemetry payload; omit them when
  // unset (esc(undefined) would print the literal text "undefined").
  const pci = g.pci ? ` <span class="port" style="color:var(--text3);font-family:var(--mono);font-size:11px">${esc(g.pci)}</span>` : "";
  const driver = g.driver ? `<span style="color:var(--text3);font-size:11px;font-family:var(--mono)">${esc(g.driver)}</span>` : "";
  // Stale is a caution state, not a transition — neutral chip, the
  // explanation in its tooltip (principle 11). A down GPU keeps its
  // light-red chip; its two-sentence explanation moves into the
  // chip's tooltip and the body renders de-emphasized below.
  const staleChip = g.status === "degraded" ? ` <span class="chip" title="Telemetry for this GPU is stale — readings are the last reported values.">stale</span>` : "";
  const downChip = down ? ` <span class="chip badge-err" title="Down — not reported by the telemetry source in recent polls; the readings shown are the last known values. A GPU that has fallen off the bus (nvidia-smi reports it as lost) recovers only on a system reboot.">down</span>` : "";
  const peakTip = g.temp_peak_c !== undefined ? ` title="Peak ${trim1(g.temp_peak_c)} \u00b0C since last reset or app start"` : "";
  const memUtil = g.mem_util_pct !== undefined
    ? `<div class="mrow"><span class="mlabel">Memory util</span><span class="mval">${g.mem_util_pct}%</span></div>` : "";
  const fan = g.fan_pct !== undefined
    ? `<div class="mrow"><span class="mlabel">Fan</span><span class="mval">${g.fan_pct}%</span></div>` : "";
  const tempSpark = g.temp_spark && g.temp_spark.length
    ? `<div class="mrow"><span></span>${sparkline(g.temp_spark, 240, 26, C.amber)}</div>` : "";
  const powerSpark = g.power_w != null && g.power_spark && g.power_spark.length
    ? `<div class="mrow"><span></span>${sparkline(g.power_spark, 240, 26, C.green)}</div>` : "";
  /* ---- VRAM residency bar: the card's prime visual. One segment
   * per resident process, width = its measured share of total
   * VRAM; the unattributed remainder (used - measured) is a neutral
   * "other" segment; free space is the unfilled track. WDDM rule
   * (principle 12): if ANY resident process is unmeasured, no
   * segment is drawn from invented data — the bar is one neutral
   * segment for total used, keyed as unmeasured. A total of 0 means
   * the total is unknown: no bar and no "0 / 0 GiB" — the value
   * renders as "not measured". ---- */
  const vramHtml = (() => {
    if (!(g.vram_total_gb > 0)) {
      return `<div style="font-size:12px;color:var(--text2);margin-top:10px">VRAM: not measured</div>`;
    }
    const totalMiB = g.vram_total_gb * 1024;
    const usedMiB = g.vram_used_gb * 1024;
    const procs = g.processes ?? [];
    const palette = [C.blue, C.purple, C.teal, C.amber, C.green];
    let segs: { pct: number; color: string; label: string }[];
    let keys: string;
    if (procs.some((p) => p.vram_mib === null)) {
      segs = [{ pct: (usedMiB / totalMiB) * 100, color: "#3a4350", label: "VRAM used — per-process breakdown not measured" }];
      keys = `<span style="color:var(--text3)">VRAM used — per-process breakdown not measured</span>`;
    } else {
      let measuredSum = 0;
      segs = procs.map((p, i) => {
        const mib = p.vram_mib ?? 0;
        measuredSum += mib;
        const name = p.backend !== "—" ? p.backend : baseName(p.process);
        const ctxNote = p.backend !== "—" && mib < 1024 ? " — context only: under 1 GiB, this process holds only its CUDA context on this GPU, not model weights" : "";
        return { pct: (mib / totalMiB) * 100, color: palette[i % palette.length], label: `${name} — ${fmtGb(mib / 1024)} GiB${ctxNote}` };
      });
      const rest = usedMiB - measuredSum;
      if (rest > 0) segs.push({ pct: (rest / totalMiB) * 100, color: "#3a4350", label: `other — ${fmtGb(rest / 1024)} GiB` });
      keys = segs.map((s, i) => `<span><span class="sw" style="background:${s.color}"></span>${esc(s.label.replace(/ — .*$/, ""))}</span>`).join("");
      keys = `<div class="legend" style="margin-top:4px">${keys}</div>`;
    }
    return `<div style="margin-top:10px">
      <div style="display:flex;justify-content:space-between;align-items:baseline"><span style="font-size:12px;color:var(--text2)">VRAM residency</span><span style="font-size:13px">VRAM: ${fmtGb(g.vram_used_gb)} / ${fmtGb(g.vram_total_gb)} GiB${down ? " \u00b7 last known" : ""}</span></div>
      <div style="margin-top:4px">${stackedBar(segs, 600, 12)}</div>
      ${keys}
    </div>`;
  })();
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
      <span style="font-size:12px;font-family:var(--mono)">${measured ? `${(p.vram_mib as number).toLocaleString()} MiB` : "not measured"}</span></div>`;
    const detail = open ? `<div style="padding:6px 6px 8px 24px;font-size:11px;color:var(--text2);display:grid;gap:2px;border-bottom:1px solid #232a34">
      <div>Process: ${esc(p.process)}</div>
      <div>PID: ${p.pid}</div>
      <div>VRAM on GPU ${esc(g.id)}: ${measured ? `${(p.vram_mib as number).toLocaleString()} MiB` : "not measured — per-process accounting is unavailable for this GPU"}</div>
      <div>Backend: ${p.backend !== "—" ? esc(p.backend) : "not attributed to a backend"}</div>
      ${p.backend !== "—" && p.vram_mib !== null && p.vram_mib < 1024 ? `<div>Under 1 GiB: this process holds only its CUDA context on this GPU, not model weights.</div>` : ""}
    </div>` : "";
    return head + detail;
  };
  // Processes render inside a collapsed expandable (the expand
  // affordance is the instruction — no "click a row" text); the
  // per-row detail expandables inside are unchanged.
  const procsKey = `procs:${g.id}`;
  const procsOpen = expanded.has(procsKey);
  const procs = g.processes && g.processes.length
    ? `<div style="margin-top:10px"><div data-proc-toggle="${esc(procsKey)}" role="button" tabindex="0" style="font-size:11px;color:var(--text3);margin-bottom:4px;cursor:pointer"><span style="font-size:10px">${procsOpen ? "\u25be" : "\u25b8"}</span> PROCESSES (${g.processes.length})</div>
      ${procsOpen ? `<div style="max-height:176px;overflow-y:auto;border:1px solid #232a34;border-radius:6px">${g.processes.map(procRow).join("")}</div>` : ""}</div>` : "";
  // Resident models (live mode) sit ABOVE Processes — this is the
  // placement answer for the view. Each row is a label line plus a
  // mini share bar (width = configured pct) with the held figure at
  // the right, or "not measured" when the backend's VRAM here is
  // unmeasured. Sample-mode rows carry no pct/mib — label line only.
  const models = modelRows.length
    ? `<div style="margin-top:10px"><div style="font-size:11px;color:var(--text3);margin-bottom:4px">MODELS ON THIS GPU</div>
        <div style="display:grid;gap:6px">${modelRows.map((r) => `<div>
          <div style="font-size:12px">${icon("layers")} Model: ${esc(r.model)} <span style="color:var(--text3)">via ${esc(r.backend)}</span></div>
          <div style="display:flex;gap:10px;align-items:center;margin-top:2px">
            <span style="flex:1">${stackedBar([{ pct: r.pct, color: C.purple, label: `Share: ${r.pct}%` }], 300, 6)}</span>
            <span style="font-size:12px;color:var(--text2)">Share: ${r.pct}%</span>
            <span style="font-size:12px">${r.mib === null ? "not measured" : `${fmtGb(r.mib / 1024)} GiB held`}</span>
          </div></div>`).join("")}</div></div>`
    : g.models && g.models.length
    ? `<div style="margin-top:8px;font-size:11px;color:var(--text3);display:grid;gap:4px">
        ${g.models.map((m) => `<div>${icon("layers")} ${esc(m.model)} \u00b7 ${esc(m.layers)}</div>`).join("")}
      </div>` : "";
  // Power null -> the tile is ABSENT (principle 13), never "N/A".
  const powerRow = g.power_w != null
    ? `<div class="mrow"><span class="mlabel">Power</span><span class="mval"${g.power_limit_w !== undefined ? ` title="Limit ${trim1(g.power_limit_w)} W"` : ""}>${trim1(g.power_w)} W</span></div>${powerSpark}` : "";
  return `<div class="card" id="card-gpu-${esc(g.id)}">
      <div class="gpu-head">${dot(g.status === "down" ? "err" : g.status)} ${esc(g.name)}${pci}${staleChip}${downChip}
        <span class="spacer" style="flex:1"></span>${driver}</div>
      <div${down ? ` style="opacity:.65"` : ""}>
      ${vramHtml}
      <div style="display:flex;gap:18px;justify-content:center;margin:12px 0">
        ${gaugeRing(g.util_pct, 84, C.blue, String(g.util_pct), "UTIL")}
      </div>
      <div class="gpu-metrics">
        <div class="mrow"><span class="mlabel">Temperature</span><span class="mval"${peakTip}>${trim1(g.temp_c)} \u00b0C</span></div>
        ${tempSpark}
        ${powerRow}
        ${memUtil}
        ${fan}
      </div>
      ${models}
      ${procs}
      </div>
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
    <button class="btn icon-only" id="gpuRefresh" title="Refresh" aria-label="Refresh">${icon("restart")}</button>
    <button class="btn icon-only" id="gpuResetPeaks" title="Reset peaks" aria-label="Reset peaks">${icon("peak")}</button>
    <button class="btn icon-only" id="gpuSnapshot" title="Snapshot" aria-label="Save telemetry snapshot">${icon("download")}</button>`)}
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
 * the block's own first line says what it is, so it renders bare.
 * The commands are a value the operator must run by hand, so the
 * block carries a Copy button (principle 10) — word-break:break-all
 * makes manual selection error-prone. The button routes through the
 * plan surfaces' shared click handling (data-plan-act). */
function manualCommandHtml(cmd: string): string {
  return `<pre style="margin:6px 0;font-family:var(--mono);font-size:11px;white-space:pre-wrap;word-break:break-all;background:var(--panel);border:1px solid var(--border);border-radius:6px;padding:8px;text-align:left">${esc(cmd)}</pre>
  <div><button class="btn icon-only" data-plan-act="copy-manual" title="Copy commands" aria-label="Copy commands">${icon("copy")}</button></div>`;
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

/** Join backend names the way the plan copy lists them:
 * "coder", "coder and planner", "coder, planner and coder-fast". */
function planNameList(names: string[]): string {
  if (names.length <= 1) return names.join("");
  return `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;
}

/** The plan's consequence line, composed from the pending journal
 * entries instead of static boilerplate: Apply restarts the
 * running backends the plan names (backend_restart_ids — the same
 * list applyActivationPlan restarts), then the gateway ONLY when a
 * gateway-scoped change is pending (a stopped gateway is started,
 * not restarted); targets whose process is stopped are named as
 * taking effect at their next start, and when nothing affected is
 * running the line says so. A gateway (re)start is always named
 * with what it can involve: the start-gateway step verifies the
 * gateway's install first and repairs it when a check fails
 * (venv, package install, Prisma generation), so the line says so
 * — an operator must never learn that a plan restarted (let alone
 * reinstalled) the gateway only by watching it happen (Pete's
 * report, 2026-10-18). Exported for the app shell's activation
 * banner, which carries the same line. Every surface renders this
 * line as visible text next to the Apply actions, not only in the
 * buttons' tooltip. */
export function planApplyLine(plan: ActivationPlanData): string {
  const restarting = plan.backend_restart_ids ?? [];
  const backendTargets: string[] = [];
  for (const c of plan.changes) {
    if (c.scope !== "backend") continue;
    for (const t of c.targets ?? []) {
      if (!backendTargets.includes(t)) backendTargets.push(t);
    }
  }
  const stopped = backendTargets.filter((t) => !restarting.includes(t));
  if (!restarting.length && !plan.gateway_pending) {
    const names = planNameList(backendTargets);
    return names
      ? `Nothing running holds the old state — these changes take effect at the next start of ${names}.`
      : "Nothing running holds the old state — these changes take effect at the next start of the affected process.";
  }
  let lead: string;
  if (restarting.length && plan.gateway_pending) {
    lead = `Apply restarts ${planNameList(restarting)}, then ${plan.gateway_up ? "the gateway" : "starts the gateway"}`;
  } else if (restarting.length) {
    lead = `Apply restarts ${planNameList(restarting)}`;
  } else {
    lead = plan.gateway_up ? "Apply restarts the gateway" : "Apply starts the gateway";
  }
  const gatewayNote = plan.gateway_pending
    ? " The gateway start verifies its install first and repairs or reinstalls it if a check fails."
    : "";
  const tail = !stopped.length
    ? ""
    : stopped.length === 1
      ? ` The change to ${stopped[0]} takes effect at its next start.`
      : ` Changes to ${planNameList(stopped)} take effect at their next start.`;
  return `${lead}.${gatewayNote}${tail}`;
}

/** The plan's aggregate counts, each computed once and rendered in
 * exactly one place per surface: changes split by scope (backend vs
 * gateway) and by activation class (restart required vs next start —
 * a change is "next start" exactly when its activation says so;
 * every other class needs a restart of something running). */
function planCounts(plan: ActivationPlanData): { backend: number; gateway: number; restart: number; nextStart: number } {
  const backend = plan.changes.filter((c) => c.scope === "backend").length;
  const nextStart = plan.changes.filter((c) => c.activation === "next_start").length;
  return {
    backend,
    gateway: plan.changes.length - backend,
    restart: plan.changes.length - nextStart,
    nextStart,
  };
}

/** Scope composition as a mini segmented bar: backend segment vs
 * gateway segment, widths proportional to the per-scope counts, the
 * counts in the segments' tooltips and the names as adjacent keys.
 * The drift-only state (no journal changes) is a single gray
 * segment keyed "Gateway drift". */
function planScopeBarHtml(plan: ActivationPlanData): string {
  const n = plan.changes.length;
  if (!n) {
    return `<span style="display:inline-flex;align-items:center;gap:8px;flex:0 1 260px;min-width:150px">${stackedBar([{ pct: 100, color: "#6d7686", label: "Gateway drift" }], 260, 8)}<span style="font-size:11px;color:var(--text3);white-space:nowrap">Gateway drift</span></span>`;
  }
  const { backend, gateway } = planCounts(plan);
  const segs: { pct: number; color: string; label?: string }[] = [];
  if (backend) segs.push({ pct: (backend / n) * 100, color: C.blue, label: `Backend: ${backend}` });
  if (gateway) segs.push({ pct: (gateway / n) * 100, color: C.purple, label: `Gateway: ${gateway}` });
  const keys = [
    backend ? `<span><span class="sw" style="background:${C.blue}"></span>Backend</span>` : "",
    gateway ? `<span><span class="sw" style="background:${C.purple}"></span>Gateway</span>` : "",
  ].join("");
  return `<span style="display:inline-flex;align-items:center;gap:8px;flex:0 1 340px;min-width:180px">${stackedBar(segs, 340, 8)}<span class="legend" style="margin-top:0;flex-wrap:nowrap">${keys}</span></span>`;
}

/** The Apply buttons shared by both plan variants, with the full
 * view's mode semantics: drain-then-restart or restart-now when the
 * gateway is up with a gateway change pending, else the single
 * Apply plan (start) action. The composed consequence line
 * (planApplyLine) rides the buttons' tooltip AND renders as visible
 * text beside the actions on every surface: what an apply will do
 * — above all a gateway restart and its possible install repair —
 * is a consequence the operator must see before clicking, not
 * explanation that can wait for a hover (Pete's requirement,
 * 2026-10-18; the visible rendering lives in activationPlanHtml
 * and the shell banner). */
function planApplyButtons(plan: ActivationPlanData, ui: PlanUiState): string {
  if (!plan.actionable) return "";
  const dis = ui.acting ? " disabled" : "";
  const tip = esc(planApplyLine(plan));
  return plan.gateway_pending && plan.gateway_up
    ? `<button class="btn" data-plan-act="quiesce" title="${tip}"${dis}>${icon("pause")} Drain Requests and Apply</button>
       <button class="btn" data-plan-act="restart" title="${tip}"${dis}>${icon("restart")} Apply now</button>`
    : `<button class="btn" data-plan-act="start" title="${tip}"${dis}>${icon("play")} Apply plan</button>`;
}

function planStatusHtml(ui: PlanUiState): string {
  return `${ui.status ? `<div style="font-size:12px;margin-top:6px;color:${ui.statusErr ? "var(--err)" : "var(--text2)"}">${esc(ui.status)}</div>` : ""}
    ${ui.manualCommand ? manualCommandHtml(ui.manualCommand) : ""}`;
}

/** The activation plan card, in two variants of one component:
 * "summary" — one line (warn icon, the pending count as Label:
 * Value, the scope bar, Review + Apply) for the Backends and
 * Gateway views and for the dismissed state everywhere; "full" —
 * the Activation Plan view's summary header (count tiles + scope
 * bar + actions), whose per-change detail is the Pending changes
 * table rendered by wireActivationPlan below it. The card never
 * lists the changes itself: the table is the one detail view. */
function activationPlanHtml(plan: ActivationPlanData | null, ui: PlanUiState, variant: "summary" | "full" = "full"): string {
  if (!plan || !plan.pending) return "";
  const n = plan.changes.length;
  const dis = ui.acting ? " disabled" : "";
  if (variant === "summary" || ui.dismissed) {
    return `<div class="card" style="margin-bottom:12px;border-color:var(--amber)">
    <div style="display:flex;align-items:center;gap:10px;flex-wrap:wrap;font-size:12px">
      ${icon("warn")} <span>Pending changes: <b>${n}</b></span>
      ${planScopeBarHtml(plan)}
      <span class="spacer" style="flex:1"></span>
      ${ui.dismissed
        ? `<button class="btn" data-plan-act="review" title="Review activation plan"${dis}>${icon("eye")} Review</button>`
        : `<a class="btn" href="#/activation-plan">Review</a>`}
      ${planApplyButtons(plan, ui)}
    </div>
    ${plan.actionable ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">${esc(planApplyLine(plan))}</div>` : ""}
    ${planStatusHtml(ui)}
  </div>`;
  }
  const { restart, nextStart } = planCounts(plan);
  const tiles: [string, string][] = [
    ["Pending changes", String(n)],
    ["Restart required", String(restart)],
    ["Next start", String(nextStart)],
  ];
  const cancelBtn = plan.snapshot_available
    ? (ui.confirmCancel
      ? `<button class="btn" data-plan-act="cancel-confirm" title="Cancelling restores the configuration from before these changes and clears the plan."${dis}>${icon("ban")} Confirm cancel changes</button>`
      : `<button class="btn" data-plan-act="cancel"${dis}>${icon("ban")} Cancel changes</button>`)
    : "";
  return `<div class="card" style="margin-bottom:12px;border-color:var(--amber)">
    <h3>${icon("warn")} Activation plan <span class="spacer" style="flex:1"></span><button class="btn icon-only" data-plan-act="later" title="Dismiss" aria-label="Dismiss"${dis}>${icon("x")}</button></h3>
    <div style="display:grid;grid-template-columns:repeat(3,1fr);gap:10px;margin-top:10px">
      ${tiles.map(([k, v]) => `<div class="kv"><span class="k">${k}</span><span class="v" style="font-size:13px">${esc(v)}</span></div>`).join("")}
    </div>
    <div style="margin-top:10px">${planScopeBarHtml(plan)}</div>
    ${plan.drift ? `<div style="font-size:12px;color:var(--text2);margin-top:8px">Gateway config: <b style="color:var(--text)">drifted</b><span class="info-tip" title="The running gateway's configuration differs from the saved configuration.">${icon("info")}</span></div>` : ""}
    ${plan.actionable ? `<div style="font-size:12px;color:var(--text2);margin-top:8px">${esc(planApplyLine(plan))}</div>` : ""}
    <div class="card-actions">${planApplyButtons(plan, ui)}
      ${cancelBtn}
      <button class="btn icon-only" data-plan-refresh title="Refresh" aria-label="Refresh">${icon("restart")}</button></div>
    ${planStatusHtml(ui)}
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
    if (q.error) { setStatus(`Drain check failed: ${q.error}`, true); return false; }
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
 *  execution as the plan card and the Activation Plan view.
 *
 *  Every apply — completed, partially failed, or aborted — is
 *  recorded whole in the durable execution history (the Activation
 *  Plan view's Recent executions) through
 *  gateway_activation_record_execution: the plan as computed here,
 *  one item per change with its outcome. Items start as unattempted
 *  skips and each step overwrites the items it acts on, so an
 *  abort leaves an accurate trail with no cleanup pass. The record
 *  call's result is ignored: recording is best-effort and must
 *  never change what an apply does or reports. */
export async function applyActivationPlan(
  plan: ActivationPlanData,
  mode: "quiesce" | "restart" | "start",
  setStatus: (text: string, err?: boolean) => void,
  setManual?: (cmd: string | null) => void,
): Promise<boolean> {
  const startedAt = new Date().toISOString();
  const items: ActivationExecutionItem[] = plan.changes.map((c) => ({
    ts: c.ts,
    kind: c.kind,
    summary: c.summary,
    scope: c.scope ?? "",
    targets: c.targets ?? [],
    activation: c.activation ?? "",
    outcome: "skipped",
    detail: "Not attempted — the apply did not reach this item",
  }));
  let lastError = "";
  const trackedStatus = (text: string, err = false): void => {
    if (err) lastError = text;
    setStatus(text, err);
  };
  const finish = (status: string, ok: boolean): boolean => {
    void recordActivationExecution({
      started_at: startedAt,
      finished_at: new Date().toISOString(),
      status,
      source: "desktop",
      mode,
      drift: plan.drift,
      gateway_up: plan.gateway_up,
      gateway_pending: Boolean(plan.gateway_pending),
      items,
    });
    return ok;
  };
  const restartIds = plan.backend_restart_ids ?? [];
  const restarted = new Set<string>();
  try {
    for (const id of restartIds) {
      trackedStatus(`Restarting backend ${id}…`);
      const r = await restartBackend(id);
      if (r.error) {
        trackedStatus(`Backend ${id} restart failed: ${r.error}`, true);
        for (const item of items) {
          if (item.scope !== "backend") {
            if (plan.gateway_pending) {
              item.detail = "Not attempted — the apply stopped before the gateway restart";
            }
            continue;
          }
          if (item.targets.includes(id)) {
            item.outcome = "failed";
            item.detail = r.error;
          } else if (item.targets.length && item.targets.every((t) => restarted.has(t))) {
            item.outcome = "applied";
            item.detail = undefined;
          } else if (item.targets.some((t) => restartIds.includes(t))) {
            item.detail = "Not attempted — the apply stopped after a failed backend restart";
          } else {
            item.detail = "Already effective — takes effect at the backend's next start";
          }
        }
        return finish("failed", false);
      }
      restarted.add(id);
    }
    // Every named backend restarted: restart-class items are
    // applied; the rest were already effective (their backends are
    // stopped — the saved change is what the next start launches
    // with).
    for (const item of items) {
      if (item.scope !== "backend") continue;
      if (item.targets.some((t) => restarted.has(t))) {
        item.outcome = "applied";
        item.detail = undefined;
      } else {
        item.detail = "Already effective — takes effect at the backend's next start";
      }
    }
    if (plan.gateway_pending) {
      const ok = await executeActivationRestart(mode, trackedStatus, setManual);
      for (const item of items) {
        if (item.scope === "backend") continue;
        if (ok) {
          item.outcome = "applied";
          item.detail = undefined;
        } else {
          item.outcome = "failed";
          item.detail = lastError || "The gateway restart did not complete";
        }
      }
      // The gateway step is the last step: reaching it means every
      // step was attempted, so a failure here completes the
      // execution with failures rather than aborting it.
      const anyFailed = items.some((i) => i.outcome === "failed");
      return finish(anyFailed ? "completed_with_failures" : "completed", ok);
    }
    trackedStatus("Backends restarted — the saved changes are live.");
    return finish("completed", true);
  } catch {
    // An unexpected failure still leaves its record; the apply's
    // contract to its callers (a boolean) is unchanged.
    for (const item of items) {
      if (item.outcome === "skipped" && item.detail?.startsWith("Not attempted")) {
        item.detail = "Not attempted — the apply errored out";
      }
    }
    return finish("failed", false);
  }
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
  if (act === "copy-manual") {
    if (!ui.manualCommand) return;
    try {
      await navigator.clipboard.writeText(ui.manualCommand);
      ui.status = "Manual commands copied to the clipboard.";
      ui.statusErr = false;
    } catch {
      ui.status = "Copy failed — select the commands in the block and copy them manually.";
      ui.statusErr = true;
    }
    repaint();
    return;
  }
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

/** The dedicated Activation Plan view: one section — the shared
 * plan card in its full variant (count tiles, scope bar, Apply /
 * Cancel actions, dismiss X) over the Pending changes detail
 * (gateway/drift chips, activation-class bar, and the changes
 * table). The global banner's Review button navigates here. */
export function renderActivationPlan(): string {
  return `${topbar("Activation Plan")}
  <div class="view"><div id="activationBody">${loadingHtml("Loading activation plan…")}</div></div>`;
}

/** Display label for a journal change's structured `kind` — the
 * kinds the backend writes (src-tauri's record_*_change call sites).
 * Unknown kinds fall back to a humanized form of the raw string. */
const PLAN_KIND_LABEL: Record<string, string> = {
  gateway_config: "Gateway config",
  gateway_bind: "Bind address",
  gateway_drift: "Gateway drift",
  backend_apply_config: "Backend config",
  backend_add: "Backend added",
  backend_flags: "Flags",
  backend_set_offload: "Offload",
  backend_set_mtp: "MTP draft",
  backend_set_raw_flags: "Raw flags",
  backend_delete: "Backend removed",
  backend_placement: "Placement",
  model_assign: "Model assignment",
};

function planKindLabel(kind: string): string {
  return PLAN_KIND_LABEL[kind] ?? (kind ? kind.replace(/_/g, " ").replace(/^\w/, (ch) => ch.toUpperCase()) : "Change");
}

/** The Change cell: the structured kind as the label and the value
 * core of the journal summary as the value. The summary's leading
 * "Backend `id` " prefix is dropped (the Target column carries it);
 * the offload and "changed to" shapes reduce to their value core.
 * When the core adds nothing beyond the label, the label stands
 * alone. The full journal sentence stays on the cell's tooltip. */
function planChangeCell(c: ActivationChange): string {
  const label = planKindLabel(c.kind);
  let core = c.summary.replace(/^Backend `[^`]*` /, "");
  const off = /expert offload set to (.+)$/.exec(core);
  const mtpSet = /MTP draft set to (.+)$/.exec(core);
  if (off) core = off[1];
  else if (mtpSet) core = mtpSet[1];
  else if (/^MTP draft turned off$/.test(core)) core = "";
  else if (/^raw flags changed$/.test(core)) core = "";
  else if (core.includes(" changed to ")) core = core.split(" changed to ").pop() ?? core;
  else if (/^gateway configuration changed$/i.test(core)) core = "";
  const redundant = !core || label.toLowerCase().includes(core.toLowerCase());
  return `<span title="${esc(c.summary)}">${esc(label)}${redundant ? "" : `: ${esc(core)}`}</span>`;
}

/** The Target cell: scope absorbed as a Label: Value ("Backend:
 * coder" / "Gateway"). A long target list is capped in the cell;
 * the full list rides the tooltip. */
function planTargetCell(c: ActivationChange): string {
  if (c.scope !== "backend") return "Gateway";
  const targets = c.targets ?? [];
  if (!targets.length) return "Backend";
  const shown = targets.slice(0, 2).join(", ") + (targets.length > 2 ? ` +${targets.length - 2}` : "");
  return `<span title="Backend: ${esc(targets.join(", "))}">Backend: ${esc(shown)}</span>`;
}

export async function wireActivationPlan(root: HTMLElement): Promise<() => void> {
  const planUi = newPlanUi();
  let lastPlan: ActivationPlanData | null = null;
  /** The last fetch error. A failed fetch must never paint the
   * all-clear: the view keeps the last good plan and surfaces the
   * error, or reports the plan as unavailable when none was ever
   * loaded (principle 12 — unknown is not "nothing pending"). */
  let lastError: string | null = null;
  /** Recorded executions (newest first) and their fetch state,
   * tracked independently of the plan: a history failure renders
   * in the history card only and never disturbs the plan above
   * it. Details load on first expansion and stay cached for the
   * view's lifetime (an execution record never changes). */
  let history: ActivationExecutionSummary[] | null = null;
  let historyError: string | null = null;
  const expandedExec = new Set<string>();
  const execDetails = new Map<string, ActivationExecution | null>();
  const execDetailErrors = new Map<string, string>();
  let disposed = false;
  let timer: number | null = null;

  const execStatusChip = (status: string): string => {
    if (status === "completed") return `<span class="chip status-tag status-on">Completed</span>`;
    if (status === "completed_with_failures") return `<span class="chip status-tag status-transition">Completed with failures</span>`;
    if (status === "failed") return `<span class="chip status-tag status-bad">Failed</span>`;
    return `<span class="chip status-tag status-off">${esc(status)}</span>`;
  };

  const execOutcomeChip = (outcome: string): string => {
    if (outcome === "applied") return `<span class="chip status-tag status-on">Applied</span>`;
    if (outcome === "failed") return `<span class="chip status-tag status-bad">Failed</span>`;
    return `<span class="chip status-tag status-off">Skipped</span>`;
  };

  const execDetailHtml = (id: string): string => {
    if (!execDetails.has(id)) {
      return `<div style="font-size:12px;color:var(--text2);margin-top:6px">Loading execution detail…</div>`;
    }
    const detail = execDetails.get(id);
    if (!detail) {
      return `<div style="font-size:12px;color:var(--err);margin-top:6px">${esc(execDetailErrors.get(id) ?? "Execution detail unavailable")}</div>`;
    }
    const modeLabel = detail.mode === "quiesce" ? "Drain and apply"
      : detail.mode === "restart" ? "Apply now"
      : detail.mode === "start" ? "Apply plan" : (detail.mode ?? "");
    const meta = `<div style="font-size:12px;color:var(--text2);margin-top:6px">Mode: <b style="color:var(--text)">${esc(modeLabel)}</b> · Source: <b style="color:var(--text)">${esc(detail.source)}</b> · Gateway: <b style="color:var(--text)">${detail.gateway_up ? "running" : "stopped"}</b>${detail.drift ? ` · Gateway config: <b style="color:var(--text)">drifted</b>` : ""}</div>`;
    if (!detail.items.length) {
      return `${meta}<div style="font-size:12px;color:var(--text2);margin-top:6px">No plan changes — the apply addressed gateway drift only.</div>`;
    }
    const rows = detail.items.map((i) => `<tr><td>${esc(fmtDate(i.ts))} ${esc(fmtTime(i.ts))}</td><td>${planTargetCell(i)}</td><td>${planChangeCell(i)}</td><td>${execOutcomeChip(i.outcome)}</td><td>${i.detail ? esc(i.detail) : ""}</td></tr>`).join("");
    return `${meta}<table class="dtab" style="margin-top:8px"><thead><tr><th>Saved</th><th>Target</th><th>Change</th><th>Outcome</th><th>Detail</th></tr></thead><tbody>${rows}</tbody></table>`;
  };

  const execRowHtml = (s: ActivationExecutionSummary): string => {
    const open = expandedExec.has(s.id);
    return `<div style="border-top:1px solid var(--border);padding:8px 0">
      <div style="display:flex;align-items:center;gap:10px;flex-wrap:wrap;font-size:12px">
        <span>${esc(fmtDate(s.started_at))} ${esc(fmtTime(s.started_at))}</span>
        ${execStatusChip(s.status)}
        <span>Applied: <b>${fmtCount(s.applied)}</b></span>
        <span>Skipped: <b>${fmtCount(s.skipped)}</b></span>
        <span>Failed: <b>${fmtCount(s.failed)}</b></span>
        <span class="spacer" style="flex:1"></span>
        <button class="btn icon-only" data-exec-toggle="${esc(s.id)}" title="${open ? "Hide" : "Show"} execution detail" aria-label="Toggle execution detail">${icon(open ? "x" : "eye")}</button>
      </div>
      ${open ? execDetailHtml(s.id) : ""}
    </div>`;
  };

  /** The Recent executions card: durable apply history under the
   * plan — summary rows first (time, status, outcome counts),
   * the recorded plan contents on demand per row. */
  const historySectionHtml = (): string => {
    let body: string;
    if (!history) {
      body = historyError
        ? `<div style="font-size:12px;color:var(--err);margin-top:6px">${esc(historyError)}</div>`
        : `<div style="font-size:12px;color:var(--text2);margin-top:6px">Loading execution history…</div>`;
    } else if (!history.length) {
      body = `<div style="font-size:12px;color:var(--text2);margin-top:6px">No executions recorded</div>`;
    } else {
      body = history.map(execRowHtml).join("");
    }
    return `<div class="card" style="margin-top:12px"><h3>${icon("clock")} Recent executions</h3>${body}</div>`;
  };

  const paint = (): void => {
    const b = root.querySelector("#activationBody");
    if (!b) return;
    if (!lastPlan && lastError) {
      b.innerHTML = `<div class="card"><h3>${icon("warn")} Activation plan: unavailable</h3>
        <div style="font-size:12px;color:var(--err);margin-top:6px">${esc(lastError)}</div>
        ${planStatusHtml(planUi)}
        <div class="card-actions"><button class="btn icon-only" data-plan-refresh title="Refresh" aria-label="Refresh">${icon("restart")}</button></div></div>`
        + historySectionHtml();
      return;
    }
    if (!lastPlan || !lastPlan.pending) {
      b.innerHTML = `<div class="card"><h3>${icon("check")} No activation plan pending<span class="info-tip" title="Every saved configuration change is live. A change that needs a restart to take effect — gateway routes, a backend's flags, tensor split, or model assignment — appears here, and in the banner at the top of the window, until it is applied.">${icon("info")}</span></h3>
        <div style="font-size:12px;color:var(--text2);margin-top:6px">Pending changes: <b style="color:var(--text)">0</b></div>
        ${lastPlan && lastPlan.files_differ && !lastPlan.gateway_up ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">Gateway config files on disk differ from the saved configuration — the next gateway start regenerates them, so nothing is pending.</div>` : ""}
        ${planStatusHtml(planUi)}
        <div class="card-actions"><button class="btn icon-only" data-plan-refresh title="Refresh" aria-label="Refresh">${icon("restart")}</button></div></div>`
        + historySectionHtml();
      return;
    }
    const p = lastPlan;
    const errNote = lastError
      ? `<div class="banner" style="margin-bottom:12px">${icon("warn")} Activation plan refresh failed: ${esc(lastError)} — showing the last loaded plan.</div>`
      : "";
    // The Pending changes detail: header (gateway state + drift as
    // chips), the activation-class bar, then the table. Refresh
    // lives on the plan card above — one Refresh for the section.
    // Drift arrives as a synthesized gateway_drift change item
    // (tracker #57), so a drift-only plan lists it in the table
    // like any journal change.
    const gwChip = `<span class="chip status-tag ${p.gateway_up ? "status-on" : "status-bad"}">Gateway: ${p.gateway_up ? "running" : "stopped"}</span>`;
    const driftChip = p.drift
      ? ` <span class="chip status-tag status-transition" title="The saved gateway configuration differs from the running one (drift).">Drift</span>`
      : "";
    let detailBody = "";
    if (p.changes.length) {
      const n = p.changes.length;
      const bar = stackedBar(p.changes.map((c) => ({
        pct: 100 / n,
        color: c.activation === "next_start" ? "#6d7686" : C.amber,
        label: `${planChangeMeta(c)} — ${c.summary}`,
      })), 600, 8);
      const rows = p.changes.map((c) => `<tr><td>${esc(fmtDate(c.ts))} ${esc(fmtTime(c.ts))}</td><td>${planTargetCell(c)}</td><td>${planChangeCell(c)}</td><td>${c.activation === "next_start"
        ? `<span class="chip status-tag status-off">Next start</span>`
        : `<span class="chip status-tag status-transition">Restart</span>`}</td></tr>`).join("");
      detailBody = `<div style="margin-top:10px">${bar}
        <div class="legend" style="margin-top:4px"><span><span class="sw" style="background:${C.amber}"></span>Restart</span><span><span class="sw" style="background:#6d7686"></span>Next start</span></div></div>
      <table class="dtab" style="margin-top:8px"><thead><tr><th>Saved</th><th>Target</th><th>Change</th><th>Activation</th></tr></thead><tbody>${rows}</tbody></table>`;
    }
    const detail = `<div class="card"><h3>${icon("logs")} Pending changes <span class="spacer" style="flex:1"></span>${gwChip}${driftChip}</h3>${detailBody}</div>`;
    b.innerHTML = errNote + activationPlanHtml(p, planUi, "full") + detail + historySectionHtml();
  };

  const load = async (): Promise<void> => {
    const [r, h] = await Promise.all([getActivationPlan(), getActivationHistory()]);
    if (disposed) return;
    if (r.error) {
      // Keep the last good plan; the error surfaces in paint.
      lastError = r.error;
    } else {
      lastError = null;
      lastPlan = r.data;
    }
    if (h.error) {
      // Same discipline for the history: keep the last good list.
      historyError = h.error;
    } else {
      historyError = null;
      history = h.data.executions;
    }
    paint();
  };

  const onClick = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLElement;
    if (t.closest("[data-plan-refresh]")) { await load(); return; }
    const et = t.closest("[data-exec-toggle]") as HTMLElement | null;
    if (et) {
      const id = et.getAttribute("data-exec-toggle") ?? "";
      if (expandedExec.has(id)) {
        expandedExec.delete(id);
        paint();
        return;
      }
      expandedExec.add(id);
      paint();
      if (!execDetails.has(id)) {
        const d = await getActivationExecution(id);
        if (disposed) return;
        if (d.error) {
          execDetails.set(id, null);
          execDetailErrors.set(id, d.error);
        } else {
          execDetails.set(id, d.data.execution);
          if (!d.data.execution) {
            execDetailErrors.set(id, "This execution is no longer in the retained history");
          }
        }
        paint();
      }
      return;
    }
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
/** Parallel-slot presets offered by the flags editor (tracker #61). */
const PARALLEL_PRESETS = [1, 2, 4, 8];
/** The Parallel slots control's range — the same bounds
 *  backend_set_parallel validates (manager_models::parallel). */
const PARALLEL_MIN = 1;
const PARALLEL_MAX = 1024;
/** The slot count llama.cpp's `--parallel` default (auto, -1)
 *  initializes on the pinned build, banner-verified — see
 *  manager_models::parallel. The unset field state names it so
 *  the effective count is visible rather than a bare "auto", and
 *  it seeds the Custom input when the operator leaves the
 *  default to type a count. */
const PARALLEL_LLAMA_AUTO_SLOTS = 4;

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
  // Prefer the backend's true per-block pricing for this position
  // (UD quants price blocks unevenly, so a uniform share disagrees
  // with the backend card's RAM figures at partial positions); the
  // uniform share is the fallback for older payloads without it.
  const priced = Array.isArray(o.sys_bytes_by_position) ? o.sys_bytes_by_position[layers] : undefined;
  const sysBytes = typeof priced === "number" && Number.isFinite(priced)
    ? priced
    : Math.floor((o.expert_bytes * layers) / o.moe_layers);
  const mib = (bytes: number): number => Math.ceil(bytes / 1048576);
  return { gpuMib: mib(o.total_bytes - sysBytes), sysMib: mib(sysBytes) };
}

/** True when a slider position is outside the guardrail's verified
 *  set: the guardrail allows endpoints only and the position is
 *  neither endpoint. Such a position is untested on this GPU;
 *  saving it requires an explicit override. */
function offloadIsUntested(o: BackendOffload, layers: number): boolean {
  return o.guardrail?.allowed === "endpoints_only" && layers !== 0 && layers !== o.moe_layers;
}

/** The offload slider's layers label, naming the untested state
 *  when the (draft) position is outside the verified set. */
function offloadLayersLabel(o: BackendOffload, layers: number): string {
  return offloadIsUntested(o, layers)
    ? `${layers} of ${o.moe_layers} MoE layers \u2014 untested on this GPU`
    : `${layers} of ${o.moe_layers} MoE layers`;
}

/** The offload slider's info-tip text. An untested draft position
 *  gains a sentence naming the GPU architecture it is unverified
 *  on and the override rule for saving it. */
function offloadTipText(o: BackendOffload, layers: number): string {
  const base = "Routed-expert weights for the first N MoE layers run from system RAM instead of the GPU, freeing GPU memory at some speed cost. Green is this model's catalog default \u2014 the most offload that stays fast; past it, CPU expert computation increasingly dominates generation speed. Figures are weights only; the KV cache stays on the GPU. The slider saves when released and applies on the backend's next start.";
  if (!offloadIsUntested(o, layers) || !o.guardrail) return base;
  return `${base} This position is outside the range verified for this model's GPU architecture (compute capability ${o.guardrail.compute_capability}): partial offload positions are not verified there, and saving requires an explicit override while starts without one are refused.`;
}

/** The flag tokens a slider position writes — the canonical forms
 *  the app itself writes (backend_set_offload). */
function offloadTokenText(o: BackendOffload, layers: number): string {
  if (layers <= 0) return "no offload tokens";
  if (layers >= o.moe_layers) return "-ot exps=CPU";
  return `--n-cpu-moe ${layers}`;
}

/* ---------------- MTP draft control (tracker #53) ----------------
 * State arrives in the list_backends payload (`BackendMtp`,
 * decoded server-side from the backend's saved raw flags) and
 * commits go through backend_set_mtp, which rewrites only the
 * MTP tokens. Values are 0..4, where 0 is Off. The control is a
 * dropdown in the ctx/batch selects' idiom (Pete, Oct 18, 2026:
 * the original range slider showed neither its stops nor that
 * it was a slider until clicked). */
/** llama.cpp's `--spec-draft-n-max` default, used to place the
 *  control for a backend that enables MTP without an explicit
 *  length (and named in that state's readout). */
const MTP_LLAMA_N_MAX_DEFAULT = 3;

/** The values the dropdown offers: 0 is Off, 1..4 are
 *  `--spec-draft-n-max` draft lengths. */
const MTP_POSITIONS = [0, 1, 2, 3, 4];

/** The control value a saved MTP state implies: 0 when off;
 *  the explicit length when set; llama.cpp's default when MTP is
 *  on without one. */
function mtpPosition(m: BackendMtp): number {
  if (!m.enabled) return 0;
  return m.n_max ?? MTP_LLAMA_N_MAX_DEFAULT;
}

/** The flag form a chosen value writes (the readout once a
 *  value is chosen, and the saved message after a commit). */
function mtpTokenText(pos: number): string {
  return pos === 0 ? "Speculative decoding off" : `--spec-type draft-mtp --spec-draft-n-max ${pos}`;
}

/** The readout for the SAVED state, before any change: a backend
 *  that enables MTP without an explicit length runs at
 *  llama.cpp's default, and the readout says so rather than
 *  implying a saved value. */
function mtpSavedTokenText(m: BackendMtp): string {
  if (!m.enabled) return "Speculative decoding off";
  if (m.n_max == null) return `--spec-type draft-mtp (draft length: llama.cpp default, ${MTP_LLAMA_N_MAX_DEFAULT})`;
  return mtpTokenText(m.n_max);
}

/* ---------------- Predicted throughput (design §6, §7) -----
 * One line shape for the flags forms (Edit Flags and Add
 * Backend) and one tile for the backend card, all fed by the
 * same resolved-measurement shape (api.ts PredictedThroughput),
 * so a figure reads identically on every surface. */

/** "Oct 14" — plus the year when it is not the current year
 *  (or always, when `alwaysYear` — the card tile's tooltip,
 *  §7). Unparseable input renders as the raw string. */
const MONTHS_SHORT = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
function fmtMeasuredDate(iso: string, alwaysYear: boolean): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const base = `${MONTHS_SHORT[d.getMonth()]} ${d.getDate()}`;
  return alwaysYear || d.getFullYear() !== new Date().getFullYear()
    ? `${base}, ${d.getFullYear()}`
    : base;
}

/** The source phrase a tip names (§5.5 disclosure). A null
 *  source is a v1 entry: the absence is stated, not filled in. */
function predictedSourceLabel(source: string | null): string {
  switch (source) {
    case "benchmark": return "Benchmark";
    case "experiment": return "Experiment run";
    case "live": return "Live observation";
    default: return "Source not recorded";
  }
}

/** Placement in short form from an inputs snapshot: the GPU
 *  names, "CPU only" for an empty GPU set, context and batch. */
function predictedPlacementText(p: PredictedThroughput): string | null {
  const inp = p.inputs;
  if (!inp) return null;
  const gpus = inp.gpus.length
    ? inp.gpus.map((g) => g.split(":").slice(1).join(":") || g).join(", ")
    : "CPU only";
  return `Context ${inp.n_ctx.toLocaleString()} · Batch ${inp.n_batch.toLocaleString()} · ${gpus}`;
}

/** The form line's info-tip (§6.1): what the figures are, when
 *  and how they were measured, the placement, and the
 *  environment (§5.5). */
function predictedFormTip(p: PredictedThroughput): string {
  const d = new Date(p.measured_at);
  const full = Number.isNaN(d.getTime())
    ? p.measured_at
    : `${MONTHS_SHORT[d.getMonth()]} ${d.getDate()}, ${d.getFullYear()}`;
  const sourcePhrase = p.source === "benchmark"
    ? "by the standardized benchmark (the Optimize page's Measure action)"
    : p.source === "experiment"
      ? "by an Optimize experiment run"
      : p.source === "live"
        ? "from live gateway traffic"
        : "before measurement sources were recorded (an early entry)";
  const placement = predictedPlacementText(p);
  const env = [`LLMdil ${p.app_version}`];
  if (p.driver_version.length) env.push(`driver ${p.driver_version.join(", ")}`);
  if (p.llama_build) env.push(`llama.cpp ${p.llama_build}`);
  return `Measured ${full} ${sourcePhrase}. `
    + (placement ? `Placement: ${placement}. ` : "Placement: not recorded for this entry. ")
    + "Prompt processing comes from the 4,000-token prompt; generation and time to first token from the 200-token prompt. "
    + `Environment: ${env.join(" · ")}.`;
}

/** The card tile's tooltip (§7): prompt rate, TTFT, measured
 *  date, source, placement, and environment on one line. */
function predictedCardTip(p: PredictedThroughput): string {
  const parts: string[] = [];
  if (p.prompt_per_second !== null) parts.push(`Prompt: ${trim1(p.prompt_per_second)} tok/s`);
  if (p.ttft_ms !== null) parts.push(`TTFT: ${(p.ttft_ms / 1000).toFixed(2)} s`);
  parts.push(`Measured: ${fmtMeasuredDate(p.measured_at, true)}`);
  parts.push(predictedSourceLabel(p.source));
  const placement = predictedPlacementText(p);
  if (placement) parts.push(placement);
  if (p.driver_version.length) parts.push(`Driver: ${p.driver_version.join(", ")}`);
  if (p.llama_build) parts.push(`llama.cpp ${p.llama_build}`);
  parts.push(`LLMdil ${p.app_version}`);
  return parts.join(" · ");
}

/** The flags form's predicted line for one lookup result
 *  (design §6.1 exact, §6.3 near): the exact line when an exact
 *  match exists, else the muted Similar measurement line with
 *  the differing fields named, else nothing at all (§5.4 —
 *  empty renders nothing). */
function predictedLineHtml(exact: PredictedThroughput | null, near: PredictedThroughput | null): string {
  const figures = (p: PredictedThroughput): string => {
    const parts: string[] = [];
    if (p.predicted_per_second !== null) parts.push(`${trim1(p.predicted_per_second)} tok/s generation`);
    if (p.prompt_per_second !== null) parts.push(`${trim1(p.prompt_per_second)} prompt`);
    return parts.join(" · ");
  };
  if (exact && figures(exact)) {
    return `<div style="font-size:12px;color:var(--text2);margin-top:6px">Predicted throughput: ${esc(figures(exact))} \u2014 measured ${esc(fmtMeasuredDate(exact.measured_at, false))}<span class="info-tip" title="${esc(predictedFormTip(exact))}">${icon("info")}</span></div>`;
  }
  if (near && figures(near)) {
    const differs = (near.differs ?? []).join(", ");
    return `<div style="font-size:12px;color:var(--text2);margin-top:6px">Similar measurement: ${esc(figures(near))}${differs ? ` \u2014 differs: ${esc(differs)}` : ""} \u2014 measured ${esc(fmtMeasuredDate(near.measured_at, false))}<span class="info-tip" title="${esc(predictedFormTip(near))}">${icon("info")}</span></div>`;
  }
  return "";
}

/** The --parallel conflict line's text. MTP drafting needs a
 *  single slot; the MTP control surfaces an explicit conflicting
 *  slot count and never rewrites it — the Parallel slots field
 *  in the same form is where the operator sets it (tracker #61),
 *  and the line says so. */
function mtpConflictText(parallel: number): string {
  return `Parallel slots: ${parallel} — MTP drafting needs --parallel 1; set Parallel slots to 1 in the Flags form — the next start will not draft until then`;
}

/* ---------------- Advanced flags validation (tracker #60) ----------------
 * A TypeScript mirror of manager-models' raw_flag_warnings, used
 * only to render the advanced editor's warnings live while the
 * operator types. The Rust function is the authority: the save op
 * returns its own warning list and the panel adopts that list
 * after saving. Keep the two in step — the flag set and the
 * warning wording come from the Rust module.
 *
 * Warnings never block: an unknown token or a malformed value is
 * passed to llama-server untouched and llama-server decides. */

export interface FlagWarningLike { token: string; message: string }

type FlagArity = { kind: "none" } | { kind: "value"; value: "int" | "any" } | { kind: "multi"; value: "int" | "any" };
interface FlagSpecLike { canonical: string; reserved: boolean; arity: FlagArity }

const V = (value: "int" | "any"): FlagArity => ({ kind: "value", value });
const FLAG_SPECS: ReadonlyArray<readonly [string, string, boolean, FlagArity]> = [
  ["--n-cpu-moe", "--n-cpu-moe", false, V("int")], ["-ncmoe", "--n-cpu-moe", false, V("int")],
  ["--cpu-moe", "--cpu-moe", false, { kind: "none" }], ["-cmoe", "--cpu-moe", false, { kind: "none" }],
  ["-ot", "--override-tensor", false, { kind: "multi", value: "any" }], ["--override-tensor", "--override-tensor", false, { kind: "multi", value: "any" }],
  ["--spec-type", "--spec-type", false, V("any")],
  ["--spec-draft-n-max", "--spec-draft-n-max", false, V("int")],
  ["--parallel", "--parallel", false, V("int")], ["-np", "--parallel", false, V("int")],
  ["--model", "--model", false, V("any")], ["-m", "--model", false, V("any")],
  ["--port", "--port", false, V("int")],
  ["--ctx-size", "--ctx-size", false, V("int")], ["-c", "--ctx-size", false, V("int")],
  ["--batch-size", "--batch-size", false, V("int")], ["-b", "--batch-size", false, V("int")],
  ["--tensor-split", "--tensor-split", false, V("any")], ["-ts", "--tensor-split", false, V("any")],
  ["--split-mode", "--split-mode", false, V("any")],
  ["--device", "--device", false, V("any")], ["-dev", "--device", false, V("any")],
  ["--main-gpu", "--main-gpu", false, V("int")],
  ["--gpu-layers", "--gpu-layers", false, V("int")], ["--n-gpu-layers", "--gpu-layers", false, V("int")], ["-ngl", "--gpu-layers", false, V("int")],
  ["--threads", "--threads", false, V("int")], ["-t", "--threads", false, V("int")],
  ["--jinja", "--jinja", false, { kind: "none" }],
  ["--host", "--host", true, V("any")],
  ["--no-jinja", "--no-jinja", true, { kind: "none" }], ["--no_jinja", "--no-jinja", true, { kind: "none" }],
];
const FLAG_SPEC_MAP: ReadonlyMap<string, FlagSpecLike> = new Map(FLAG_SPECS.map(([name, canonical, reserved, arity]) => [name, { canonical, reserved, arity }]));

function flagSpecFor(token: string): FlagSpecLike | undefined {
  if (token === "--jinja=false") return FLAG_SPEC_MAP.get("--no-jinja");
  return FLAG_SPEC_MAP.get(token);
}

const isIntText = (s: string): boolean => /^[+-]?\d+$/.test(s);

/** Validate raw flags text the way the save op will. Same
 *  whitespace tokenization the flag decoders use. */
export function rawFlagWarnings(raw: string): FlagWarningLike[] {
  const tokens = raw.split(/\s+/).filter((t) => t !== "");
  const out: FlagWarningLike[] = [];
  const seen = new Map<string, string>();
  const checkInt = (flagTok: string, value: string): void => {
    if (!isIntText(value)) out.push({ token: flagTok, message: `${flagTok} expects a whole number; "${value}" will make llama-server reject the flag` });
  };
  const noteSeen = (flagTok: string, spec: FlagSpecLike): void => {
    if (spec.arity.kind === "multi") return;
    if (seen.has(spec.canonical)) out.push({ token: flagTok, message: `${spec.canonical} is set more than once (${flagTok}); only the last value is used` });
    else seen.set(spec.canonical, flagTok);
  };
  let i = 0;
  while (i < tokens.length) {
    const tok = tokens[i];
    if (!tok.startsWith("-")) {
      out.push({ token: tok, message: `"${tok}" is not part of a flag llama-server knows; it will reject the launch arguments` });
      i += 1;
      continue;
    }
    const eq = tok.indexOf("=");
    const name = eq >= 0 ? tok.slice(0, eq) : tok;
    const inline = eq >= 0 ? tok.slice(eq + 1) : null;
    const spec = flagSpecFor(name) ?? flagSpecFor(tok);
    if (!spec) {
      out.push({ token: name, message: `${name} is not a flag this app recognizes; it will be passed to llama-server unchanged` });
      if (inline === null && i + 1 < tokens.length && !tokens[i + 1].startsWith("-")) i += 1;
      i += 1;
      continue;
    }
    noteSeen(name, spec);
    if (spec.reserved) {
      out.push({ token: name, message: `${spec.canonical} is reserved: the app filters it out before launch, so it has no effect here` });
    }
    if (spec.arity.kind === "none") {
      if (inline !== null && !(name === "--jinja" && inline === "false")) {
        out.push({ token: name, message: `${name} takes no value; "=${inline}" will make llama-server reject the flag` });
      }
      i += 1;
      continue;
    }
    const valueKind = spec.arity.value;
    if (inline !== null) {
      if (valueKind === "int") checkInt(name, inline);
      i += 1;
      continue;
    }
    if (i + 1 >= tokens.length || tokens[i + 1].startsWith("-")) {
      out.push({ token: name, message: `${name} expects a value but none follows it` });
      i += 1;
      continue;
    }
    if (valueKind === "int") checkInt(name, tokens[i + 1]);
    i += 2;
  }
  return out;
}

/** The advanced editor's warning lines (amber, inline under the
 *  text box). Empty when the text is clean. */
function flagsRawWarningsHtml(warnings: FlagWarningLike[]): string {
  if (!warnings.length) return "";
  return warnings.map((w) => `<div style="font-size:12px;color:#e3b341;margin-top:4px">${icon("warn")} ${esc(w.message)}</div>`).join("");
}

/** The offload slider track's green-to-yellow-to-red gradient for
 *  a model's band boundaries (fractions of the offloaded range,
 *  computed by the backend from the catalog green limit). */
function offloadGradient(o: BackendOffload): string {
  const pct = (f: number): string => `${(f * 100).toFixed(1)}%`;
  return `linear-gradient(90deg, ${C.green} 0%, ${C.green} ${pct(o.green_end)}, ${C.amber} ${pct(o.green_end)}, ${C.amber} ${pct(o.yellow_end)}, ${C.red} ${pct(o.yellow_end)}, ${C.red} 100%)`;
}

/** The offload slider track's background. With an endpoints-only
 *  guardrail the interior of the track (between the two endpoint
 *  positions' half-step zones at each end) is unverified, so it
 *  renders muted and hatched rather than in the green/yellow/red
 *  band gradient: a hatch layer and a muted fill span the interior
 *  (a centered span, so background-position 50% places it exactly),
 *  with the normal band gradient retained underneath and visible
 *  only at the endpoint zones. Without a guardrail the track is
 *  exactly the band gradient. */
function offloadTrackBackground(o: BackendOffload): string {
  if (o.guardrail?.allowed !== "endpoints_only" || o.moe_layers <= 1) return offloadGradient(o);
  const interior = (((o.moe_layers - 1) / o.moe_layers) * 100).toFixed(1);
  const span = `center / ${interior}% 100% no-repeat`;
  const hatch = `repeating-linear-gradient(45deg, rgba(232,235,240,0.30) 0 2px, rgba(18,21,26,0.30) 2px 4px) ${span}`;
  const muted = `linear-gradient(90deg, #3a4350, #3a4350) ${span}`;
  return `${hatch}, ${muted}, ${offloadGradient(o)}`;
}

/** The context/batch field pair of the flags editor — preset
 *  selects with a Custom numeric input when the value is not a
 *  preset — shared between the Edit Flags form and the Add backend
 *  form (tracker #49): one implementation of the parameter
 *  fields, two hosts. `fieldAttr` is the data attribute the host's
 *  draft sync reads ("data-flags-field" in the edit form,
 *  "data-add-flags-field" in the add form); ctxMax annotates
 *  presets beyond the model's rated context. */
function flagsCtxBatchHtml(o: {
  ctx: number;
  batch: number;
  ctxMax: number | null;
  fieldAttr: string;
  disabled: boolean;
}): string {
  const dis = o.disabled ? " disabled" : "";
  return `<div style="display:grid;grid-template-columns:1fr 1fr;gap:10px;margin-top:8px">
        <label style="font-size:12px;color:var(--text2)">Context length (-c)
          <span style="display:flex;gap:6px;margin-top:4px">
            <select ${o.fieldAttr}="ctx-preset" style="${SELECT_STYLE};flex:1;min-width:0"${dis}>
              ${CTX_PRESETS.map((v) => `<option value="${v}"${v === o.ctx ? " selected" : ""}>${v.toLocaleString()} (${ctxShort(v)})${v === 32768 ? " \u2014 default" : ""}${o.ctxMax && v > o.ctxMax ? " \u2014 beyond rated" : ""}</option>`).join("")}
              <option value="custom"${CTX_PRESETS.includes(o.ctx) ? "" : " selected"}>Custom\u2026</option>
            </select>
            ${CTX_PRESETS.includes(o.ctx) ? "" : `<input ${o.fieldAttr}="ctx-custom" type="number" value="${o.ctx}" min="1024" max="1048576" style="${SELECT_STYLE};width:110px"${dis}>`}
          </span></label>
        <label style="font-size:12px;color:var(--text2)">Batch size (-b)
          <span style="display:flex;gap:6px;margin-top:4px">
            <select ${o.fieldAttr}="batch-preset" style="${SELECT_STYLE};flex:1;min-width:0"${dis}>
              ${BATCH_PRESETS.map((v) => `<option value="${v}"${v === o.batch ? " selected" : ""}>${v.toLocaleString()}${v === 512 ? " \u2014 default" : ""}</option>`).join("")}
              <option value="custom"${BATCH_PRESETS.includes(o.batch) ? "" : " selected"}>Custom\u2026</option>
            </select>
            ${BATCH_PRESETS.includes(o.batch) ? "" : `<input ${o.fieldAttr}="batch-custom" type="number" value="${o.batch}" min="1" style="${SELECT_STYLE};width:110px"${dis}>`}
          </span></label>
      </div>`;
}

/** The flag form the Parallel slots selection writes: the
 *  canonical token for an explicit count, or the default's
 *  description when unset. */
function parallelTokenText(value: number | null): string {
  if (value === null) return `llama.cpp default: auto (${PARALLEL_LLAMA_AUTO_SLOTS} slots)`;
  if (value === -1) return "--parallel -1 (auto)";
  return `--parallel ${value}`;
}

/** The parallel-slots field of the flags editor — a preset select
 *  with a Custom numeric input when the value is not a preset,
 *  in the ctx/batch idiom — shared between the Edit Flags form
 *  and the Add backend form (tracker #61): one implementation of
 *  the field, two hosts. The value is the explicit slot count,
 *  or null for the unset state: no `--parallel` token is saved
 *  and llama.cpp's default (auto) applies — the select's first
 *  choice carries that state, so unset is a visible selection,
 *  not an absence. `savedCustom` marks the saved spelling as one
 *  the control does not write (an `-np` or `=` spelling, ...),
 *  the MTP control's marker; the readout beside the field names
 *  what the current selection writes. */
function flagsParallelHtml(o: {
  value: number | null;
  savedCustom: boolean;
  fieldAttr: string;
  disabled: boolean;
}): string {
  const dis = o.disabled ? " disabled" : "";
  const v = o.value;
  const isCustom = v !== null && !PARALLEL_PRESETS.includes(v);
  return `<div style="display:grid;grid-template-columns:1fr 1fr;gap:10px;margin-top:8px;align-items:end">
        <label style="font-size:12px;color:var(--text2)">Parallel slots (--parallel)<span class="info-tip" title="Request slots the server runs concurrently; each slot is an independent sequence with its own share of the work. An explicit count divides the context length across the slots; Auto (llama.cpp's default) keeps one unified KV cache at the full context. 1 suits a single request stream, and MTP drafting needs 1.">${icon("info")}</span>
          <span style="display:flex;gap:6px;margin-top:4px">
            <select ${o.fieldAttr}="parallel-preset" style="${SELECT_STYLE};flex:1;min-width:0"${dis}>
              <option value="default"${v === null ? " selected" : ""}>Auto \u2014 llama.cpp default</option>
              ${PARALLEL_PRESETS.map((p) => `<option value="${p}"${p === v ? " selected" : ""}>${p}</option>`).join("")}
              <option value="custom"${isCustom ? " selected" : ""}>Custom\u2026</option>
            </select>
            ${isCustom ? `<input ${o.fieldAttr}="parallel-custom" type="number" value="${v}" min="${PARALLEL_MIN}" max="${PARALLEL_MAX}" style="${SELECT_STYLE};width:110px"${dis}>` : ""}
          </span></label>
        <div style="font-size:12px;color:var(--text2)"><span style="font-family:var(--mono)">${esc(parallelTokenText(v))}</span>${o.savedCustom ? `<span title="The saved flags spell parallel slots in a form the control does not write (an -np or = spelling, repeated tokens, ...). Saving from the field writes the canonical --parallel N; every other flag is preserved."> \u00b7 custom flags</span>` : ""}</div>
      </div>`;
}

/** The rated-context line under the flags fields, shared by the
 *  Edit Flags form and the Add backend form. `savedText` is the
 *  host's sentence for when the values take effect (the edit form
 *  saves to the configuration immediately; the add form saves at
 *  creation). */
function flagsRatedLineHtml(o: {
  ctxMax: number | null;
  modelId: string | null;
  savedText: string;
}): string {
  return `<div style="font-size:12px;color:var(--text2);margin-top:6px">${o.modelId && o.ctxMax
    ? `Rated context: ${o.ctxMax.toLocaleString()}<span class="info-tip" title="${esc(`Model ${o.modelId} is rated to ${ctxShort(o.ctxMax)} context (${o.ctxMax.toLocaleString()} tokens); larger values are accepted but exceed the rating. ${o.savedText}`)}">${icon("info")}</span>`
    : `Context range: 1,024\u20131,048,576<span class="info-tip" title="Accepted context range: 1,024 to 1,048,576. ${o.savedText}">${icon("info")}</span>`}</div>`;
}

/** The expert-offload slider block — label with guardrail-aware
 *  text, the slider track, the weight figures, and the host's
 *  guardrail decision row — shared between the Edit Flags form
 *  and the Add backend form. Dragging updates the readout in
 *  place via paintOffloadReadout; the host owns what releasing
 *  the slider means (the edit form saves immediately through
 *  backend_set_offload; the add form records the position for
 *  creation). */
function offloadSliderHtml(o: {
  offload: BackendOffload;
  layers: number;
  ownerId: string;
  disabled: boolean;
  decisionHtml: string;
}): string {
  const off = o.offload;
  const n = o.layers;
  const figs = offloadFigures(off, n);
  const dis = o.disabled ? " disabled" : "";
  return `<div style="margin-top:12px">
      <label style="font-size:12px;color:var(--text2);display:block">Expert offload to system RAM \u2014 <span data-offload-layers-label>${esc(offloadLayersLabel(off, n))}</span><span class="info-tip" data-offload-tip title="${esc(offloadTipText(off, n))}">${icon("info")}</span></label>
      <input type="range" class="offload-range" min="0" max="${off.moe_layers}" step="1" value="${n}" data-offload-slider data-offload-backend="${esc(o.ownerId)}" style="background:${offloadTrackBackground(off)}" aria-label="Expert offload layers for ${esc(o.ownerId)}"${dis}>
      <div style="font-size:12px;color:var(--text2);margin-top:4px"><span data-offload-token style="font-family:var(--mono)">${esc(offloadTokenText(off, n))}</span> \u00b7 GPU weights: <span data-offload-gpu>${fmtGb(figs.gpuMib / 1024)}</span> GiB \u00b7 System RAM: <span data-offload-sys>${fmtGb(figs.sysMib / 1024)}</span> GiB</div>
      ${o.decisionHtml}
    </div>`;
}

/** Update an offload slider's readout in place (layers label,
 *  flag tokens, weight figures, info tip) — shared by both forms,
 *  so a drag never needs a repaint in either. */
function paintOffloadReadout(card: Element, o: BackendOffload, n: number): void {
  const figs = offloadFigures(o, n);
  const setText = (sel: string, txt: string): void => {
    const el = card.querySelector(sel);
    if (el) el.textContent = txt;
  };
  setText("[data-offload-layers-label]", offloadLayersLabel(o, n));
  setText("[data-offload-token]", offloadTokenText(o, n));
  setText("[data-offload-gpu]", fmtGb(figs.gpuMib / 1024));
  setText("[data-offload-sys]", fmtGb(figs.sysMib / 1024));
  const tip = card.querySelector("[data-offload-tip]");
  if (tip) tip.setAttribute("title", offloadTipText(o, n));
}

/** Apply one flags-field event to a draft — the collection logic
 *  both forms share for the ctx/batch preset+custom pairs and
 *  the parallel preset+custom pair. Preset selects set the value
 *  on "change" and report that a repaint is needed (to show or
 *  hide the Custom numeric input); custom inputs only update the
 *  draft. Returns true when the host should repaint. The parallel
 *  value is the explicit slot count, or null for the field's
 *  Default (auto) choice; choosing Custom seeds the input from
 *  the current selection — the auto slot count when unset — so
 *  the operator edits a real number, never a blank. */
function applyFlagsField(
  draft: { ctx: number; batch: number; parallel: number | null },
  field: string,
  value: string,
  isChange: boolean,
): boolean {
  if (field === "ctx-custom") { draft.ctx = Number(value); return false; }
  if (field === "batch-custom") { draft.batch = Number(value); return false; }
  if (field === "parallel-custom") { draft.parallel = Number(value); return false; }
  if (isChange && field === "ctx-preset") {
    if (value !== "custom") draft.ctx = Number(value);
    return true;
  }
  if (isChange && field === "batch-preset") {
    if (value !== "custom") draft.batch = Number(value);
    return true;
  }
  if (isChange && field === "parallel-preset") {
    if (value === "default") draft.parallel = null;
    else if (value === "custom") draft.parallel = draft.parallel ?? PARALLEL_LLAMA_AUTO_SLOTS;
    else draft.parallel = Number(value);
    return true;
  }
  return false;
}

/** Draft state for the in-card flags form, non-null only for the
 *  backend whose form is open. ctxMax/modelId describe the backend's
 *  model (its rated context from the catalog), for annotating
 *  presets beyond the rating. offloadLayers is the offload slider's
 *  draft position (MoE layers on system RAM), null when the
 *  backend's model is not offload-capable. offloadPendingLayers is
 *  a released draft position the guardrail blocks, awaiting the
 *  operator's override/snap decision in the form, else null.
 *  mtpPosition is the MTP draft control's chosen value (0 = Off,
 *  1..4 = draft tokens), null when the backend's model is not
 *  MTP-capable. parallel is the Parallel slots field's draft
 *  selection: the explicit slot count, or null for the unset
 *  state (llama.cpp's default, auto). advanced/rawText/rawWarnings
 *  carry the advanced flags editor's draft (tracker #60): while
 *  advanced is on, the raw text is the single source of truth and
 *  the structured fields are inert. */
export interface FlagsDraft { ctx: number; batch: number; ctxMax: number | null; modelId: string | null; offloadLayers: number | null; offloadPendingLayers: number | null; mtpPosition: number | null; parallel: number | null; advanced: boolean; rawText: string; rawWarnings: FlagWarningLike[] }

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
  predictedHtml: string,
  place: PlaceCardState | null,
  gpuTotalsMiB: Map<number, number>,
  detailsOpen: boolean,
): string {
  const active = b.state === "running" || b.state === "starting";
  // Buttons render disabled while this card's action is in flight.
  // The set lives outside the repainted DOM, so the disabled state
  // survives the repaints a poll or a log-tail fetch triggers.
  const dis = inFlight.has(b.id) ? " disabled" : "";
  const stopBtn = `<button class="btn" data-back-act="stop" data-back-id="${esc(b.id)}"${dis}>${icon("pause")} Stop</button>`;
  const restartBtn = `<button class="btn" data-back-act="restart" data-back-id="${esc(b.id)}"${dis}>${icon("restart")} Restart</button>`;
  const startBtn = `<button class="btn" data-back-act="start" data-back-id="${esc(b.id)}"${dis}>${icon("play")} Start</button>`;
  const testBtn = `<button class="btn icon-only" data-back-act="test" data-back-id="${esc(b.id)}" title="Test backend" aria-label="Test ${esc(b.id)}"${dis}>${icon("bolt")}</button>`;
  // Tool-call probe (MCP design §7.2): a secondary safe action, so
  // icon-only with the action named in its tooltip/aria-label.
  const toolCallBtn = `<button class="btn icon-only" data-back-act="toolcall" data-back-id="${esc(b.id)}" title="Test tool calls" aria-label="Test tool calls for ${esc(b.id)}"${dis}>${icon("wrench")}</button>`;
  // Go to logs (tracker #47): every backend has a Logs-view scope (its
  // supervisor ring, retained even after removal), so the button is
  // unconditional. Icon-only per rule 15 — non-destructive navigation,
  // not the card's standard workflow action — matching the Dashboard
  // card's logs button. An anchor, not a card action: navigation is
  // not disabled while a card action is in flight.
  const logsBtn = `<a class="btn icon-only" href="#/logs?scope=${esc(encodeURIComponent(b.id))}&follow=1" title="Logs" aria-label="Logs for ${esc(b.id)}">${icon("logs")}</a>`;
  // Enable/disable is the shared state switch (rule 16). It renders
  // for every configured backend so the state is always visible;
  // the control is inert exactly when the old buttons were not
  // offered — an action in flight, or enabled in a live state (the
  // command layer refuses disable-while-running: a running backend
  // is stopped, not disabled). The data-back-act flips with the
  // state, so the delegated handler fires the same
  // applyBackendConfig call the two buttons fired.
  const toggleable = !b.enabled || b.state === "stopped" || b.state === "failed";
  const enableSw = enableSwitch({
    on: b.enabled,
    name: `Backend ${b.id}`,
    attrs: `data-back-act="${b.enabled ? "disable" : "enable"}" data-back-id="${esc(b.id)}"`,
    title: b.enabled && !toggleable ? `Backend ${b.id}: enabled (stop it before disabling)` : undefined,
    disabled: inFlight.has(b.id) || !toggleable,
  });
  const removeBtn = `<button class="btn icon-only danger${confirmRemove.has(b.id) ? " armed" : ""}" data-back-act="remove" data-back-id="${esc(b.id)}" title="${confirmRemove.has(b.id) ? "Confirm remove" : "Remove backend"}" aria-label="${confirmRemove.has(b.id) ? "Confirm remove" : "Remove"} ${esc(b.id)}"${dis}>${icon("trash")}</button>`;
  // Offer only what the command layer will accept:
  // - Stop whenever a live process exists (pid present, running or
  //   starting), whether the backend is configured, enabled, or an
  //   orphan — stopping is always allowed.
  // - Start/Restart only for configured, enabled backends; no Start
  //   while a stop is still draining ("stopping").
  // - The enable/disable switch shows on every configured backend;
  //   it is toggleable only in the states listed for it above.
  // - Test only while running.
  const parts: string[] = [];
  if (b.pid !== null && active) parts.push(stopBtn);
  if (b.configured && b.enabled && active) parts.push(restartBtn);
  if (b.configured && b.enabled && !active && b.state !== "stopping") parts.push(startBtn);
  if (b.configured) parts.push(enableSw);
  if (b.state === "running") parts.push(testBtn);
  if (b.configured) parts.push(toolCallBtn);
  parts.push(logsBtn);
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
  // Card narrative lives in header-chip tooltips, never as inline
  // prose (Pete's Label: Value rule, 2026-10-16): the orphan and
  // adopted explanations ride the title attribute of the chips that
  // already flag those states. The orphan text follows the process
  // state — a failed or stopping orphan is not "still running".
  const orphanText = !b.configured
    ? b.state === "failed"
      ? "It failed, and its entry was removed from the configuration. It cannot be restarted."
      : active
        ? "Still running, but its entry was removed from the configuration. It can be stopped here; it cannot be restarted."
        : "Its entry was removed from the configuration. It cannot be restarted."
    : "";
  // An adopted backend was started by a previous launch of the app
  // (its processes survive the app closing) and taken over by pid:
  // it can be stopped/restarted here, but its earlier output is not
  // in this session's log ring.
  const adoptedText = b.adopted
    ? "Started by a previous launch and adopted by this session. Output from before adoption is not in the Logs view."
    : "";
  let logsBlock = "";
  if (b.state === "failed") {
    const tail = logTails.get(b.id);
    // An empty tail renders no block at all (principle 13) — the
    // "Open in Logs" link below still carries the operator to the
    // full log. The caption's qualifier rides its info-tip.
    const lines = tail && tail.length
      ? `<div style="margin-top:8px;font-size:11px;color:var(--text3)">Latest output<span class="info-tip" title="stderr preferred — that is where llama-server reports load failures">${icon("info")}</span></div><div class="logstream" style="margin-top:4px">${tail.map((r) => `<div class="ln"><span class="t">${esc(fmtTime(r.ts))}</span> <span class="chip">${esc(r.stream)}</span> ${esc(r.line)}</div>`).join("")}</div>`
      : "";
    logsBlock = `${lines}<div style="margin-top:6px;font-size:12px"><a href="#/logs?scope=${esc(encodeURIComponent(b.id))}&follow=1" style="color:var(--accent)">Open in Logs</a></div>`;
  }
  // GPU residency for this backend, as bars (Pete's card redesign,
  // 2026-10-16): one horizontal bar per GPU, name at left, GiB at
  // right, filled to the backend's share of that GPU's total VRAM.
  // Measured per-process telemetry is authoritative while the
  // backend is active; under 1 GiB is a CUDA context only and
  // renders as a striped sliver (its tooltip says so). When the
  // backend is inactive there is no measured residency, so the bars
  // are ghost fills of the configured tensor split — where the
  // model will load at its next start. An unmeasured GPU (mib null
  // — WDDM, counter data unavailable) gets an empty bar; "not
  // measured" exists only as its tooltip, never as inline text.
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
  const barRow = (index: number, fillPct: number, valText: string, cls = "", tip = ""): string => `<div class="res-bar-row"${tip ? ` title="${esc(tip)}"` : ""}>
      <span class="res-bar-name">${gpuLabel(index)}</span>
      <span class="res-bar-track${cls === "ghost" ? " ghost" : ""}"><i class="${cls}" style="width:${Math.min(100, Math.max(0, fillPct)).toFixed(1)}%;background-color:${gpuColors[index % gpuColors.length]}"></i></span>
      <span class="res-bar-val">${valText}</span></div>`;
  let gpuHtml = "";
  if (b.configured) {
    const knownRes = res.filter((r) => r.mib !== null);
    let bars = "";
    let basis = "";
    let titleTip = "";
    if (knownRes.length) {
      basis = "measured";
      bars = res.map((r) => {
        if (r.mib === null) return barRow(r.gpu, 0, "", "", "Not measured — per-process usage is not measured on this GPU; placement follows the configured split.");
        const total = gpuTotalsMiB.get(r.gpu) ?? 0;
        const share = total > 0 ? (r.mib / total) * 100 : 0;
        const gib = `${fmtGb(r.mib / 1024)} GiB`;
        return r.mib < 1024
          ? barRow(r.gpu, share, gib, "ctx", `Context only: ${gib} — under 1 GiB on a GPU is the CUDA context only, not model weights.`)
          : barRow(r.gpu, share, gib);
      }).join("");
    } else if (res.length) {
      basis = "not measured";
      titleTip = "Per-process usage is not measured on this GPU — placement follows the configured split.";
      bars = res.map((r) => barRow(r.gpu, 0, "", "", "Not measured — per-process usage is not measured on this GPU; placement follows the configured split.")).join("");
    } else if (!active) {
      // Ghost bars from the configured split; an unsplit backend
      // loads whole on GPU 0 (the app's standing placement default,
      // the same basis the dashboard placements use).
      const ghost = splitEntries.length ? splitEntries : [{ index: 0, frac: 1 }];
      basis = "configured";
      bars = ghost.map((e) => barRow(e.index, e.frac * 100, `${Math.round(e.frac * 100)}%`, "ghost", "Configured split — this share of the model loads on this GPU at the backend's next start.")).join("");
    }
    // An active backend with no attributed process yet has nothing
    // real to encode; the section appears with the first sample.
    if (bars) {
      gpuHtml = `<div class="backend-gpus">
      <div class="backend-gpus-title">GPUs (${basis})${titleTip ? `<span class="info-tip" title="${esc(titleTip)}">${icon("info")}</span>` : ""}</div>
      ${bars}
    </div>`;
    }
  }
  // Gateway statistics for this backend (trailing hour, from the
  // LiteLLM metrics window store). The card carries Throughput only:
  // Requests (1h) and Errors (1h) already live in the view's overview
  // list (numbers in one place, principle 6). Absent until a window
  // reports throughput.
  // The stat tiles: live Throughput (trailing window, present
  // once the metrics poller has one) and Predicted throughput
  // (the saved configuration's exact-match measurement, design
  // §7 — generation only, the rest in the tooltip; absent
  // entirely when no exact measurement exists).
  const statTiles: [string, string, string | null][] = [];
  if (b.throughput_toks !== undefined) {
    statTiles.push(["Throughput", `${trim1(b.throughput_toks)} tok/s`, null]);
  }
  if (b.predicted && b.predicted.predicted_per_second !== null) {
    statTiles.push(["Predicted throughput", `${trim1(b.predicted.predicted_per_second)} tok/s`, predictedCardTip(b.predicted)]);
  }
  const statsHtml = statTiles.length
    ? `<div style="display:grid;grid-template-columns:repeat(3,1fr);gap:10px;margin-top:8px">
        ${statTiles
          .map(([k, v, tip]) => `<div class="kv"><span class="k">${esc(k)}${tip ? `<span class="info-tip" title="${esc(tip)}">${icon("info")}</span>` : ""}</span><span class="v" style="font-size:13px">${esc(v)}</span></div>`).join("")}
      </div>`
    : "";
  // Offload guardrail and probe lines for the saved position
  // (at rest, not the flags-form draft). Status colors follow the
  // design principles: light red for blocked/failed, amber for
  // transitional/warning, gray for the neutral custom spelling.
  // Nothing renders when none of the states apply (principle 13).
  // A blocked-and-unresolved position carries the one-click
  // resolving action; a custom spelling in that same state reuses
  // that button rather than duplicating it.
  const offloadStatusHtml = (() => {
    const o = b.offload;
    const probe = b.offload_probe;
    if (!o && !probe) return "";
    const lines: string[] = [];
    const line = (color: string, inner: string): string =>
      `<div style="font-size:12px;color:${color};margin-top:6px">${inner}</div>`;
    const g = o?.guardrail ?? null;
    if (o && g?.status === "blocked" && !g.overridden) {
      // The blocked fact is a launch-preflight fact and renders in
      // both modes; the one-click resolving action is a flags
      // write, so in advanced flags mode (tracker #60) the line
      // carries no button — the editor is the remedy, and the
      // server refuses the structured write anyway.
      lines.push(line("#ff7b72", b.flags_advanced
        ? `Start blocked: offload ${o.layers} of ${o.moe_layers} layers is outside the verified range for this GPU`
        : `Start blocked: offload ${o.layers} of ${o.moe_layers} layers is outside the verified range for this GPU <button class="btn" data-back-act="offload-set-verified" data-back-id="${esc(b.id)}"${dis}>${icon("check")} Set to ${g.nearest_verified_layers} of ${o.moe_layers} layers</button>`));
    } else if (o && g?.status === "blocked" && g.overridden) {
      lines.push(line("#e3b341", `Offload override recorded: ${o.layers} of ${o.moe_layers} layers is unverified on this GPU`));
    } else if (o && g?.status === "untested") {
      lines.push(line("#e3b341", `Partial offload: unverified on this GPU`));
    }
    // The "custom spelling" line is a structured-form readout:
    // in advanced mode the operator's own string is displayed
    // verbatim (the raw flags line), so the line is redundant.
    if (o?.custom && !b.flags_advanced) {
      lines.push(line("var(--text2)", `Offload: Custom (${o.layers} of ${o.moe_layers} layers)`));
    }
    if (probe?.state === "probing") {
      lines.push(line("#e3b341", o ? `Probing offload: ${probe.layers} of ${o.moe_layers} layers` : `Probing offload: ${probe.layers} layers`));
    } else if (probe?.state === "failed") {
      const detail = (probe.detail ?? "").trim();
      const inner = !detail
        ? "Offload probe failed"
        : detail.length <= 120
          ? `Offload probe failed \u2014 ${esc(detail)}`
          : `Offload probe failed<span class="info-tip" title="${esc(detail)}">${icon("info")}</span>`;
      lines.push(line("#ff7b72", inner));
    }
    // A passed probe is transient assurance, not card state: no line.
    return lines.join("");
  })();
  // At-rest MTP lines (tracker #53), in the offload status
  // block's idiom: a non-standard saved spelling, and the
  // --parallel conflict when MTP is on — surfaced, never fixed
  // silently. Nothing renders in the ordinary states.
  const mtpStatusHtml = (() => {
    const m = b.mtp;
    // Advanced flags mode (tracker #60): both lines decode the
    // raw flags into structured terms the mode supersedes. The
    // raw string itself is displayed verbatim on the card, so a
    // custom spelling or a --parallel conflict is visible in it
    // directly, and the flags editor is the remedy the conflict
    // copy points at.
    if (!m || b.flags_advanced) return "";
    const lines: string[] = [];
    if (m.custom) {
      lines.push(`<div style="font-size:12px;color:var(--text2);margin-top:6px">MTP: Custom (${esc(mtpSavedTokenText(m))})</div>`);
    }
    if (m.enabled && m.parallel != null && m.parallel !== 1) {
      lines.push(`<div style="font-size:12px;color:#e3b341;margin-top:6px">${esc(mtpConflictText(m.parallel))}</div>`);
    }
    return lines.join("");
  })();
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
    // A released position the guardrail blocks is not committed:
    // the form carries an inline decision row until the operator
    // saves it with an override or snaps to the nearest verified
    // position. Both actions are operation-critical, so both keep
    // their words and an icon.
    const pending = flags.offloadPendingLayers;
    const decisionHtml = pending !== null && o.guardrail && offloadIsUntested(o, pending) ? `<div data-offload-decision style="display:flex;gap:8px;align-items:center;flex-wrap:wrap;margin-top:8px">
      <span data-offload-decision-text style="font-size:12px;color:#e3b341">${pending} of ${o.moe_layers} layers is unverified on this GPU</span>
      <button class="btn" data-back-act="offload-override" data-back-id="${esc(b.id)}"${dis}>${icon("warn")} Save with override</button>
      <button class="btn" data-back-act="offload-snap" data-back-id="${esc(b.id)}"${dis}>${icon("check")} Snap to ${o.guardrail.nearest_verified_layers} of ${o.moe_layers} layers</button>
    </div>` : "";
    return offloadSliderHtml({ offload: o, layers: n, ownerId: b.id, disabled: inFlight.has(b.id), decisionHtml });
  })() : "";
  // The MTP draft control (tracker #53), directly under the
  // offload slider in the same panel: a dropdown over
  // Off/1/2/3/4 in the ctx/batch selects' idiom (SELECT_STYLE,
  // the current value as the selected option, always visible at
  // rest), whose value lives in the wire-level draft state
  // (flags.mtpPosition), with the token readout naming the
  // flags the value writes, and the --parallel conflict line
  // when the saved flags set a slot count other than 1.
  // Rendered only for MTP-capable backends (the payload carries
  // `mtp`). The readout shows the SAVED spelling until the value
  // changes; a backend on without an explicit length reads as
  // llama.cpp's default rather than an implied saved value.
  const mtpHtml = flags && b.mtp ? (() => {
    const m = b.mtp;
    const pos = flags.mtpPosition ?? mtpPosition(m);
    const conflict = pos > 0 && m.parallel != null && m.parallel !== 1;
    return `<div style="margin-top:12px">
      <label style="font-size:12px;color:var(--text2);display:block">MTP draft tokens<span class="info-tip" title="Speculative decoding: the model drafts up to this many tokens ahead and verifies them in a single pass. More draft tokens can generate faster, but drafts the verifier rejects are wasted work, so gains depend on how often drafts are accepted. Applies on the backend's next start.">${icon("info")}</span>
        <span style="display:flex;gap:6px;margin-top:4px">
          <select data-mtp-select data-mtp-backend="${esc(b.id)}" style="${SELECT_STYLE};flex:1;min-width:0" aria-label="MTP draft tokens for ${esc(b.id)}"${dis}>
            ${MTP_POSITIONS.map((v) => `<option value="${v}"${v === pos ? " selected" : ""}>${v === 0 ? "Off" : String(v)}</option>`).join("")}
          </select>
        </span></label>
      <div style="font-size:12px;color:var(--text2);margin-top:4px"><span data-mtp-token style="font-family:var(--mono)">${esc(mtpSavedTokenText(m))}</span>${m.custom ? `<span title="The saved flags spell MTP in a form the control does not write (an = spelling, a shared --spec-type list, ...). Saving from this control writes the canonical tokens; every other flag is preserved."> · custom flags</span>` : ""}</div>
      <div data-mtp-conflict style="font-size:12px;color:#e3b341;margin-top:4px;${conflict ? "" : "display:none"}">${m.parallel != null ? esc(mtpConflictText(m.parallel)) : ""}</div>
    </div>`;
  })() : "";
  // The Parallel slots field (tracker #61), under the ctx/batch
  // pair: a preset+custom select over the backend's --parallel
  // count, committing through backend_set_parallel on change —
  // the sliders' commit model, since the value lives in raw flags
  // like theirs, not in the Save button's ctx/batch. The custom
  // marker shows while the draft still equals the saved value:
  // it describes the saved spelling, and a changed selection has
  // no saved spelling yet.
  const parallelHtml = flags && b.parallel ? flagsParallelHtml({
    value: flags.parallel,
    savedCustom: b.parallel.custom && flags.parallel === b.parallel.slots,
    fieldAttr: "data-flags-field",
    disabled: inFlight.has(b.id),
  }) : "";
  // Advanced flags mode (tracker #60): the panel is REPLACED by
  // the raw-flags text editor — the structured controls are not
  // rendered at all, so nothing inert invites an edit the server
  // would refuse. The textarea pre-fills with the backend's saved
  // raw flags (the string the structured decoders read); text
  // edits sync to the wire-level draft, so poll repaints keep the
  // text and the warning list renders live under the box.
  const flagsHtml = flags ? (flags.advanced ? `<div class="backend-form-panel flags-panel">
      <div style="font-size:12px;font-weight:600">${icon("gear")} Flags for ${esc(b.id)}</div>
      <div style="font-size:12px;color:var(--text2);margin-top:6px">Mode: Advanced<span class="info-tip" title="In advanced mode the text below is the backend's launch flags, edited directly. Context, batch, offload, MTP and placement are all set by this string; the structured form and its controls are disabled for this backend until you switch back. Warnings never block a save — unknown flags pass through to llama-server, which decides.">${icon("info")}</span></div>
      <textarea data-flags-raw="${esc(b.id)}" rows="4" spellcheck="false" style="width:100%;margin-top:8px;background:var(--panel2);border:1px solid var(--border);border-radius:8px;color:var(--text);padding:8px 10px;font-family:var(--mono);font-size:12px" aria-label="Raw flags for ${esc(b.id)}">${esc(flags.rawText)}</textarea>
      <div data-flags-warnings>${flagsRawWarningsHtml(flags.rawWarnings)}</div>
      <div style="font-size:12px;color:var(--text2);margin-top:6px">Saved to the configuration immediately; applies on the backend's next start.</div>
      <div data-predicted-slot>${predictedHtml}</div>
      <div class="card-actions"><button class="btn" data-back-act="flags-raw-save" data-back-id="${esc(b.id)}"${dis}>${icon("check")} Save flags</button>
        <button class="btn" data-back-act="flags-mode-structured" data-back-id="${esc(b.id)}"${dis}>${icon("gear")} Use structured form</button>
        <button class="btn icon-only" data-back-act="flags-cancel" data-back-id="${esc(b.id)}" title="Cancel" aria-label="Cancel">${icon("x")}</button></div>
    </div>` : `<div class="backend-form-panel flags-panel">
      <div style="font-size:12px;font-weight:600">${icon("gear")} Flags for ${esc(b.id)}</div>
      ${flagsCtxBatchHtml({ ctx: flags.ctx, batch: flags.batch, ctxMax: flags.ctxMax, fieldAttr: "data-flags-field", disabled: inFlight.has(b.id) })}
      ${parallelHtml}
      ${offloadHtml}
      ${mtpHtml}
      ${flagsRatedLineHtml({ ctxMax: flags.ctxMax, modelId: flags.modelId, savedText: "Saved to the configuration immediately; applies on the backend's next start. Tensor split is set by the Placement form on this card." })}
      <div data-predicted-slot>${predictedHtml}</div>
      <div class="card-actions"><button class="btn" data-back-act="flags-save" data-back-id="${esc(b.id)}"${dis}>${icon("check")} Save flags</button>
        <button class="btn" data-back-act="flags-mode-advanced" data-back-id="${esc(b.id)}"${dis}>${icon("pencil")} Edit flags as text</button>
        <button class="btn icon-only" data-back-act="flags-cancel" data-back-id="${esc(b.id)}" title="Cancel" aria-label="Cancel">${icon("x")}</button></div>
    </div>`) : "";
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
      const free = p.free_mib && p.free_mib[i] !== undefined ? `${p.free_mib[i].toLocaleString()} MiB` : "\u2014";
      const fit = frac === 0 ? "not used" : p.per_gpu_fits[i] ? "fits" : "does not fit";
      const creditCells = credited ? `<td>${(p.current_mib?.[i] ?? 0).toLocaleString()} MiB</td><td>${(p.additional_mib?.[i] ?? p.per_gpu_mib[i]).toLocaleString()} MiB</td>` : "";
      return `<tr><td>${esc(gname)}</td><td>${Math.round(frac * 100)}%</td><td>${p.per_gpu_mib[i].toLocaleString()} MiB</td>${creditCells}<td>${free}</td><td>${fit}</td></tr>`;
    }).join("");
    const ageMs = p.snapshot_age_ms ?? null;
    return `<div style="margin-top:10px">
      ${p.down_indices && p.down_indices.length ? `<div style="font-size:12px;color:var(--err);margin-bottom:6px" title="${p.down_indices.map((i) => `GPU ${i}`).join(", ")} ${p.down_indices.length === 1 ? "is" : "are"} down and ${p.down_indices.length === 1 ? "is" : "are"} treated as having no free VRAM.">${p.down_indices.map((i) => `GPU ${i}`).join(", ")}: down — no free VRAM</div>` : ""}
      <div style="font-size:12px"><span title="${p.fits ? "The model fits the selected GPUs with the projected split." : "The model does not fit the available VRAM with this split."}">Fit: ${p.fits ? "fits" : "does not fit"}</span><span class="info-tip" title="Projection uses model weights only; run-time allocations add overhead.">${icon("info")}</span></div>
      <table class="dtab" style="margin-top:8px"><thead><tr><th>GPU</th><th>Fraction</th><th>Required</th>${credited ? `<th title="The weight allocation this backend already holds on the GPU. Applying the split restarts the backend, freeing it first.">Current</th><th title="Required minus current: what must fit in free VRAM.">Additional</th>` : ""}<th>Free (current)</th><th>Fit</th></tr></thead><tbody>${rows}</tbody></table>
      ${p.sys_mib ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">System RAM: ${fmtGb(p.sys_mib / 1024)} GiB<span class="info-tip" title="Routed experts under expert offload. The per-GPU figures cover the GPU-resident weights only.">${icon("info")}</span></div>` : ""}
      <div style="font-size:12px;color:var(--text2);margin-top:6px">${ageMs === null ? "Snapshot: latest" : `Snapshot age: ${Math.round(ageMs / 1000)} s`}</div></div>`;
  })() : "";
  const placeHtml = place ? `<div class="backend-form-panel placement-panel">
      <div style="font-size:12px;font-weight:600">${icon("layers")} Placement for ${esc(b.id)}</div>
      ${place.modelId
        ? ""
        : `<div style="font-size:12px;color:var(--err);margin-top:4px">Placement: unavailable<span class="info-tip" title="This backend's model file matches no configured model, so a split cannot be proposed or applied for it.">${icon("info")}</span></div>`}
      ${place.gpus.length ? `<div style="margin-top:8px">
        <span style="font-size:12px;color:var(--text2)">GPUs in the split:</span><span class="info-tip" title="The split divides the model's weights across the selected GPUs, in nvidia-smi index order. Uncheck a GPU to leave it out \u2014 for example the display GPU, whose memory Windows already uses. The split is saved to the backend's configuration and applies on its next start.">${icon("info")}</span>
        <div class="gpu-chip-row">${place.gpus.map((g) => {
          const on = !place.excluded.has(g.index);
          return `<label class="gpu-chip${on ? " on" : ""}${placeDis ? " is-disabled" : ""}" title="${esc(g.name)}"><span class="gpu-chip-top"><input type="checkbox" data-place-gpu="${g.index}" data-place-backend="${esc(b.id)}"${on ? " checked" : ""}${placeDis}> GPU ${g.index}</span><span class="gpu-chip-name">${esc(gpuShortName(g.name))}</span></label>`;
        }).join("")}</div>
      </div>`
      : `<div style="font-size:12px;color:var(--text2);margin-top:6px">GPUs: none reported<span class="info-tip" title="No GPU telemetry yet \u2014 a split cannot be proposed until telemetry reports the GPUs.">${icon("info")}</span></div>`}
      ${projHtml}
      <div class="card-actions"><button class="btn" data-back-act="place-propose" data-back-id="${esc(b.id)}" title="Propose split"${placeDis || !place.modelId || !place.gpus.length ? " disabled" : ""}>${icon("search")} Propose split</button>
        <button class="btn" data-back-act="place-apply" data-back-id="${esc(b.id)}"${place.projection && !place.busy && !dis ? "" : " disabled"}>${icon("check")} Apply split</button>
        <button class="btn icon-only" data-back-act="place-cancel" data-back-id="${esc(b.id)}" title="Close" aria-label="Close">${icon("x")}</button></div>
    </div>` : "";
  // Basis of the memory figures (exact | catalog | estimate): the
  // explanation lives in a tooltip on the RAM line; the word itself
  // stays inline only for estimate, as the qualifier it was before.
  const basisTip = b.memory_basis === "exact"
    ? "Basis: exact — measured from the downloaded model file."
    : b.memory_basis === "catalog"
      ? "Basis: catalog — from the catalog entry for this model (used before download)."
      : b.memory_basis === "estimate"
        ? "Basis: estimate — estimated figures, not measured from the model file."
        : "";
  // Metric strip (Pete's card redesign): one row of labeled
  // single-value tiles — Model, Context, GPU RAM, System RAM, KV
  // cache. No sentences; the model tile carries the model select
  // (configured) or the file name (orphan), as before.
  const modelTileVal = b.configured
    ? `<select class="backend-model-select" data-back-model="${esc(b.id)}" style="${SELECT_STYLE};width:100%;min-width:0"${dis}>${modelOptions(b, models)}</select>`
    : `<span class="backend-model-file" title="${esc(b.model_file)}">${esc(b.model_file)}</span>`;
  const stripTiles: [string, string][] = [["Model", modelTileVal]];
  // Advanced flags mode (tracker #60): the structured readouts the
  // mode supersedes are not presented as authoritative — the
  // context value is decoded from flags the structured form no
  // longer owns, so the tile shows an em dash with the explanation
  // in its tooltip, and the KV estimate (derived from that
  // context) is omitted. The raw flags line below carries the
  // state itself.
  if (b.n_ctx !== undefined) stripTiles.push(["Context", b.flags_advanced ? `<span title="Set by the raw flags in advanced flags mode">&#8212;</span>` : b.n_ctx.toLocaleString()]);
  if (b.gpu_ram_mib !== undefined) {
    stripTiles.push(["GPU RAM", `${fmtGb(b.gpu_ram_mib / 1024)} GiB`]);
    // An absent sys_ram_mib is unknown, never 0 (principle 12): the
    // tile renders only when the payload carries the figure.
    if (b.sys_ram_mib !== undefined) stripTiles.push(["System RAM", `${fmtGb(b.sys_ram_mib / 1024)} GiB`]);
    if (!b.flags_advanced && b.kv_ram_mib != null) stripTiles.push(["KV cache", `<span title="KV cache at the configured context">${fmtGb(b.kv_ram_mib / 1024)} GiB</span>`]);
  }
  const stripHtml = `<div style="display:grid;grid-template-columns:${["2fr", ...stripTiles.slice(1).map(() => "1fr")].join(" ")};gap:10px;margin-top:8px">
      ${stripTiles.map(([k, v]) => `<div class="kv"><span class="k">${k}</span><span class="v" style="font-size:13px">${v}</span></div>`).join("")}
    </div>`;
  // Details expandable: everything demoted off the closed card —
  // State/PID/Uptime/Batch, the configured-split percentages, and
  // the memory basis (its explanation rides the info-tip). Open
  // state lives in the wire layer, so poll repaints keep it.
  // State is not repeated here — the header's state chip carries it
  // (numbers and states live in one place, principle 6).
  const detailKv: [string, string][] = [
    ["PID", b.pid !== null ? String(b.pid) : "—"],
    ["Uptime", fmtUptime(b.uptime_secs)],
  ];
  // Batch is a structured flags readout like Context: omitted in
  // advanced flags mode (tracker #60) rather than shown as fact.
  if (!b.flags_advanced && b.n_batch != null) detailKv.push(["Batch", b.n_batch.toLocaleString()]);
  const detailsHtml = detailsOpen ? `<div class="backend-details">
      <div style="display:grid;grid-template-columns:repeat(${detailKv.length},1fr);gap:10px">
        ${detailKv.map(([k, v]) => `<div class="kv"><span class="k">${k}</span><span class="v" style="font-size:13px">${esc(v)}</span></div>`).join("")}
      </div>
      ${splitEntries.length ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">Configured split: ${splitSummary}</div>` : ""}
      ${basisTip ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">Basis: ${esc(b.memory_basis ?? "")}<span class="info-tip" title="${esc(basisTip)}">${icon("info")}</span></div>` : ""}
    </div>` : "";
  // Advanced flags mode (tracker #60): the saved raw flags ARE
  // the configuration the operator owns in this mode, so they
  // render verbatim in Label: Value form in place of the
  // structured readouts the mode supersedes. An empty string
  // renders no line (principle 13); the header chip carries the
  // mode itself.
  const rawFlagsHtml = b.flags_advanced && b.raw_flags
    ? `<div style="font-size:12px;color:var(--text2);margin-top:8px">Flags: <span style="font-family:var(--mono);color:var(--text)">${esc(b.raw_flags)}</span></div>`
    : "";
  // Tool-call probe state (MCP design §7.2): the persisted probe
  // outcome, or "not measured" when the backend has never been
  // probed (principle 12 — absence is not a measured state).
  // Supervisor-only orphan rows carry no probe and no line.
  const toolCallHtml = b.configured
    ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">Tool calls: <b style="color:var(--text)">${b.tool_call_probe ? esc(b.tool_call_probe.outcome) : "not measured"}</b></div>`
    : "";
  const detailsBtn = `<button class="btn icon-only" data-back-act="details" data-back-id="${esc(b.id)}" aria-expanded="${detailsOpen}" title="Details" aria-label="Details">${icon(detailsOpen ? "chevD" : "chevR")}</button>`;
  return `<div class="card backend-card" id="card-backend-${esc(b.id)}">
      <h3 class="backend-card-head">${stateDot(b.state)} ${esc(b.id)} <span class="port">:${b.port}</span>
        <span class="spacer" style="flex:1"></span>
        ${b.configured ? "" : `<span class="chip status-tag status-off" title="${esc(orphanText)}">not in config</span>`}
        ${b.configured ? `<span class="chip status-tag ${b.enabled ? "status-on" : "status-off"}">${b.enabled ? "enabled" : "disabled"}</span>` : ""}
        ${b.adopted ? `<span class="chip status-tag status-on" title="${esc(adoptedText)}">adopted</span>` : ""}
        ${b.flags_advanced ? `<span class="chip status-tag" title="Advanced flags mode: this backend's launch flags are edited as raw text in the Flags panel, and the structured flags form is disabled for it.">advanced flags</span>` : ""}
        <span class="chip status-tag ${stateTagClass(b.state)}">${esc(b.state)}</span></h3>
      ${stripHtml}
      ${toolCallHtml}
      ${gpuHtml}
      ${statsHtml}
      ${rawFlagsHtml}
      ${offloadStatusHtml}
      ${mtpStatusHtml}
      ${detailsHtml}
      ${flagsHtml}
      ${placeHtml}
      <div class="card-actions backend-actions">${detailsBtn}
        ${actions}
        ${advancedMenu}</div>
      <div class="back-msg" style="margin-top:8px;font-size:12px">${msgHtml}</div>
      ${logsBlock}
      ${argv}
    </div>`;
}

export function renderBackends(): string {
  return `${topbar("Backends", `<button class="btn" id="backAdd" title="Add backend">${icon("plus")} Add backend</button> <button class="btn icon-only" id="backRefresh" title="Refresh" aria-label="Refresh">${icon("restart")}</button>`)}
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
  /** Backends whose Details expandable is open. */
  const openDetails = new Set<string>();
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
    if (host) host.innerHTML = activationPlanHtml(lastPlan, planUi, "summary");
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
          ? { ctx: flagsCtx, batch: flagsBatch, ctxMax: mdl?.ctx_max ? mdl.ctx_max : null, modelId: mdl?.id ?? null, offloadLayers: flagsOffloadLayers, offloadPendingLayers: offloadPending?.backendId === row.id ? offloadPending.layers : null, mtpPosition: flagsMtpPosition, parallel: flagsParallel, advanced: row.flags_advanced === true, rawText: flagsRawText, rawWarnings: flagsRawWarnings }
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
        return backendCard(row, openArgv, openMenus, argvCache, msgs, logTails, inFlight, lastModels, confirmRemove, residency, new Map(lastGpuList.map((g) => [Number(g.id), g.name])), flagsArg, flagsFor === row.id ? predictedHtml : "", placeArg, new Map(lastGpuList.map((g) => [Number(g.id), g.vram_total_gb * 1024])), openDetails.has(row.id));
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
    // Prune per-backend card state for ids that no longer exist
    // (code-review 5.9): a deleted backend's notes, caches, and
    // open forms must not linger — or resurface if the id is ever
    // reused.
    {
      const alive = new Set(lastRows.map((row) => row.id));
      for (const map of [msgs, argvCache, logTails] as Map<string, unknown>[]) {
        for (const k of [...map.keys()]) if (!alive.has(k)) map.delete(k);
      }
      for (const set of [confirmRemove, openArgv, openDetails, inFlight, tailsWanted] as Set<string>[]) {
        for (const k of [...set]) if (!alive.has(k)) set.delete(k);
      }
      if (flagsFor !== null && !alive.has(flagsFor)) flagsFor = null;
      if (offloadPending !== null && !alive.has(offloadPending.backendId)) offloadPending = null;
      if (placeFor !== null && !alive.has(placeFor)) { placeFor = null; placeProjection = null; }
    }
    // While the offload slider is focused (mid-drag), a repaint
    // would recreate the range input and break the drag; the
    // draft is synced to wire state, so skipping one repaint
    // loses nothing (same rule as the model rename input). The
    // advanced flags textarea (tracker #60) follows the same
    // rule: recreating it mid-edit would lose the caret.
    const sliderFocused = flagsFor !== null
      && (document.activeElement as HTMLElement | null)?.hasAttribute?.("data-offload-slider");
    const rawFocused = flagsFor !== null
      && (document.activeElement as HTMLElement | null)?.hasAttribute?.("data-flags-raw");
    if (!sliderFocused && !rawFocused) paint();
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
  /** A released offload position the guardrail blocks, awaiting the
   *  operator's decision in the flags form (save with override, or
   *  snap to the nearest verified position); null otherwise. */
  let offloadPending: { backendId: string; layers: number } | null = null;
  /** True while a slider commit (backend_set_offload) is in
   *  flight; the slider ignores further commits until it lands. */
  let offloadCommitting = false;
  /** The MTP draft control's chosen value while the flags form
   *  is open (0 = Off, 1..4 = draft tokens); null when the open
   *  backend's model is not MTP-capable. */
  let flagsMtpPosition: number | null = null;
  /** True while an MTP draft commit (backend_set_mtp) is in
   *  flight; the control ignores further commits until it
   *  lands. */
  let mtpCommitting = false;
  /** The Parallel slots field's draft selection while the flags
   *  form is open: the explicit --parallel count, or null for the
   *  unset state (llama.cpp's default, auto). Unlike the MTP
   *  position this exists for every backend — the field is not
   *  capability-gated. */
  let flagsParallel: number | null = null;
  /** True while a parallel commit (backend_set_parallel) is in
   *  flight; the field ignores further commits until it lands. */
  let parallelCommitting = false;
  /** The advanced flags editor's draft text (tracker #60): the
   *  open backend's raw flags as edited, synced on every input
   *  event so poll repaints re-render the textarea from here.
   *  Initialized from the row's saved string when the form opens —
   *  opening the editor performs no write. */
  let flagsRawText = "";
  /** The draft's validation warnings, recomputed live as the
   *  operator types (the save op's own list replaces them after a
   *  save — the server is the authority). */
  let flagsRawWarnings: FlagWarningLike[] = [];
  /** The open flags form's predicted-throughput line
   *  (predicted-throughput design §6): the last lookup's
   *  rendered line for the CURRENT draft, held in wire state
   *  so poll repaints re-render it and a lookup in flight
   *  never blanks it (§6.2). Empty when the draft matches no
   *  measurement exactly or nearly. */
  let predictedHtml = "";
  let predictedSeq = 0;
  let predictedTimer: number | null = null;
  const runPredictedLookup = async (): Promise<void> => {
    const id = flagsFor;
    if (!id) return;
    const row = lastRows.find((r) => r.id === id);
    if (!row) return;
    const seq = ++predictedSeq;
    const req: MeasurementLookupRequest = row.flags_advanced
      ? { backend_id: id, raw_flags: flagsRawText }
      : {
          backend_id: id,
          n_ctx: flagsCtx,
          n_batch: flagsBatch,
          offload_layers: flagsOffloadLayers,
          mtp_position: flagsMtpPosition,
          parallel_slots: flagsParallel,
        };
    const r = await measurementLookup(req);
    if (disposed || seq !== predictedSeq || flagsFor !== id) return;
    predictedHtml = r.data ? predictedLineHtml(r.data.exact, r.data.near) : "";
    // Update the open form's slot in place: a repaint here
    // could disturb an in-progress slider drag or text edit.
    const slot = root.querySelector("[data-predicted-slot]");
    if (slot) slot.innerHTML = predictedHtml;
  };
  /** Debounced lookup trigger (§6.2, ~300 ms): fires on the
   *  same events that mutate the draft. */
  const schedulePredictedLookup = (): void => {
    if (predictedTimer !== null) window.clearTimeout(predictedTimer);
    predictedTimer = window.setTimeout(() => {
      predictedTimer = null;
      void runPredictedLookup();
    }, 300);
  };
  const closeFlagsForm = (): void => {
    flagsFor = null;
    offloadPending = null;
    predictedHtml = "";
    predictedSeq += 1;
    if (predictedTimer !== null) { window.clearTimeout(predictedTimer); predictedTimer = null; }
    paint();
  };
  const openFlagsForm = (row: BackendRow): void => {
    if (flagsFor === row.id) { closeFlagsForm(); return; }
    flagsFor = row.id;
    flagsCtx = row.n_ctx ?? 32768;
    flagsBatch = row.n_batch ?? 512;
    flagsOffloadLayers = row.offload ? row.offload.layers : null;
    flagsMtpPosition = row.mtp ? mtpPosition(row.mtp) : null;
    flagsParallel = row.parallel ? row.parallel.slots : null;
    offloadPending = null;
    // The advanced editor opens on the backend's CURRENT saved
    // raw flags — a pure read of the row payload; opening writes
    // nothing (tracker #60, entry performs no write).
    flagsRawText = row.raw_flags ?? "";
    flagsRawWarnings = rawFlagWarnings(flagsRawText);
    predictedHtml = "";
    predictedSeq += 1;
    paint();
    schedulePredictedLookup();
  };
  /** Keep the draft in step with the in-card form fields. Preset
   *  selects repaint (to show or hide the Custom numeric input);
   *  custom inputs only update the draft. Registered for both
   *  "change" and "input" so typing is captured before any repaint. */
  const syncFlagsDraft = (ev: Event): void => {
    const t = ev.target as HTMLElement;
    const field = t.getAttribute("data-flags-field");
    if (!field) return;
    const draft = { ctx: flagsCtx, batch: flagsBatch, parallel: flagsParallel };
    const repaint = applyFlagsField(draft, field, (t as HTMLInputElement).value, ev.type === "change");
    flagsCtx = draft.ctx;
    flagsBatch = draft.batch;
    flagsParallel = draft.parallel;
    if (repaint) paint();
    schedulePredictedLookup();
  };
  root.addEventListener("change", syncFlagsDraft);
  root.addEventListener("input", syncFlagsDraft);
  /** The advanced editor's textarea sync (tracker #60): keep the
   *  draft text and its live warnings in wire state. The warning
   *  list updates in place — no repaint, so typing is never
   *  disturbed (the poll repaint is also skipped while the
   *  textarea is focused; see load()). */
  const syncRawDraft = (ev: Event): void => {
    const t = ev.target as HTMLTextAreaElement;
    if (!t.hasAttribute("data-flags-raw")) return;
    if (t.getAttribute("data-flags-raw") !== flagsFor) return;
    flagsRawText = t.value;
    flagsRawWarnings = rawFlagWarnings(flagsRawText);
    const box = t.closest(".backend-card")?.querySelector("[data-flags-warnings]");
    if (box) box.innerHTML = flagsRawWarningsHtml(flagsRawWarnings);
    schedulePredictedLookup();
  };
  root.addEventListener("input", syncRawDraft);

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
    schedulePredictedLookup();
    const card = t.closest(".backend-card");
    if (!card) return;
    const o = row.offload;
    paintOffloadReadout(card, o, n);
    // Keep a pending guardrail decision in step with the draft:
    // its text follows a still-blocked position, and the row hides
    // while the draft sits on a verified position (the release
    // handler re-decides from the final position).
    if (offloadPending?.backendId === id) {
      const dec = card.querySelector("[data-offload-decision]") as HTMLElement | null;
      if (dec) {
        if (offloadIsUntested(o, n)) {
          offloadPending.layers = n;
          dec.style.display = "";
          const dt = dec.querySelector("[data-offload-decision-text]");
          if (dt) dt.textContent = `${n} of ${o.moe_layers} layers is unverified on this GPU`;
        } else {
          dec.style.display = "none";
        }
      }
    }
  };
  /** Invoke backend_set_offload for one position and report the
   *  outcome on the card. An override save's message names the
   *  override; any other save keeps the token-form message. On
   *  error the message carries the server's error and the pending
   *  decision (if any) is left for the caller to restore. Returns
   *  true when the position was saved. */
  const saveOffloadPosition = async (id: string, n: number, allowOverride: boolean): Promise<boolean> => {
    if (offloadCommitting) return false;
    const row = lastRows.find((r) => r.id === id);
    offloadCommitting = true;
    inFlight.add(id);
    paint();
    try {
      const r = await setBackendOffload(id, n, allowOverride);
      if (disposed) return false;
      if (r.error) {
        msgs.set(id, { text: `Offload not saved: ${r.error}`, err: true });
        paint();
        return false;
      }
      offloadPending = null;
      if (flagsFor === id) flagsOffloadLayers = n;
      msgs.set(id, {
        text: allowOverride
          ? "Offload saved with override \u2014 applies on next start."
          : `Offload saved (${row?.offload ? offloadTokenText(row.offload, n) : n}) \u2014 applies on next start.`,
        err: false,
      });
      await load();
      schedulePredictedLookup();
      return true;
    } finally {
      offloadCommitting = false;
      inFlight.delete(id);
      if (!disposed) paint();
    }
  };
  const commitOffload = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLInputElement;
    if (!t.hasAttribute("data-offload-slider")) return;
    const id = t.getAttribute("data-offload-backend") ?? "";
    const row = lastRows.find((r) => r.id === id);
    if (!row?.offload || offloadCommitting) return;
    const n = Number(t.value);
    flagsOffloadLayers = n;
    if (n === row.offload.layers) { offloadPending = null; paint(); return; } // released where it started
    // A guardrail-blocked position is NOT invoked: the form renders
    // the override/snap decision row for it instead.
    if (offloadIsUntested(row.offload, n)) {
      offloadPending = { backendId: id, layers: n };
      paint();
      return;
    }
    const ok = await saveOffloadPosition(id, n, false);
    if (!ok && !disposed) {
      // A rejected commit reverts the draft to the saved position.
      flagsOffloadLayers = row.offload.layers;
      paint();
    }
  };
  root.addEventListener("input", syncOffloadDraft);
  root.addEventListener("change", commitOffload);

  /* ---------------- MTP draft control (tracker #53) ----------------
   * A dropdown (the ctx/batch selects' idiom), so there is no
   * drag preview to sync: the "change" event does the whole
   * job — it moves the draft, updates the readouts in place,
   * and commits through backend_set_mtp, which rewrites only
   * the MTP tokens in the backend's raw flags. Value 0 commits
   * as Off. Choosing the saved value is a no-op. A rejected
   * commit reverts the draft to the saved value. The
   * --parallel conflict line follows the draft: it shows
   * whenever the value enables MTP and the SAVED flags set a
   * slot count other than 1 — the control never rewrites that
   * setting. */
  /** Invoke backend_set_mtp for one value and report the
   *  outcome on the card. Returns true when the value was
   *  saved. */
  const saveMtpPosition = async (id: string, pos: number): Promise<boolean> => {
    if (mtpCommitting) return false;
    mtpCommitting = true;
    inFlight.add(id);
    paint();
    try {
      const r = await setBackendMtp(id, pos === 0 ? null : pos);
      if (disposed) return false;
      if (r.error) {
        msgs.set(id, { text: `MTP draft not saved: ${r.error}`, err: true });
        paint();
        return false;
      }
      if (flagsFor === id) flagsMtpPosition = pos;
      msgs.set(id, {
        text: pos === 0
          ? "MTP draft turned off — applies on next start."
          : `MTP draft saved (${mtpTokenText(pos)}) — applies on next start.`,
        err: false,
      });
      await load();
      schedulePredictedLookup();
      return true;
    } finally {
      mtpCommitting = false;
      inFlight.delete(id);
      if (!disposed) paint();
    }
  };
  const changeMtp = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLSelectElement;
    if (!t.hasAttribute("data-mtp-select")) return;
    const id = t.getAttribute("data-mtp-backend") ?? "";
    const row = lastRows.find((r) => r.id === id);
    if (!row?.mtp || mtpCommitting) return;
    const n = Number(t.value);
    flagsMtpPosition = n;
    schedulePredictedLookup();
    const card = t.closest(".backend-card");
    if (card) {
      const token = card.querySelector("[data-mtp-token]");
      if (token) token.textContent = mtpTokenText(n);
      const conflict = card.querySelector("[data-mtp-conflict]") as HTMLElement | null;
      if (conflict) {
        const show = n > 0 && row.mtp.parallel != null && row.mtp.parallel !== 1;
        conflict.style.display = show ? "" : "none";
      }
    }
    if (n === mtpPosition(row.mtp)) { paint(); return; } // the saved value chosen again
    const ok = await saveMtpPosition(id, n);
    if (!ok && !disposed) {
      // A rejected commit reverts the draft to the saved value.
      flagsMtpPosition = mtpPosition(row.mtp);
      paint();
    }
  };
  root.addEventListener("change", changeMtp);

  /* ---------------- Parallel slots field (tracker #61) --------
   * The ctx/batch idiom with the sliders' commit model: the
   * draft syncs through syncFlagsDraft (the shared collection
   * logic), and a "change" on the preset select or the Custom
   * input commits through backend_set_parallel, which rewrites
   * only the --parallel tokens in the backend's raw flags.
   * Choosing Custom… only reveals the numeric input — the value
   * commits when the input itself changes. A rejected or invalid
   * commit reverts the draft to the saved selection. Setting the
   * field to 1 is also what clears the MTP conflict line: the
   * reload after the commit refreshes the payload it renders
   * from. */
  const saveParallelValue = async (id: string, n: number | null): Promise<boolean> => {
    if (parallelCommitting) return false;
    parallelCommitting = true;
    inFlight.add(id);
    paint();
    try {
      const r = await setBackendParallel(id, n);
      if (disposed) return false;
      if (r.error) {
        msgs.set(id, { text: `Parallel slots not saved: ${r.error}`, err: true });
        paint();
        return false;
      }
      if (flagsFor === id) flagsParallel = n;
      msgs.set(id, {
        text: n === null
          ? "Parallel slots returned to the llama.cpp default (auto) \u2014 applies on next start."
          : `Parallel slots saved (${parallelTokenText(n)}) \u2014 applies on next start.`,
        err: false,
      });
      await load();
      schedulePredictedLookup();
      return true;
    } finally {
      parallelCommitting = false;
      inFlight.delete(id);
      if (!disposed) paint();
    }
  };
  const commitParallel = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLElement;
    const field = t.getAttribute("data-flags-field");
    if (field !== "parallel-preset" && field !== "parallel-custom") return;
    if (!flagsFor || parallelCommitting) return;
    // Choosing Custom… reveals the numeric input; nothing commits
    // until the input itself changes.
    if (field === "parallel-preset" && (t as HTMLSelectElement).value === "custom") return;
    const id = flagsFor;
    const row = lastRows.find((r) => r.id === id);
    if (!row?.parallel) return;
    const saved = row.parallel.slots;
    const n = flagsParallel;
    if (n === saved) { paint(); return; } // changed back to the saved selection
    // The same bounds the set op validates, checked here so the
    // error reads in the form's own terms; the op re-validates
    // regardless.
    if (n !== null && (!Number.isInteger(n) || n < PARALLEL_MIN || n > PARALLEL_MAX)) {
      msgs.set(id, { text: `Parallel slots must be between ${PARALLEL_MIN} and ${PARALLEL_MAX.toLocaleString()}.`, err: true });
      flagsParallel = saved;
      paint();
      return;
    }
    const ok = await saveParallelValue(id, n);
    if (!ok && !disposed) {
      // A rejected commit reverts the draft to the saved selection.
      flagsParallel = saved;
      paint();
    }
  };
  root.addEventListener("change", commitParallel);

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
    // No-models state: a compact directive card — no form, no
    // disabled submit, no prose (principle 13).
    if (!lastModels.length) {
      addHost.innerHTML = `<div class="card"><h3>${icon("server")} Add a backend</h3>
        <div class="kv" style="margin-top:10px"><span class="k">Models</span><span class="v" style="font-size:12px">none registered</span></div>
        <div class="card-actions"><a class="btn" href="#/models">Add a model</a>
          <button class="btn icon-only" id="addBackCancel" title="Cancel" aria-label="Cancel">${icon("x")}</button></div></div>`;
      addHost.querySelector("#addBackCancel")?.addEventListener("click", closeAddForm);
      return;
    }
    const modelOpts = lastModels.map((m) => `<option value="${esc(m.id)}">${esc(modelShortName(m))}</option>`).join("");
    const groupOpts = lastGroups.map((g) => `<option value="${esc(g.name)}">${esc(g.name)}</option>`).join("")
      + `<option value="__new__">New group\u2026</option>`;
    addHost.innerHTML = `<div class="card"><h3>${icon("server")} Add a backend<span class="info-tip" title="The backend is created with the launch parameters below (32K context, 512 batch, and the model's catalog default offload unless changed); a tensor split is assigned from live GPU telemetry when available unless GPUs are picked under Placement. Joining an existing group makes the gateway share that group's traffic across its members; a new group gives the backend its own model name.">${icon("info")}</span></h3>
    <div style="display:grid;grid-template-columns:1fr 1fr;gap:10px;margin-top:10px">
      <label style="font-size:12px;color:var(--text2)">Name<span class="info-tip" title="lowercase, digits, hyphens">${icon("info")}</span><br>
        <input id="addBackName" type="text" placeholder="nemotron" style="${SELECT_STYLE};width:100%"></label>
      <label style="font-size:12px;color:var(--text2)">Model<br>
        <select id="addBackModel" style="${SELECT_STYLE};width:100%">${modelOpts}</select></label>
      <label style="font-size:12px;color:var(--text2)">Port<br>
        <input id="addBackPort" type="number" value="${nextFreePort()}" style="${SELECT_STYLE};width:100%"></label>
      <label style="font-size:12px;color:var(--text2)">Gateway group<br>
        <select id="addBackGroup" style="${SELECT_STYLE};width:100%">${groupOpts}</select></label>
    </div>
    <label id="addBackNewGroupWrap" style="font-size:12px;color:var(--text2);display:none;margin-top:10px">New group name<span class="info-tip" title="Defaults to the backend name.">${icon("info")}</span><br>
      <input id="addBackNewGroup" type="text" placeholder="nemotron" style="${SELECT_STYLE};width:100%"></label>
    <label style="font-size:12px;color:var(--text2);display:block;margin-top:10px" title="The gateway routes enabled backends; it can be started right away.">
      <input id="addBackEnabled" type="checkbox" checked> Enabled</label>
    <div id="addBackParams"></div>
    <div id="addBackStrip"></div>
    <div class="card-actions"><button class="btn" id="addBackSubmit">${icon("plus")} Add backend</button>
      <button class="btn icon-only" id="addBackCancel" title="Cancel" aria-label="Cancel">${icon("x")}</button></div>
    <div id="addBackMsg" style="font-size:12px;margin-top:6px"></div>
  </div>`;
    // Model summary strip: the model's size — the one creation
    // consequence not carried by a form field (the context lives
    // in the Launch parameters fields now, tracker #49, and a
    // value lives in one place). GPU/System RAM requirement tiles
    // need a propose-assignment payload for a not-yet-created
    // backend, which does not exist (see IMPLEMENTATION.md gaps) —
    // those tiles are omitted, never zero-filled.
    const stripEl = addHost.querySelector("#addBackStrip") as HTMLElement | null;
    const modelSelEl = addHost.querySelector("#addBackModel") as HTMLSelectElement | null;
    const paintStrip = (): void => {
      if (!stripEl || !modelSelEl) return;
      const m = lastModels.find((x) => x.id === modelSelEl.value);
      if (!m) { stripEl.innerHTML = ""; return; }
      const size = m.size_bytes ? fmtBytes(m.size_bytes)
        : m.catalog_size_bytes !== null ? `~${fmtBytes(m.catalog_size_bytes)} (estimate)` : null;
      stripEl.innerHTML = `<div style="display:grid;grid-template-columns:repeat(4,1fr);gap:10px;margin-top:10px">
        ${size !== null ? `<div class="kv"><span class="k">Size</span><span class="v" style="font-size:12px">${esc(size)}</span></div>` : ""}
      </div>`;
    };
    modelSelEl?.addEventListener("change", paintStrip);
    paintStrip();

    /* Launch parameters (tracker #49): the same fields the Edit
     * Flags and Placement forms expose for an existing backend,
     * rendered by the same helpers and collected into this draft
     * for backend_add. ctx/batch start at the creation defaults
     * and survive a model change (they are model-independent; only
     * their rated-context annotation follows the model). The
     * offload draft and the placement selection are
     * model-dependent: each backend_add_preview payload resets
     * them — the slider to the model's catalog default position,
     * the chips to the split creation would assign. */
    const pDraft: { ctx: number; batch: number; parallel: number | null } = { ctx: 32768, batch: 512, parallel: null };
    let preview: BackendAddPreview | null = null;
    let offLayers: number | null = null;
    /** A released offload position the guardrail blocks, awaiting
     *  the operator's decision in the form. `override` records the
     *  decision to create with an explicit override (backend_add
     *  journals it, as backend_set_offload does); without it the
     *  position must be snapped or moved before submit. */
    let offPending: { layers: number; override: boolean } | null = null;
    /** GPU indices the operator unchecked in the Placement chips.
     *  gpuTouched separates the previewed selection (creation
     *  auto-assigns) from an explicit one (creation writes the
     *  even split across it). */
    const gpuExcluded = new Set<number>();
    let gpuTouched = false;
    let previewSeq = 0;
    /** The predicted-throughput line for the add form's draft
     *  (design §6, shared with the Edit Flags form): the last
     *  measurement_lookup result for the configuration this
     *  form would create, debounced like the edit form's. */
    let addPredictedHtml = "";
    let addPredictedSeq = 0;
    let addPredictedTimer: number | null = null;
    const runAddLookup = async (): Promise<void> => {
      const modelId = modelSelEl?.value ?? "";
      if (!modelId) return;
      const seq = ++addPredictedSeq;
      const r = await measurementLookup({
        model_id: modelId,
        n_ctx: pDraft.ctx,
        n_batch: pDraft.batch,
        offload_layers: offLayers ?? preview?.offload?.layers ?? null,
        parallel_slots: pDraft.parallel,
        gpu_indices: gpuTouched ? selectedGpus() : null,
      });
      if (disposed || seq !== addPredictedSeq) return;
      addPredictedHtml = r.data ? predictedLineHtml(r.data.exact, r.data.near) : "";
      const slot = paramsEl?.querySelector("[data-add-predicted-slot]");
      if (slot) slot.innerHTML = addPredictedHtml;
    };
    const scheduleAddLookup = (): void => {
      if (addPredictedTimer !== null) window.clearTimeout(addPredictedTimer);
      addPredictedTimer = window.setTimeout(() => {
        addPredictedTimer = null;
        void runAddLookup();
      }, 300);
    };
    const paramsEl = addHost.querySelector("#addBackParams") as HTMLElement | null;
    const selectedGpus = (): number[] =>
      lastGpuList.map((g) => Number(g.id)).filter((ix) => !gpuExcluded.has(ix));
    const paintParams = (): void => {
      if (!paramsEl) return;
      const m = lastModels.find((x) => x.id === modelSelEl?.value);
      const ctxMax = m?.ctx_max ? m.ctx_max : null;
      // The offload slider, when the previewed model is
      // offload-capable. The decision row carries the edit form's
      // two resolving actions with creation wording: nothing can
      // be saved yet, so an override is recorded in the draft and
      // carried by backend_add; snapping moves the draft to the
      // nearest verified position.
      let offloadHtml = "";
      if (preview?.offload) {
        const o = preview.offload;
        const n = offLayers ?? o.layers;
        let decisionHtml = "";
        if (offPending && o.guardrail && offloadIsUntested(o, offPending.layers)) {
          decisionHtml = offPending.override
            ? `<div data-add-offload-decision style="display:flex;gap:8px;align-items:center;flex-wrap:wrap;margin-top:8px">
      <span data-add-decision-text style="font-size:12px;color:#e3b341">${offPending.layers} of ${o.moe_layers} layers is unverified on this GPU \u2014 the backend will be created with an override recorded</span>
      <button class="btn" data-add-decision="snap">${icon("check")} Snap to ${o.guardrail.nearest_verified_layers} of ${o.moe_layers} layers</button>
    </div>`
            : `<div data-add-offload-decision style="display:flex;gap:8px;align-items:center;flex-wrap:wrap;margin-top:8px">
      <span data-add-decision-text style="font-size:12px;color:#e3b341">${offPending.layers} of ${o.moe_layers} layers is unverified on this GPU</span>
      <button class="btn" data-add-decision="override">${icon("warn")} Create with override</button>
      <button class="btn" data-add-decision="snap">${icon("check")} Snap to ${o.guardrail.nearest_verified_layers} of ${o.moe_layers} layers</button>
    </div>`;
        }
        offloadHtml = offloadSliderHtml({ offload: o, layers: n, ownerId: "new backend", disabled: false, decisionHtml });
      }
      // Placement chips: the Placement form's include/exclude
      // chips over the telemetry GPUs, defaulting to the split
      // creation would assign.
      let placeHtml = "";
      if (lastGpuList.length) {
        const split = preview?.tensor_split ?? [];
        const summary = lastGpuList
          .map((g, p) => ({ ix: Number(g.id), frac: split[p] ?? 0 }))
          .filter((e) => e.frac > 0)
          .map((e) => `GPU ${e.ix} \u2014 ${Math.round(e.frac * 100)}%`)
          .join(" \u00b7 ");
        placeHtml = `<div style="margin-top:12px">
        <span style="font-size:12px;color:var(--text2)">Placement (tensor split)</span><span class="info-tip" title="The split divides the model's weights across the selected GPUs, in nvidia-smi index order. The selection starts at the split a creation would assign from live telemetry; changing it makes the split an even division across the selected GPUs instead.">${icon("info")}</span>
        <div class="gpu-chip-row">${lastGpuList.map((g) => {
          const ix = Number(g.id);
          const on = !gpuExcluded.has(ix);
          return `<label class="gpu-chip${on ? " on" : ""}" title="${esc(g.name)}"><span class="gpu-chip-top"><input type="checkbox" data-add-place-gpu="${ix}"${on ? " checked" : ""}> GPU ${ix}</span><span class="gpu-chip-name">${esc(gpuShortName(g.name))}</span></label>`;
        }).join("")}</div>
        ${summary ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">Split: ${esc(summary)}</div>` : ""}
      </div>`;
      } else {
        placeHtml = `<div style="font-size:12px;color:var(--text2);margin-top:12px">Placement: auto<span class="info-tip" title="No GPU telemetry yet \u2014 a tensor split cannot be previewed or selected; creation leaves the split unset, as it does today.">${icon("info")}</span></div>`;
      }
      paramsEl.innerHTML = `<div style="margin-top:12px;border-top:1px solid var(--border);padding-top:10px">
      <div style="font-size:12px;font-weight:600">${icon("gear")} Launch parameters</div>
      ${flagsCtxBatchHtml({ ctx: pDraft.ctx, batch: pDraft.batch, ctxMax, fieldAttr: "data-add-flags-field", disabled: false })}
      ${flagsParallelHtml({ value: pDraft.parallel, savedCustom: false, fieldAttr: "data-add-flags-field", disabled: false })}
      ${offloadHtml}
      ${flagsRatedLineHtml({ ctxMax, modelId: m?.id ?? null, savedText: "Saved when the backend is added; applies from its first start. Placement is set by the GPU selection below." })}
      <div data-add-predicted-slot>${addPredictedHtml}</div>
      ${placeHtml}
    </div>`;
    };
    const resetPlacementFromPreview = (): void => {
      gpuExcluded.clear();
      gpuTouched = false;
      const split = preview?.tensor_split ?? [];
      lastGpuList.forEach((g, p) => {
        // No assigned split (single GPU, or none yet): an unsplit
        // creation loads on the first GPU — the Placement form's
        // fallback selection for the same state.
        const on = split.length ? (split[p] ?? 0) > 0 : p === 0;
        if (!on) gpuExcluded.add(Number(g.id));
      });
    };
    const loadPreview = async (modelChanged: boolean): Promise<void> => {
      const modelId = modelSelEl?.value ?? "";
      if (!modelId) return;
      const seq = ++previewSeq;
      const r = await backendAddPreview(modelId, gpuTouched ? selectedGpus() : undefined);
      if (disposed || seq !== previewSeq) return;
      if (r.error || !r.data) { preview = null; paintParams(); scheduleAddLookup(); return; }
      preview = r.data;
      if (modelChanged) {
        offLayers = preview.offload ? preview.offload.layers : null;
        offPending = null;
        resetPlacementFromPreview();
      }
      paintParams();
      scheduleAddLookup();
    };
    modelSelEl?.addEventListener("change", () => { void loadPreview(true); });
    paramsEl?.addEventListener("input", (ev) => {
      const t = ev.target as HTMLElement;
      if (t.hasAttribute("data-offload-slider")) {
        if (!preview?.offload) return;
        const n = Number((t as HTMLInputElement).value);
        offLayers = n;
        scheduleAddLookup();
        const card = t.closest(".card");
        if (card) paintOffloadReadout(card, preview.offload, n);
        // Keep a pending decision in step with the draft (the
        // edit form's rule): its text follows a still-blocked
        // position, and the row hides while the draft sits on a
        // verified position; the release handler re-decides.
        if (offPending) {
          const dec = paramsEl.querySelector("[data-add-offload-decision]") as HTMLElement | null;
          if (dec) {
            if (offloadIsUntested(preview.offload, n)) {
              offPending.layers = n;
              dec.style.display = "";
              const dt = dec.querySelector("[data-add-decision-text]");
              if (dt) dt.textContent = `${n} of ${preview.offload.moe_layers} layers is unverified on this GPU${offPending.override ? " \u2014 the backend will be created with an override recorded" : ""}`;
            } else {
              dec.style.display = "none";
            }
          }
        }
        return;
      }
      const field = t.getAttribute("data-add-flags-field");
      if (field) { applyFlagsField(pDraft, field, (t as HTMLInputElement).value, false); scheduleAddLookup(); }
    });
    paramsEl?.addEventListener("change", (ev) => {
      const t = ev.target as HTMLElement;
      if (t.hasAttribute("data-offload-slider")) {
        if (!preview?.offload) return;
        const n = Number((t as HTMLInputElement).value);
        offLayers = n;
        // Release decides the pending state: a guardrail-blocked
        // position becomes a decision the operator must make (an
        // override confirmed for one position does not carry to
        // another); a verified position clears it.
        if (offloadIsUntested(preview.offload, n)) {
          if (!offPending || offPending.layers !== n) offPending = { layers: n, override: false };
        } else {
          offPending = null;
        }
        paintParams();
        scheduleAddLookup();
        return;
      }
      if (t.hasAttribute("data-add-place-gpu")) {
        const ix = Number(t.getAttribute("data-add-place-gpu"));
        if ((t as HTMLInputElement).checked) gpuExcluded.delete(ix);
        else gpuExcluded.add(ix);
        gpuTouched = true;
        // The placement drives the guardrail evaluation: refresh
        // the preview against the explicit selection.
        void loadPreview(false);
        return;
      }
      const field = t.getAttribute("data-add-flags-field");
      if (field && applyFlagsField(pDraft, field, (t as HTMLInputElement).value, true)) { paintParams(); scheduleAddLookup(); }
    });
    paramsEl?.addEventListener("click", (ev) => {
      const t = (ev.target as HTMLElement).closest("[data-add-decision]") as HTMLElement | null;
      if (!t || !preview?.offload || !offPending) return;
      if (t.getAttribute("data-add-decision") === "override") {
        offPending.override = true;
      } else {
        // Snap: move the draft to the nearest verified position.
        offLayers = preview.offload.guardrail?.nearest_verified_layers ?? offPending.layers;
        offPending = null;
      }
      paintParams();
    });
    paintParams();
    void loadPreview(true);
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
      // Launch parameters: the same validation the create op
      // applies (context range; batch within 1..=context), checked
      // here so the error reads in the form's own terms; the op
      // re-validates regardless.
      if (!Number.isInteger(pDraft.ctx) || pDraft.ctx < 1024 || pDraft.ctx > 1048576) {
        if (msg) msg.innerHTML = `<span style="color:var(--err)">Context length must be between 1,024 and 1,048,576.</span>`;
        return;
      }
      if (!Number.isInteger(pDraft.batch) || pDraft.batch < 1 || pDraft.batch > pDraft.ctx) {
        if (msg) msg.innerHTML = `<span style="color:var(--err)">Batch size must be between 1 and the context length.</span>`;
        return;
      }
      // Parallel slots: the same bounds the create op applies
      // (backend_set_parallel's range), checked here in the form's
      // terms; an untouched field (null) sends nothing and the
      // backend is created with no --parallel token.
      if (pDraft.parallel !== null && (!Number.isInteger(pDraft.parallel) || pDraft.parallel < PARALLEL_MIN || pDraft.parallel > PARALLEL_MAX)) {
        if (msg) msg.innerHTML = `<span style="color:var(--err)">Parallel slots must be between ${PARALLEL_MIN} and ${PARALLEL_MAX.toLocaleString()}.</span>`;
        return;
      }
      // An undecided guardrail-blocked offload position cannot be
      // carried into creation (the op would refuse it without the
      // override): the operator decides here first.
      if (offPending && !offPending.override) {
        if (msg) msg.innerHTML = `<span style="color:var(--err)">Offload position ${offPending.layers} of ${preview?.offload?.moe_layers ?? ""} layers is unverified on this GPU \u2014 create with an override or snap to a verified position.</span>`;
        return;
      }
      if (gpuTouched && !selectedGpus().length) {
        if (msg) msg.innerHTML = `<span style="color:var(--err)">Select at least one GPU for the split.</span>`;
        return;
      }
      submit.disabled = true;
      const r = await addBackend({
        id: name, modelId, port, group, createGroup, enabled,
        nCtx: pDraft.ctx,
        nBatch: pDraft.batch,
        offloadLayers: preview?.offload ? (offLayers ?? preview.offload.layers) : undefined,
        offloadOverride: offPending?.override ? true : undefined,
        gpuIndices: gpuTouched ? selectedGpus() : undefined,
        parallel: pDraft.parallel ?? undefined,
      });
      if (disposed) return;
      if (r.error) {
        if (msg) msg.innerHTML = `<span style="color:var(--err)">Add failed: ${esc(r.error)}</span>`;
        submit.disabled = false;
        return;
      }
      const modelRow = lastModels.find((x) => x.id === modelId);
      addHost.innerHTML = `<div class="card"><h3>${icon("check")} Backend added</h3>
        <div style="display:grid;gap:4px;margin-top:8px;font-size:12px">
          <div><span style="color:var(--text2)">Backend:</span> ${esc(name)}</div>
          <div><span style="color:var(--text2)">Model:</span> ${esc(modelRow ? modelShortName(modelRow) : modelId)}</div>
          <div><span style="color:var(--text2)">Port:</span> ${port}</div>
          <div><span style="color:var(--text2)">Group:</span> ${esc(group)}</div>
          <div><span style="color:var(--text2)">Enabled:</span> ${enabled ? "yes" : "no"}</div>
          <div><span style="color:var(--text2)">Context:</span> ${pDraft.ctx.toLocaleString()}</div>
          <div><span style="color:var(--text2)">Batch:</span> ${pDraft.batch.toLocaleString()}</div>
          ${pDraft.parallel !== null ? `<div><span style="color:var(--text2)">Parallel slots:</span> ${pDraft.parallel}</div>` : ""}
          ${preview?.offload ? `<div><span style="color:var(--text2)">Offload:</span> ${esc(offloadTokenText(preview.offload, offLayers ?? preview.offload.layers))}${offPending?.override ? " (override recorded)" : ""}</div>` : ""}
          ${gpuTouched ? `<div><span style="color:var(--text2)">GPUs:</span> ${esc(selectedGpus().map((ix) => `GPU ${ix}`).join(", "))}</div>` : ""}
          <div><span style="color:var(--text2)">Activation plan:</span> recorded<span class="info-tip" title="The change is recorded in the activation plan at the top of this page — the gateway routes the new backend after the plan is executed.">${icon("info")}</span></div>
        </div>
        <div class="card-actions"><button class="btn icon-only" id="addBackDone" title="Done" aria-label="Done">${icon("check")}</button></div></div>`;
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

    if (act === "details") {
      if (openDetails.has(id)) openDetails.delete(id); else openDetails.add(id);
      paint();
      return;
    }

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
      // The projection prices the model it was proposed for. If the
      // backend's model changed since (a model assignment reloads
      // the rows), the projection is stale: drop it rather than
      // applying the old model's split to the new model
      // (code-review 5.7).
      const prow = lastRows.find((r) => r.id === id);
      const currentModel = lastModels.find((m) => prow !== undefined && sameModelFile(prow.model_file, m.gguf_path));
      if (!prow || !currentModel || currentModel.id !== proj.modelId) {
        msgs.set(id, { text: "The backend's model changed since this split was proposed — propose again to price the current model.", err: true });
        placeProjection = null;
        paint();
        return;
      }
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
        } else if (r.data.persisted === false) {
          // Applied in memory only (code-review 5.2): say so on
          // the card instead of the saved message — the split is
          // lost when the app restarts.
          msgs.set(id, { text: "Split applied for this session only — it was not saved and will be lost when the app restarts.", err: true });
          placeFor = null;
          placeProjection = null;
          placeExcluded.clear();
          await load();
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
      } else if (r.data.persisted === false) {
        // Code-review 5.2: a non-persisted save is not a save.
        msgs.set(id, { text: "Flags applied for this session only — they were not saved and will be lost when the app restarts.", err: true });
        flagsFor = null;
        await load();
      } else {
        msgs.set(id, { text: `Flags saved (context ${flagsCtx.toLocaleString()}, batch ${flagsBatch}) — applies on next start.`, err: false });
        flagsFor = null;
        await load();
      }
      return;
    }

    if (act === "flags-mode-advanced") {
      // Enter advanced flags mode (tracker #60). The flip writes
      // only the mode — the saved flags do not change — but it
      // closes the structured form, so unsaved structured edits
      // (a moved ctx/batch draft or a pending offload decision)
      // are discarded: confirm first, per the app's confirm
      // idiom, only when there is something to lose.
      const row = lastRows.find((r) => r.id === id);
      if (!row) return;
      const dirty = flagsCtx !== (row.n_ctx ?? 32768)
        || flagsBatch !== (row.n_batch ?? 512)
        || offloadPending !== null;
      if (dirty && !confirm("Switch to advanced flags? Unsaved changes in this form will be discarded. The saved flags themselves do not change.")) return;
      inFlight.add(id);
      paint();
      try {
        const r = await setBackendFlagsMode(id, true);
        if (disposed) return;
        if (r.error) {
          msgs.set(id, { text: `Advanced mode not enabled: ${r.error}`, err: true });
          paint();
          return;
        }
        msgs.set(id, { text: "Advanced flags mode on — the flags are edited as text in the panel.", err: false });
        await load();
        // Reopen the panel in its advanced form, on the saved
        // string the mode flip returned (byte-identical to what
        // was live — entering the mode writes no flags).
        const fresh = lastRows.find((x) => x.id === id);
        if (fresh) { flagsFor = null; openFlagsForm(fresh); }
      } finally {
        inFlight.delete(id);
        if (!disposed) paint();
      }
      return;
    }

    if (act === "flags-mode-structured") {
      // Leave advanced mode (tracker #60): the structured form
      // decodes the SAVED raw string through the existing
      // decoders, so unsaved text edits would be lost — confirm
      // when the text differs from the saved string.
      const row = lastRows.find((r) => r.id === id);
      if (!row) return;
      if (flagsRawText !== (row.raw_flags ?? "")
        && !confirm("Return to the structured form? The unsaved flag edits in the text box will be discarded. The saved flags do not change.")) return;
      inFlight.add(id);
      paint();
      try {
        const r = await setBackendFlagsMode(id, false);
        if (disposed) return;
        if (r.error) {
          msgs.set(id, { text: `Structured form not restored: ${r.error}`, err: true });
          paint();
          return;
        }
        msgs.set(id, { text: "Structured flags form restored — it shows the values decoded from the saved flags.", err: false });
        await load();
        const fresh = lastRows.find((x) => x.id === id);
        if (fresh) { flagsFor = null; openFlagsForm(fresh); }
      } finally {
        inFlight.delete(id);
        if (!disposed) paint();
      }
      return;
    }

    if (act === "flags-raw-save") {
      // Save the advanced editor's text (tracker #60). The op
      // journals like the other flags writes and applies on the
      // next start; nothing restarts. Warnings ride the result
      // and never block — the panel adopts the server's list.
      inFlight.add(id);
      paint();
      try {
        const text = flagsRawText;
        const r = await setBackendRawFlags(id, text.trim() === "" ? null : text);
        if (disposed) return;
        if (r.error) {
          msgs.set(id, { text: `Flags not saved: ${r.error}`, err: true });
          paint();
          return;
        }
        flagsRawText = r.data.raw_flags ?? "";
        flagsRawWarnings = r.data.warnings ?? [];
        msgs.set(id, {
          text: r.data.changed
            ? "Raw flags saved — applies on next start."
            : "Raw flags unchanged.",
          err: false,
        });
        await load();
        schedulePredictedLookup();
      } finally {
        inFlight.delete(id);
        if (!disposed) paint();
      }
      return;
    }

    if (act === "offload-override") {
      // Save the pending guardrail-blocked position with an explicit
      // override. A server error surfaces on the card and the
      // decision row stays (the helper keeps the pending state and
      // the draft), so the operator can still snap instead.
      const pending = offloadPending?.backendId === id ? offloadPending : null;
      if (!pending) return;
      await saveOffloadPosition(id, pending.layers, true);
      return;
    }

    if (act === "offload-snap") {
      // Move the draft to the nearest verified position and save it
      // without an override. On error the draft returns to the
      // blocked position, keeping the decision row coherent.
      const pending = offloadPending?.backendId === id ? offloadPending : null;
      const row = lastRows.find((r) => r.id === id);
      const target = row?.offload?.guardrail?.nearest_verified_layers;
      if (!pending || !row?.offload || target === undefined) return;
      flagsOffloadLayers = target;
      const ok = await saveOffloadPosition(id, target, false);
      if (!ok && !disposed) {
        flagsOffloadLayers = pending.layers;
        paint();
      }
      return;
    }

    if (act === "offload-set-verified") {
      // The blocked card line's resolving action: save the nearest
      // verified position (no override). On error the helper has
      // already surfaced the message on the card.
      const row = lastRows.find((r) => r.id === id);
      const target = row?.offload?.guardrail?.nearest_verified_layers;
      if (!row?.offload || target === undefined) return;
      await saveOffloadPosition(id, target, false);
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
        else if (r.data.persisted === false) {
          // Code-review 5.2: warn instead of silently succeeding.
          msgs.set(id, { text: `${act === "enable" ? "Enabled" : "Disabled"} for this session only — the change was not saved and will be lost when the app restarts.`, err: true });
        } else msgs.delete(id);
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
      if (act === "toolcall") {
        // The probe persists its outcome on the backend (the card's
        // Tool calls line), so a reload shows the measured state;
        // the probe's own detail rides the card note meanwhile.
        const r = await testBackendToolCall(id);
        if (disposed) return;
        if (r.error) msgs.set(id, { text: `Tool-call test failed: ${r.error}`, err: true });
        else msgs.set(id, { text: `Tool calls: ${r.data.outcome} — ${r.data.detail}`, err: r.data.outcome !== "verified" });
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
    return `<div class="ov-row">${btn}<span class="ov-name" title="${esc(m.id)}">${esc(modelShortName(m))}</span><span class="ov-dim">${esc(m.quant)}</span><span class="ov-dim">${esc(size)}</span><span class="ov-dim">${esc(stateTxt)}</span><span class="ov-dim">${st ? String(st.backends) : "0"}</span><span class="ov-dim">${st?.req !== undefined ? st.req.toLocaleString() : "\u2014"}</span></div>`;
  }).join("");
  return `<div class="ovlist ov-m"><div class="ov-head"><span></span><span>Model</span><span>Quant</span><span>Size</span><span>State</span><span>Serving backends</span><span>Requests (1h)</span></div>${body}</div>`;
}

function modelCard(m: ModelRow, downloading: Set<string>, notes: Map<string, ModelNote>, stats?: { backends: number; req?: number; err?: number; toks?: number }, rename?: { draft: string } | null, detailsOpen = false): string {
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
  const dlTotal = m.on_disk ? m.size_bytes : m.catalog_size_bytes;
  const progressHtml = (inFlight || partial > 0) && dlTotal !== null && dlTotal > 0 && partial > 0
    ? `<div style="display:flex;gap:10px;align-items:center;margin-top:8px">
        <span style="flex:1">${meter((partial / dlTotal) * 100, 300, 8, C.blue)}</span>
        <span style="font-size:12px;color:var(--text2)">Downloaded: ${esc(fmtBytes(partial))} of ${m.on_disk ? "" : "~"}${esc(fmtBytes(dlTotal))}${m.on_disk ? "" : " (estimate)"}</span>
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
  const renameHtml = rename ? `<div class="backend-form-panel flags-panel">
      <div style="font-size:12px;font-weight:600">Rename short name</div>
      <div style="display:flex;gap:6px;margin-top:6px;min-width:0">
        <input data-model-rename-input="${esc(m.id)}" type="text" value="${esc(rename.draft)}" maxlength="64" style="${SELECT_STYLE};flex:1;min-width:0">
      </div>
      <div class="card-actions"><button class="btn" data-model-act="rename-save" data-model-id="${esc(m.id)}">${icon("check")} Save name</button>
        <button class="btn icon-only" data-model-act="rename-cancel" data-model-id="${esc(m.id)}" title="Cancel" aria-label="Cancel">${icon("x")}</button></div>
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
        <div class="kv"><span class="k">Requests (1h)</span><span class="v" style="font-size:12px">${stats.req.toLocaleString()}</span></div>
        ${stats.err !== undefined ? `<div class="kv"><span class="k">Errors (1h)</span><span class="v" style="font-size:12px">${stats.err.toLocaleString()}</span></div>` : ""}
        ${stats.toks !== undefined ? `<div class="kv"><span class="k">Throughput</span><span class="v" style="font-size:12px">${trim1(stats.toks)} tok/s</span></div>` : ""}
      </div>
      ${stats.err !== undefined && stats.req > 0 ? stackedBar([
        { pct: (stats.err / stats.req) * 100, color: C.red, label: `Errors: ${stats.err.toLocaleString()} of ${stats.req.toLocaleString()}` },
        { pct: ((stats.req - stats.err) / stats.req) * 100, color: "#3a4350", label: `Succeeded: ${(stats.req - stats.err).toLocaleString()} of ${stats.req.toLocaleString()}` },
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
      ${m.ctx_max != null ? `<div class="kv"><span class="k">Context</span><span class="v" style="font-size:12px">${m.ctx_max.toLocaleString()}</span></div>` : ""}
    </div>
    ${statsHtml}
    <div class="card-actions">${actions}</div>
    ${detailsHtml}
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
  <div class="card"><h3>${icon("box")} Add a model<span class="info-tip" title="The model is added enabled, with no pinned SHA-256 — its downloads run unverified until a hash is recorded in the configuration.">${icon("info")}</span></h3>
    <div style="display:grid;grid-template-columns:1fr 2fr;gap:10px;margin-top:10px">
      <label style="font-size:12px;color:var(--text2)">Model id<br>
        <input id="addModelId" type="text" placeholder="my-model-7b" style="${SELECT_STYLE};width:100%"></label>
      <label style="font-size:12px;color:var(--text2)">Source URL<br>
        <input id="addModelUrl" type="text" placeholder="Model download URL (.gguf)" style="${SELECT_STYLE};width:100%"></label>
    </div>
    <div style="margin-top:10px"><button class="btn" id="addModelOptToggle" aria-expanded="false" title="Optional fields" aria-label="Optional fields" style="padding:2px 8px;font-size:11px">${icon("chevR")}</button></div>
    <div id="addModelOptional" style="display:none">
      <div style="display:grid;grid-template-columns:1fr 1fr 1fr;gap:10px;margin-top:10px">
        <label style="font-size:12px;color:var(--text2)">Quant<br>
          <input id="addModelQuant" type="text" placeholder="Q4_K_M" style="${SELECT_STYLE};width:100%"></label>
        <label style="font-size:12px;color:var(--text2)">File name<span class="info-tip" title="Defaults to the URL's file name.">${icon("info")}</span><br>
          <input id="addModelFile" type="text" placeholder="model.gguf" style="${SELECT_STYLE};width:100%"></label>
        <label style="font-size:12px;color:var(--text2)">Parameters (B)<span class="info-tip" title="Total parameter count in billions (e.g. 80 for an 80B model — for a mixture-of-experts model, the total, not the active count). Required for models outside the curated catalog: the count feeds memory-fit estimation and is never re-derived later.">${icon("info")}</span><br>
          <input id="addModelParams" type="text" inputmode="decimal" placeholder="80" style="${SELECT_STYLE};width:100%"></label>
      </div>
    </div>
    <div class="card-actions"><button class="btn" id="addModelBtn">${icon("plus")} Add model</button></div>
    <div id="addModelMsg" style="font-size:12px;margin-top:6px;display:none"></div>
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
      b.innerHTML = `<div class="cards-fluid">${lastRows.map((m) => modelCard(m, downloading, notes, modelStats.get(m.id), renameFor === m.id ? { draft: renameDraft } : null, openModelDetails.has(m.id))).join("")}</div>`;
    }
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

  const optToggle = root.querySelector("#addModelOptToggle") as HTMLButtonElement | null;
  optToggle?.addEventListener("click", () => {
    const box = root.querySelector("#addModelOptional") as HTMLElement | null;
    if (!box) return;
    const open = box.style.display === "none";
    box.style.display = open ? "" : "none";
    optToggle.setAttribute("aria-expanded", String(open));
    optToggle.innerHTML = icon(open ? "chevD" : "chevR");
  });

  const addBtn = root.querySelector("#addModelBtn") as HTMLButtonElement | null;
  addBtn?.addEventListener("click", async () => {
    const msg = root.querySelector("#addModelMsg") as HTMLElement | null;
    // The message row stays hidden until its first write.
    if (msg) msg.style.display = "";
    const val = (sel: string): string => (root.querySelector(sel) as HTMLInputElement | null)?.value.trim() ?? "";
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
    addBtn.disabled = true;
    const r = await modelAdd(id, url, val("#addModelFile") || undefined, val("#addModelQuant") || undefined, paramsB);
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

/** Logs view control state (tracker #59): everything the filter
 *  card holds, kept at module level so leaving the view and coming
 *  back within the same app run restores it exactly as left. An
 *  app restart re-creates the module and returns to the defaults
 *  below. While the view is wired the controls are the runtime
 *  truth; wireLogs mirrors them into this object on every change
 *  and once more from its cleanup. */
interface LogsViewState {
  scope: string;
  level: string;
  query: string;
  from: string;
  to: string;
  sort: "asc" | "desc";
  limit: number;
  follow: boolean;
  page: number;
}

function defaultLogsViewState(): LogsViewState {
  return { scope: "all", level: "", query: "", from: "", to: "", sort: "desc", limit: 200, follow: false, page: 1 };
}

let logsViewState: LogsViewState = defaultLogsViewState();

/** One recorded Logs search set (tracker #59): an applied query —
 *  every getLogs parameter except the pagination offset — plus
 *  when it last ran. Follow is view state, not part of a set:
 *  applying a set never changes the Follow toggle. */
interface LogSearchSet {
  scope: string;
  level: string;
  query: string;
  from: string;
  to: string;
  sort: "asc" | "desc";
  limit: number;
  ranAt: string;
}

const LOGS_RECENTS_KEY = "llm.logs.recents";
const LOGS_RECENTS_CAP = 30;

/** Recents persist across restarts in localStorage (the dashboard
 *  view-mode pattern): 30 small entries at most. Stored data is
 *  validated field by field; any storage problem degrades to an
 *  empty history rather than breaking the view. */
function loadLogsRecents(): LogSearchSet[] {
  try {
    const raw = localStorage.getItem(LOGS_RECENTS_KEY);
    if (!raw) return [];
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    const out: LogSearchSet[] = [];
    for (const e of parsed) {
      if (!e || typeof e !== "object") continue;
      const r = e as Record<string, unknown>;
      if (typeof r.scope !== "string" || typeof r.ranAt !== "string") continue;
      out.push({
        scope: r.scope,
        level: typeof r.level === "string" ? r.level : "",
        query: typeof r.query === "string" ? r.query : "",
        from: typeof r.from === "string" ? r.from : "",
        to: typeof r.to === "string" ? r.to : "",
        sort: r.sort === "asc" ? "asc" : "desc",
        limit: typeof r.limit === "number" && [50, 200, 500].includes(r.limit) ? r.limit : 200,
        ranAt: r.ranAt,
      });
      if (out.length >= LOGS_RECENTS_CAP) break;
    }
    return out;
  } catch { return []; }
}

let logsRecents: LogSearchSet[] = loadLogsRecents();
/** Whether the Recent searches section is expanded — session-only
 *  view state. The section itself renders only while the history is
 *  non-empty (principle 13). */
let logsRecentsOpen = false;

function saveLogsRecents(): void {
  try { localStorage.setItem(LOGS_RECENTS_KEY, JSON.stringify(logsRecents)); } catch { /* storage unavailable */ }
}

/** A set's identity: the query fields only — ranAt is metadata, so
 *  re-running the same set dedupes instead of duplicating. */
function logSearchKey(s: LogSearchSet): string {
  return JSON.stringify([s.scope, s.level, s.query, s.from, s.to, s.sort, s.limit]);
}

/** Record an applied search set: newest first; an identical set
 *  moves to the top with a fresh ranAt; the oldest drop past the
 *  cap. */
function recordLogSearch(set: LogSearchSet): void {
  const key = logSearchKey(set);
  logsRecents = [set, ...logsRecents.filter((e) => logSearchKey(e) !== key)].slice(0, LOGS_RECENTS_CAP);
  saveLogsRecents();
}

/** One-line summary of a search set: the scope, then only the
 *  distinguishing values (defaults omitted), so entries scan. */
function logSearchSummary(s: LogSearchSet): string {
  const parts: string[] = [s.scope];
  if (s.level) parts.push(`level ${s.level}`);
  if (s.query) parts.push(`"${s.query}"`);
  if (s.from || s.to) parts.push(`${s.from ? fmtRanAt(s.from) : "…"} → ${s.to ? fmtRanAt(s.to) : "…"}`);
  if (s.sort === "asc") parts.push("oldest first");
  if (s.limit !== 200) parts.push(`limit ${s.limit}`);
  return parts.join(" · ");
}

/** A timestamp compactly — "18 Oct 22:14" (the full stamp rides
 *  the title attribute where one is offered). Accepts ISO stamps
 *  and datetime-local values alike. */
function fmtRanAt(ts: string): string {
  const d = new Date(ts);
  return Number.isNaN(d.getTime())
    ? ts
    : d.toLocaleString("en-GB", { day: "2-digit", month: "short", hour: "2-digit", minute: "2-digit", hour12: false });
}

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
    <label style="font-size:12px;color:var(--text2);display:inline-flex;align-items:center;gap:6px">Scope<span class="info-tip" title="Backend logs are kept in memory for this session only — up to 10,000 lines per process; one previous run per process is kept. The gateway log is file-backed (under the data directory's logs folder) and survives restarts.">${icon("info")}</span>
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
    <button class="btn" id="logFromCal" type="button" title="Pick a date and time" aria-label="Pick a date and time" style="padding:4px 7px">${icon("calendar")}</button>
    <label style="font-size:12px;color:var(--text2);display:inline-flex;align-items:center;gap:6px">To
      <input id="logTo" type="datetime-local" style="${SELECT_STYLE}"/></label>
    <button class="btn" id="logToCal" type="button" title="Pick a date and time" aria-label="Pick a date and time" style="padding:4px 7px">${icon("calendar")}</button>
    <label style="font-size:12px;color:var(--text2);display:inline-flex;align-items:center;gap:6px">Sort
      <select id="logSort" style="${SELECT_STYLE}">
        <option value="desc" selected>Newest first</option>
        <option value="asc">Oldest first</option>
      </select></label>
    <label style="font-size:12px;color:var(--text2);display:inline-flex;align-items:center;gap:6px">
      <input id="logFollow" type="checkbox"/> Follow</label>
  </div>
  <div id="logMsg" style="font-size:12px;margin-top:6px"></div>
  <div style="display:flex;gap:10px;align-items:center;flex-wrap:wrap;border-top:1px solid var(--border);margin-top:10px;padding-top:10px">
    <button class="btn icon-only" id="logPrev" title="Previous page" aria-label="Previous page">${icon("chevL")}</button>
    <button class="btn icon-only" id="logNext" title="Next page" aria-label="Next page">${icon("chevR")}</button>
    <span id="logPageInfo" style="font-size:12px;color:var(--text2)"></span>
    <span id="logPosBar" style="display:inline-block;width:64px"></span>
    <span id="logMatchInfo" style="font-size:12px;color:var(--text2)"></span>
    <span class="spacer" style="flex:1"></span>
    <label style="font-size:12px;color:var(--text2);display:inline-flex;align-items:center;gap:6px">Limit
      <select id="logLimit" style="${SELECT_STYLE}">
        <option value="50">50</option>
        <option value="200" selected>200</option>
        <option value="500">500</option>
      </select></label>
    <button class="btn icon-only" id="logRefresh" title="Refresh" aria-label="Refresh">${icon("restart")}</button>
    <button class="btn icon-only" id="logExport" title="Export diagnostics bundle — the bundle contains raw, unredacted process logs. Review it before sharing." aria-label="Export diagnostics bundle">${icon("download")}</button>
    <button class="btn icon-only" id="logOpenFolder" title="Open folder" aria-label="Open folder" style="display:none">${icon("folder")}</button>
  </div></div>
  <div id="logRecents"></div>
  <div id="logBody">${loadingHtml("Loading logs\u2026")}</div></div>`;
}

export async function wireLogs(root: HTMLElement, initialScope?: string, initialFollow = false): Promise<() => void> {
  let disposed = false;
  /** Load coalescing (code-review 5.1): every load takes a
   *  sequence number and only the LATEST load paints. The previous
   *  boolean in-flight guard silently DROPPED a page/filter change
   *  made while a load was running — the view kept showing the old
   *  page under the new filters until some later action. This is
   *  the same pattern the Backends view uses. */
  let loadSeq = 0;
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
  const matchInfo = root.querySelector("#logMatchInfo");
  const posBar = root.querySelector("#logPosBar") as HTMLElement | null;
  const exportBtn = root.querySelector("#logExport") as HTMLButtonElement | null;

  /** A datetime-local value as epoch ms (local time); empty or
   *  unparseable means unbounded. */
  const msOf = (v: string | undefined): number | undefined => {
    if (!v) return undefined;
    const ms = new Date(v).getTime();
    return Number.isNaN(ms) ? undefined : ms;
  };

  /** The "To" bound is minute-precision: it covers the whole
   *  picked minute, not just its first instant — lines stamped
   *  later in that minute were previously excluded, so a range
   *  ending "now" (to the minute) silently dropped the newest
   *  lines (code-review 5.5). */
  const untilMsOf = (v: string | undefined): number | undefined => {
    const ms = msOf(v);
    return ms === undefined ? undefined : ms + 59_999;
  };

  const msg = (text: string, isErr = false): void => {
    const m = root.querySelector("#logMsg");
    if (m) m.innerHTML = text ? `<span style="color:${isErr ? "var(--err)" : "var(--text2)"}">${esc(text)}</span>` : "";
  };

  /* ---- Date/time picker for the From/To inputs (hand-rolled) ----
   * The inputs stay native datetime-local: fully typeable, with the
   * same value format and change semantics. The picker only composes
   * a value, writes it into the input, and dispatches the input's
   * change event, so applying from the picker is indistinguishable
   * from a manual edit (including the page-1 reset). The popup is
   * fixed-positioned from the anchor's rect so the Logs card cannot
   * clip it; it closes on Apply/Clear, Escape, and outside click. */
  const fromCalBtn = root.querySelector("#logFromCal") as HTMLButtonElement | null;
  const toCalBtn = root.querySelector("#logToCal") as HTMLButtonElement | null;
  const pad2 = (n: number): string => String(n).padStart(2, "0");
  let pickerEl: HTMLDivElement | null = null;
  let pickerInput: HTMLInputElement | null = null;
  let pickerAnchor: HTMLElement | null = null;
  let pickerView = { y: 0, m: 0 };
  let pickerSel = { y: 0, m: 0, d: 0 };
  let pickerTimeState = { hh: 0, mm: 0 };

  const closePicker = (): void => {
    pickerEl?.remove();
    pickerEl = null;
    pickerInput = null;
    pickerAnchor = null;
    document.removeEventListener("mousedown", onPickerOutside, true);
    document.removeEventListener("keydown", onPickerKey, true);
  };
  const onPickerOutside = (ev: MouseEvent): void => {
    const t = ev.target as Node;
    if (pickerEl?.contains(t) || pickerAnchor?.contains(t)) return;
    closePicker();
  };
  const onPickerKey = (ev: KeyboardEvent): void => {
    if (ev.key === "Escape") closePicker();
  };

  /** Read the popup's time fields into pickerTimeState (clamped);
   *  called before any re-render so typed time is never lost. */
  const syncPickerTime = (): void => {
    const h = pickerEl?.querySelector("#dtHour") as HTMLInputElement | null;
    const m = pickerEl?.querySelector("#dtMinute") as HTMLInputElement | null;
    if (!h || !m) return;
    const clamp = (v: string, max: number): number => {
      const n = Number.parseInt(v, 10);
      return Number.isNaN(n) ? 0 : Math.min(max, Math.max(0, n));
    };
    pickerTimeState = { hh: clamp(h.value, 23), mm: clamp(m.value, 59) };
  };

  const renderPicker = (): void => {
    if (!pickerEl) return;
    const { y, m } = pickerView;
    const monthName = new Date(y, m, 1).toLocaleDateString("en-GB", { month: "long", year: "numeric" });
    const startDay = new Date(y, m, 1).getDay();
    const daysInMonth = new Date(y, m + 1, 0).getDate();
    const daysInPrev = new Date(y, m, 0).getDate();
    const now = new Date();
    let cells = "";
    for (let i = 0; i < 42; i++) {
      const dn = i - startDay + 1;
      let cy = y; let cm = m; let cd = dn; let other = false;
      if (dn < 1) {
        const pm = new Date(y, m - 1, 1);
        cy = pm.getFullYear(); cm = pm.getMonth(); cd = dn + daysInPrev; other = true;
      } else if (dn > daysInMonth) {
        const nm = new Date(y, m + 1, 1);
        cy = nm.getFullYear(); cm = nm.getMonth(); cd = dn - daysInMonth; other = true;
      }
      const cls = `dt-day${other ? " other" : ""}${cy === pickerSel.y && cm === pickerSel.m && cd === pickerSel.d ? " sel" : ""}${cy === now.getFullYear() && cm === now.getMonth() && cd === now.getDate() ? " today" : ""}`;
      cells += `<button type="button" class="${cls}" data-y="${cy}" data-m="${cm}" data-d="${cd}">${cd}</button>`;
    }
    pickerEl.innerHTML = `
      <div class="dt-head">
        <button type="button" class="btn dt-nav" data-nav="-1" title="Previous month" aria-label="Previous month">${icon("chevL")}</button>
        <span>${esc(monthName)}</span>
        <button type="button" class="btn dt-nav" data-nav="1" title="Next month" aria-label="Next month">${icon("chevR")}</button>
      </div>
      <div class="dt-grid">${["Su", "Mo", "Tu", "We", "Th", "Fr", "Sa"].map((w) => `<span class="dt-wd">${w}</span>`).join("")}${cells}</div>
      <div class="dt-time">Time
        <input id="dtHour" type="number" min="0" max="23" value="${pad2(pickerTimeState.hh)}" style="${SELECT_STYLE};width:58px"/> :
        <input id="dtMinute" type="number" min="0" max="59" value="${pad2(pickerTimeState.mm)}" style="${SELECT_STYLE};width:58px"/>
      </div>
      <div class="dt-actions">
        <button type="button" class="btn icon-only" data-act="clear" title="Clear" aria-label="Clear">${icon("x")}</button>
        <button type="button" class="btn" data-act="apply">${icon("check")} Apply</button>
      </div>`;
  };

  const onPickerClick = (ev: Event): void => {
    const t = ev.target as HTMLElement;
    const nav = t.closest("[data-nav]") as HTMLElement | null;
    if (nav) {
      syncPickerTime();
      const nm = new Date(pickerView.y, pickerView.m + Number(nav.getAttribute("data-nav")), 1);
      pickerView = { y: nm.getFullYear(), m: nm.getMonth() };
      renderPicker();
      return;
    }
    const day = t.closest(".dt-day") as HTMLElement | null;
    if (day) {
      syncPickerTime();
      pickerSel = { y: Number(day.dataset.y), m: Number(day.dataset.m), d: Number(day.dataset.d) };
      pickerView = { y: pickerSel.y, m: pickerSel.m };
      renderPicker();
      return;
    }
    const act = t.closest("[data-act]") as HTMLElement | null;
    if (!act || !pickerInput) return;
    if (act.getAttribute("data-act") === "clear") {
      pickerInput.value = "";
      pickerInput.dispatchEvent(new Event("change", { bubbles: true }));
      closePicker();
      return;
    }
    syncPickerTime();
    pickerInput.value = `${pickerSel.y}-${pad2(pickerSel.m + 1)}-${pad2(pickerSel.d)}T${pad2(pickerTimeState.hh)}:${pad2(pickerTimeState.mm)}`;
    pickerInput.dispatchEvent(new Event("change", { bubbles: true }));
    closePicker();
  };

  const openPicker = (input: HTMLInputElement, anchor: HTMLElement): void => {
    if (pickerEl && pickerInput === input) { closePicker(); return; }
    closePicker();
    const parsed = input.value ? new Date(input.value) : null;
    const base = parsed && !Number.isNaN(parsed.getTime()) ? parsed : new Date();
    pickerSel = { y: base.getFullYear(), m: base.getMonth(), d: base.getDate() };
    pickerView = { y: pickerSel.y, m: pickerSel.m };
    pickerTimeState = { hh: base.getHours(), mm: base.getMinutes() };
    pickerInput = input;
    pickerAnchor = anchor;
    pickerEl = document.createElement("div");
    pickerEl.className = "dt-pop";
    pickerEl.style.visibility = "hidden";
    pickerEl.addEventListener("click", onPickerClick);
    document.body.appendChild(pickerEl);
    renderPicker();
    const r = anchor.getBoundingClientRect();
    const w = pickerEl.offsetWidth;
    const h = pickerEl.offsetHeight;
    let top = r.bottom + 4;
    if (top + h > window.innerHeight - 8) top = Math.max(8, r.top - h - 4);
    pickerEl.style.top = `${top}px`;
    pickerEl.style.left = `${Math.min(Math.max(8, r.left), Math.max(8, window.innerWidth - w - 8))}px`;
    pickerEl.style.visibility = "";
    document.addEventListener("mousedown", onPickerOutside, true);
    document.addEventListener("keydown", onPickerKey, true);
  };

  const onFromCal = (): void => { if (fromInput && fromCalBtn) openPicker(fromInput, fromCalBtn); };
  const onToCal = (): void => { if (toInput && toCalBtn) openPicker(toInput, toCalBtn); };
  fromCalBtn?.addEventListener("click", onFromCal);
  toCalBtn?.addEventListener("click", onToCal);

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

  /* ---- Session state + recent searches (tracker #59) ---- */
  const recentsEl = root.querySelector("#logRecents") as HTMLElement | null;
  /** Set once this wire has applied its entry state. The router can
   *  call a superseded wire's cleanup after its DOM was replaced;
   *  such a wire must not snapshot its never-applied defaults over
   *  the session state. */
  let stateApplied = false;

  /** Mirror the controls (and the closure page) into the
   *  module-level session state. */
  const syncLogsState = (): void => {
    logsViewState = {
      scope: scopeSel?.value ?? "all",
      level: levelSel?.value ?? "",
      query: searchInput?.value ?? "",
      from: fromInput?.value ?? "",
      to: toInput?.value ?? "",
      sort: sortSel?.value === "asc" ? "asc" : "desc",
      limit: Number(limitSel?.value ?? 200),
      follow: followChk?.checked ?? false,
      page,
    };
  };

  /** The currently applied query as a search set, stamped now. The
   *  query is the trimmed value — the one load() actually sends —
   *  so sets differing only in stray spaces dedupe together. */
  const currentSearchSet = (): LogSearchSet => ({
    scope: logsViewState.scope,
    level: logsViewState.level,
    query: logsViewState.query.trim(),
    from: logsViewState.from,
    to: logsViewState.to,
    sort: logsViewState.sort,
    limit: logsViewState.limit,
    ranAt: new Date().toISOString(),
  });

  /** Recent searches: an expandable section under the filter card,
   *  collapsed by default (principle 1), omitted entirely while
   *  the history is empty (principle 13). An entry click applies
   *  the whole set; Clear empties the history (words + icon —
   *  clearing is destructive, principle 15). */
  const paintRecents = (): void => {
    if (!recentsEl) return;
    if (!logsRecents.length) { recentsEl.innerHTML = ""; return; }
    const open = logsRecentsOpen;
    recentsEl.innerHTML = `<div class="card" style="margin-bottom:12px">
      <div style="display:flex;align-items:center;gap:8px">
        <button class="btn" id="logRecentsToggle" aria-expanded="${open}">${icon(open ? "chevD" : "chevR")} Recent searches (${logsRecents.length})</button>
        <span class="spacer" style="flex:1"></span>
        ${open ? `<button class="btn" id="logRecentsClear">${icon("x")} Clear</button>` : ""}
      </div>
      ${open ? `<div style="display:grid;gap:2px;margin-top:8px">${logsRecents.map((e, i) => `<button class="btn" data-recent="${i}" title="${esc(fmtDateTime(e.ranAt))}" style="justify-content:flex-start;text-align:left"><span style="flex:1">${esc(logSearchSummary(e))}</span><span style="color:var(--text3);font-size:11px">${esc(fmtRanAt(e.ranAt))}</span></button>`).join("")}</div>` : ""}
    </div>`;
    recentsEl.querySelector("#logRecentsToggle")?.addEventListener("click", () => {
      logsRecentsOpen = !logsRecentsOpen;
      paintRecents();
    });
    recentsEl.querySelector("#logRecentsClear")?.addEventListener("click", () => {
      logsRecents = [];
      saveLogsRecents();
      paintRecents();
    });
    recentsEl.querySelectorAll("[data-recent]").forEach((b) => {
      b.addEventListener("click", () => {
        const e = logsRecents[Number((b as HTMLElement).dataset.recent)];
        if (e) applySearchSet(e);
      });
    });
  };

  /** Capture the applied set into the recents and repaint them. */
  const captureSearch = (): void => {
    recordLogSearch(currentSearchSet());
    paintRecents();
  };

  /** Write a search set into the session state and the controls,
   *  restart paging at 1, and run it. Follow is not part of a set
   *  and is left as it stands. */
  const applySearchSet = (s: LogSearchSet): void => {
    mergeScopeOptions([{ value: s.scope, label: s.scope }]);
    if (scopeSel) scopeSel.value = s.scope;
    if (levelSel) levelSel.value = s.level;
    if (searchInput) searchInput.value = s.query;
    if (fromInput) fromInput.value = s.from;
    if (toInput) toInput.value = s.to;
    if (sortSel) sortSel.value = s.sort;
    if (limitSel) limitSel.value = String(s.limit);
    page = 1;
    syncLogsState();
    captureSearch();
    void load();
  };

  const filtersActive = (): boolean => Boolean(
    levelSel?.value || searchInput?.value.trim() || fromInput?.value || toInput?.value,
  );

  const load = async (): Promise<void> => {
    const seq = ++loadSeq;
    const stale = (): boolean => disposed || seq !== loadSeq;
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
        untilMs: untilMsOf(toInput?.value),
        order: sortSel?.value === "asc" ? "asc" : "desc",
        offset: (page - 1) * limit,
      });
      let r = await fetchPage();
      if (stale()) return;
      // The filtered set may have shrunk below the current page
      // (e.g. a Follow refresh after lines aged out): clamp to the
      // last page and refetch, so the page shown is real. The
      // clamp LOOPS (bounded): the set can shrink again between
      // the two fetches, and a single clamp-and-refetch could
      // still land past the end (code-review 5.6). Page targets
      // the CURRENT page value: a page turn during a fetch
      // supersedes this load anyway (the seq check).
      for (let attempt = 0; attempt < 3; attempt += 1) {
        const lastPage = Math.max(1, Math.ceil(r.data.total / limit));
        if (page <= lastPage) break;
        page = lastPage;
        r = await fetchPage();
        if (stale()) return;
      }
      // The clamp may have moved the page; the session state keeps
      // the page that is actually shown (tracker #59).
      logsViewState.page = page;
      setBanner(root, "logBanner", bannerHtml(r));
      mergeScopeOptions(r.data.scopes.map((s) => ({
        value: s.id,
        label: s.previous ? `${s.id} (incl. previous run)` : s.id,
      })));
      const pages = Math.max(1, Math.ceil(r.data.total / limit));
      // Pagination values as Label: Value, the total with thousands
      // separators, and position as a micro-bar (fill = page/pages).
      if (pageInfo) pageInfo.textContent = `Page: ${page} of ${pages}`;
      if (matchInfo) matchInfo.textContent = `Matching: ${fmtCount(r.data.total)}`;
      if (posBar) posBar.innerHTML = meter((page / pages) * 100, 64, 6, C.blue);
      if (prevBtn) prevBtn.disabled = page <= 1;
      if (nextBtn) nextBtn.disabled = page >= pages;
      const b = root.querySelector("#logBody");
      if (!b) return;
      if (r.error) { b.innerHTML = emptyHtml(`Logs unavailable: ${r.error}`); return; }
      if (!r.data.lines.length) {
        b.innerHTML = emptyHtml(filtersActive() || r.data.total_matching > 0
          ? "No matching lines."
          : "No lines yet.");
        return;
      }
      b.innerHTML = `<div class="logstream">${r.data.lines.map((row) => logRow(row, scope === "all")).join("")}</div>`;
    } catch {
      // getLogs wraps invoke errors into the result (r.error is
      // rendered above); a throw here is a programming error — the
      // next load (filter change, Follow tick) repaints.
    }
  };

  // Scope options: "all" plus every backend id (configured or not — a
  // retained ring can outlive its config entry).
  const backs = await getBackends();
  if (!disposed && backs.live && !backs.error) {
    mergeScopeOptions(backs.data.map((bk) => ({ value: bk.id, label: bk.id })));
  }
  // Entry state (tracker #59). A parametrized entry — a deep link
  // (#/logs?scope=<id>&follow=1, the entity cards' "go to logs"
  // buttons; a scope alone from the wizard's failure link) — applies
  // its params over the fresh defaults exactly as it did before #59,
  // and the applied result becomes the session state and is
  // captured as a recent search. A plain entry instead restores
  // the session state verbatim — scope, level, search text, time
  // bounds, sort, limit, Follow, and page.
  const paramEntry = initialScope !== undefined || initialFollow;
  if (paramEntry) {
    // Deep link (#/logs?scope=<id>): select it, adding the option
    // when it is not a known backend id (e.g. a previous-run scope).
    if (scopeSel && initialScope) {
      mergeScopeOptions([{ value: initialScope, label: initialScope }]);
      scopeSel.value = initialScope;
    }
    if (followChk) followChk.checked = initialFollow;
    stateApplied = true;
    syncLogsState();
    captureSearch();
  } else {
    // The retained scope may need its option added first (a scope
    // whose backend has left the config, or a previous-run scope
    // the first payload has not listed yet).
    mergeScopeOptions([{ value: logsViewState.scope, label: logsViewState.scope }]);
    if (scopeSel) scopeSel.value = logsViewState.scope;
    if (levelSel) levelSel.value = logsViewState.level;
    if (searchInput) searchInput.value = logsViewState.query;
    if (fromInput) fromInput.value = logsViewState.from;
    if (toInput) toInput.value = logsViewState.to;
    if (sortSel) sortSel.value = logsViewState.sort;
    if (limitSel) limitSel.value = String(logsViewState.limit);
    if (followChk) followChk.checked = logsViewState.follow;
    page = logsViewState.page;
    stateApplied = true;
  }
  paintRecents();

  /** Any filter change (scope, level, text, time bounds, sort, page
   *  size) restarts paging at page 1 — and is a search being run:
   *  it updates the session state and is captured into the recents
   *  (tracker #59). Refresh, pager turns, and Follow toggles only
   *  re-run or navigate the current search, so they sync the
   *  session state without capturing. */
  const onFilterChange = (): void => { page = 1; syncLogsState(); captureSearch(); void load(); };
  /** Manual Refresh reloads the current page as-is. */
  const onRefresh = (): void => { void load(); };
  const onSearch = (): void => {
    if (searchTimer !== null) window.clearTimeout(searchTimer);
    searchTimer = window.setTimeout(() => {
      searchTimer = null;
      page = 1;
      syncLogsState();
      captureSearch();
      void load();
    }, 300);
  };
  const onPrev = (): void => { if (page > 1) { page -= 1; syncLogsState(); void load(); } };
  const onNext = (): void => { page += 1; syncLogsState(); void load(); };
  const onFollow = (): void => {
    syncLogsState();
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

  // Arrival with Follow on — a deep link's follow=1, or a session
  // state that was left following (tracker #59): arrive in exactly
  // the state the Follow toggle produces — checkbox on, 2 s
  // re-poll running, first load fired by onFollow itself, so the
  // restored filters' results load before the tail resumes.
  if (followChk?.checked) {
    onFollow();
  } else {
    await load();
  }

  return () => {
    disposed = true;
    // Final session-state snapshot: the controls still hold the
    // operator's last values (the router runs cleanup before the
    // next view's markup replaces this DOM) — including search
    // text whose debounce had not fired yet. Guarded by
    // stateApplied so a superseded wire that never applied its
    // entry state cannot overwrite the session with defaults.
    if (stateApplied) syncLogsState();
    closePicker();
    if (followTimer !== null) window.clearInterval(followTimer);
    if (searchTimer !== null) window.clearTimeout(searchTimer);
    scopeSel?.removeEventListener("change", onFilterChange);
    levelSel?.removeEventListener("change", onFilterChange);
    fromInput?.removeEventListener("change", onFilterChange);
    toInput?.removeEventListener("change", onFilterChange);
    fromCalBtn?.removeEventListener("click", onFromCal);
    toCalBtn?.removeEventListener("click", onToCal);
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

/** Routing-rules card form state (routing-rules design §7), held
 *  in the wireGateway closure so the 5-second repaint preserves an
 *  open form. `editingId` null = adding a new rule; `optimalTokens`
 *  is the raw input text — empty means the fraction default. */
interface RuleFormState {
  open: boolean;
  editingId: string | null;
  target: string;
  optimalTokens: string;
  advisory: boolean;
}

function gatewayPaint(
  status: GatewayStatusData,
  cfg: GatewayConfigData,
  backends: BackendRow[],
  models: ModelRow[],
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
  routingTab: "flow" | "groups",
  orModelsOpen: boolean,
  openFragments: Set<string>,
  ruleForm: RuleFormState,
  confirmRuleDelete: Set<string>,
  ruleRawFlags: Map<string, string | null>,
  genTab: "yaml" | "routing",
  routingPyText: string | null,
): string {
  const port = status.port || cfg.port;
  const backendById = new Map(backends.map((b) => [b.id, b]));
  /** Is this member dropped at generation time, and why? A backend
   *  member is dropped when disabled; the openrouter member when its
   *  tier is disabled. A member naming neither (an unknown name)
   *  cannot be verified as excluded and is treated as active.
   *  Exclusion is a settled state, so it renders gray (principle
   *  11) with the reason in a tooltip — never amber. */
  const memberState = (name: string): { excluded: boolean; reason: string } => {
    if (name === "openrouter") {
      return cfg.openrouter && !cfg.openrouter.enabled
        ? { excluded: true, reason: "tier disabled" }
        : { excluded: false, reason: "" };
    }
    const b = backendById.get(name);
    return b && !b.enabled
      ? { excluded: true, reason: "backend disabled" }
      : { excluded: false, reason: "" };
  };
  const memberExcluded = (name: string): boolean => memberState(name).excluded;
  const groupByName = new Map(cfg.groups.map((g) => [g.name, g]));
  /** A fallback naming a group whose members are ALL excluded (or
   *  which has no members) produces no deployments — the generated
   *  config drops the fallback reference itself. */
  const fallbackState = (name: string): { excluded: boolean; reason: string } => {
    const g = groupByName.get(name);
    if (g && name !== "openrouter") {
      return g.members.length === 0 || g.members.every(memberExcluded)
        ? { excluded: true, reason: "no active members" }
        : { excluded: false, reason: "" };
    }
    return memberState(name);
  };

  // The exclusion chips above are computed from the backend list; when
  // that list failed to load, say so instead of silently rendering
  // members as included.
  const backendsNote = backendsError
    ? `<div class="banner" style="margin-bottom:12px">${icon("warn")} Backend list unavailable (${esc(backendsError)}) \u2014 exclusion states in this view may be incomplete.</div>`
    : "";

  const withWin = backends.filter((b) => b.requests_1h !== undefined);
  // The traffic block renders only when at least one backend has a
  // metrics window (no placeholder line otherwise). Requests are a
  // per-backend distribution, so they render as a stacked bar whose
  // segments are the reporting backends (the largest segment IS the
  // hottest backend — no separate tile); the totals live once, as
  // the rows' Label: Value. Totals sum reporting backends only, and
  // partial coverage is stated in the Requests row's info-tip —
  // never claimed as "all backends" (principle 12). Errors render
  // as their share of requests in a thin proportion bar.
  const gwStats = withWin.length
    ? (() => {
        const totalReq = withWin.reduce((a, b) => a + (b.requests_1h ?? 0), 0);
        const totalErr = withWin.reduce((a, b) => a + (b.errors_1h ?? 0), 0);
        const totalToks = withWin.reduce((a, b) => a + (b.throughput_toks ?? 0), 0);
        const palette = [C.blue, C.purple, C.teal, C.amber, C.green];
        const segs = withWin.map((b, i) => ({
          pct: totalReq > 0 ? ((b.requests_1h ?? 0) / totalReq) * 100 : 0,
          color: palette[i % palette.length],
          label: `${b.id} — ${fmtCount(b.requests_1h ?? 0)} req (1h)`,
        }));
        const keys = withWin.map((b, i) => `<span><span class="sw" style="background:${palette[i % palette.length]}"></span>${esc(b.id)} ${fmtCount(b.requests_1h ?? 0)}</span>`).join("");
        const coverageTip = withWin.length < backends.length
          ? `<span class="info-tip" title="${withWin.length} of ${backends.length} backends reporting — totals cover reporting backends only">${icon("info")}</span>`
          : "";
        const errSegs = totalReq > 0 && totalErr > 0
          ? [{ pct: (totalErr / totalReq) * 100, color: C.red, label: `Errors: ${fmtCount(totalErr)} of ${fmtCount(totalReq)} requests (1h)` }]
          : [];
        return `<div style="margin-top:12px;border-top:1px solid var(--border);padding-top:10px">
        <div style="font-size:12px;color:var(--text2)">Requests (1h): <b style="color:var(--text)">${fmtCount(totalReq)}</b>${coverageTip}</div>
        <div style="margin-top:4px">${stackedBar(segs, 600, 10)}</div>
        <div class="legend" style="margin-top:4px">${keys}</div>
        <div style="font-size:12px;color:var(--text2);margin-top:8px">Errors (1h): <b style="color:var(--text)">${fmtCount(totalErr)}</b></div>
        <div style="margin-top:4px">${stackedBar(errSegs, 600, 4)}</div>
        <div class="kv" style="margin-top:8px"><span class="k">Throughput</span><span class="v" style="font-size:13px">${trim1(totalToks)} tok/s</span></div>
      </div>`;
      })()
    : "";
  const summary = `<div class="card" style="margin-bottom:12px"><div class="sec-title">${dot(status.up ? "ok" : "err")} Gateway \u2014 LiteLLM \u00b7 ${status.up ? "up" : "down"}</div>
    <div style="display:grid;grid-template-columns:repeat(5,1fr);gap:10px">
      ${([["Port", String(port)], ["Groups", fmtCount(status.groups), "Enabled groups — a group counts when at least one of its member backends is enabled; the gateway serves enabled groups only."], ["Routing rules", fmtCount(status.routing_rules)], ["Routing test traces", fmtCount(status.routing_log_len), "Routing test traces are dry-run decisions recorded from routing tests this session — live requests are not logged."], ["LiteLLM", status.litellm_version ?? "not measured", "The LiteLLM version the start-gateway step probed in the gateway venv at the last spawn (MCP design §4.3). Not measured until a spawn records one."]] as [string, string, string?][])

        .map(([k, v, tip]) => `<div class="kv"><span class="k">${k}${tip ? `<span class="info-tip" title="${esc(tip)}">${icon("info")}</span>` : ""}</span><span class="v" style="font-size:13px">${esc(v)}</span></div>`).join("")}
    </div>
    ${gwStats}</div>`;

  // Bind explanations ride an info-tip on the Network access row —
  // the LAN row itself renders only when a LAN URL exists.
  const bindTip = status.bind_host === "127.0.0.1"
    ? "Reachable from this machine only."
    : status.lan_ip
      ? null
      : "Bound to all interfaces; this machine's LAN address could not be determined.";
  const masterKeyTip = `Clients authenticate with the master key — LiteLLM requires one. The VS Code LiteLLM extension takes the base URL without /v1 (http://127.0.0.1:${port}); it appends /v1 itself.${masterKey === null ? " Reveal loads it." : ""}`;
  // The gateway lifecycle controls answer "how do I control the
  // gateway", not the endpoint card's question ("what do clients
  // connect to") — the row renders directly under the summary card
  // instead (client-endpoint audit Q7). Ids and handlers unchanged.
  const gwActions = `<div class="card-actions" style="margin:0 0 12px">
      <button class="btn" id="gwStartBtn"${starting || stopping ? " disabled" : ""}>${icon("restart")} ${status.up ? "Restart gateway" : "Start gateway"}</button><span class="info-tip" title="Runs the Setup Wizard's start-gateway step: LiteLLM binds the configured address, and the step reconciles the Windows firewall rule with it (one elevation prompt when the rule state changes). When the OpenRouter tier is enabled, the fallback is armed from Windows Credential Manager.">${icon("info")}</span>
      <button class="btn" id="gwStopBtn"${status.up && !starting && !stopping ? "" : " disabled"}>${icon("pause")} Stop gateway</button>
      <a class="btn" href="#/wizard">${icon("wrench")} Setup Wizard</a>
      <button class="btn icon-only" id="gwAdminUiBtn" title="LiteLLM admin UI" aria-label="LiteLLM admin UI">${icon("gateway")}</button><span class="info-tip" title="Sign in as admin with the master key as the password. The admin UI requires a database — run the setup wizard's Install PostgreSQL step if sign-in reports &quot;Not connected to DB&quot;.">${icon("info")}</span>
      <a class="btn icon-only" href="#/logs?scope=gateway&follow=1" title="Logs" aria-label="Gateway logs">${icon("logs")}</a>
    </div>`;
  const endpoint = `<div class="card" style="margin-bottom:12px"><div class="sec-title">${icon("gateway")} Client endpoint</div>
    <div style="font-size:12px;color:var(--text2);display:grid;gap:4px">
      <div>Base URL: <span style="font-family:var(--mono);color:var(--text)">http://127.0.0.1:${port}/v1</span>
        <button class="btn icon-only" id="gwBaseUrlCopyBtn" title="Copy base URL" aria-label="Copy base URL">${icon("copy")}</button></div>
      <div>Network access:${bindTip ? `<span class="info-tip" title="${esc(bindTip)}">${icon("info")}</span>` : ""} <select id="gwBindSelect" class="input" style="width:auto;display:inline-block;padding:2px 6px">
          <option value="0.0.0.0"${status.bind_host !== "127.0.0.1" ? " selected" : ""}>Local network (all interfaces)</option>
          <option value="127.0.0.1"${status.bind_host === "127.0.0.1" ? " selected" : ""}>This computer only (127.0.0.1)</option>
        </select></div>
      ${status.bind_host !== "127.0.0.1" && status.lan_ip
        ? `<div>LAN base URL: <span style="font-family:var(--mono);color:var(--text)">http://${esc(status.lan_ip)}:${port}/v1</span><span class="info-tip" title="Other machines use this address with the master key. The VS Code LiteLLM extension takes it without /v1.">${icon("info")}</span>
        <button class="btn icon-only" id="gwLanUrlCopyBtn" title="Copy LAN base URL" aria-label="Copy LAN base URL">${icon("copy")}</button></div>`
        : ""}
      <div>Master key:<span class="info-tip" title="${esc(masterKeyTip)}">${icon("info")}</span> <span style="font-family:var(--mono);color:var(--text)">${masterKey === null ? "not loaded" : keyRevealed ? esc(masterKey) : "\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022"}</span>
        <button class="btn icon-only" id="gwKeyRevealBtn" title="${keyRevealed ? "Hide master key" : "Reveal master key"}" aria-label="${keyRevealed ? "Hide master key" : "Reveal master key"}">${icon(keyRevealed ? "eyeOff" : "eye")}</button>
        <button class="btn icon-only" id="gwKeyCopyBtn" title="Copy master key" aria-label="Copy master key">${icon("copy")}</button></div>
    </div></div>`;

  const or = cfg.openrouter;
  // The tier card is omitted entirely when the tier is not
  // configured (principle 13). State renders as header chips, the
  // credential's probed presence as a chip beside its label
  // (present / not found / not checked — the probe's null is an
  // explicit unknown, never prose), and every explanation rides a
  // tooltip verbatim. The reference is an address (of the secret in
  // Windows Credential Manager), so it carries a Copy button.
  const openrouter = or === null ? "" : (() => {
    const present = status.openrouter_cred_present;
    const credChip = or.cred_ref
      ? (present === true
        ? `<span class="chip status-tag status-on">present</span>`
        : present === false
          ? `<span class="chip status-tag status-bad" title="The fallback cannot authenticate.">not found</span>`
          : `<span class="chip">not checked</span>`)
      : or.enabled
        ? `<span class="chip status-tag status-bad" title="The fallback cannot authenticate until one is configured.">no credential reference</span>`
        : "";
    const credStatusRow = or.cred_ref || or.enabled
      ? `<div style="display:flex;justify-content:space-between;gap:16px"><span class="k">Credential status</span><span>${credChip}</span></div>`
      : "";
    const allowRow = `<div style="display:flex;justify-content:space-between;gap:16px"><span class="k">Model allowlist</span><span class="v" style="font-size:12px;text-align:right">${or.model_allowlist.length ? `${or.model_allowlist.length} model${or.model_allowlist.length === 1 ? "" : "s"}` : "none"}${or.model_allowlist.length ? ` <button class="btn" data-or-models aria-expanded="${orModelsOpen}" title="Model allowlist" aria-label="Model allowlist" style="padding:1px 7px;font-size:11px">${icon(orModelsOpen ? "chevD" : "chevR")}</button>` : ""}</span></div>
      ${orModelsOpen && or.model_allowlist.length ? `<div style="display:flex;gap:6px;flex-wrap:wrap;margin-top:4px">${or.model_allowlist.map((m) => `<span class="chip">${esc(m)}</span>`).join("")}</div>` : ""}`;
    return `<div class="card" style="margin-bottom:12px${or.enabled ? "" : ";opacity:.65"}"><div class="sec-title">${icon("gateway")} OpenRouter fallback tier
      <span class="spacer" style="flex:1"></span>
      ${or.enabled
        ? `<span class="chip status-tag status-on">enabled</span>`
        : `<span class="chip status-tag status-off" title="The tier is disabled — fallback entries naming openrouter are dropped when the gateway config is generated, and no traffic is sent to OpenRouter.">disabled</span>`}
    </div>
    <div class="kv" style="gap:10px">
      ${or.cred_ref ? `<div style="display:flex;justify-content:space-between;gap:16px"><span class="k">Credential<span class="info-tip" title="When the gateway starts, the key is read from Windows Credential Manager (reference above) and passed to LiteLLM as OPENROUTER_API_KEY. The key itself is never stored in the configuration.">${icon("info")}</span></span><span class="v" style="font-size:12px;text-align:right">${esc(or.cred_ref)} <button class="btn" id="gwOrCredCopyBtn" title="Copy credential reference" aria-label="Copy credential reference" style="padding:1px 7px;font-size:11px">${icon("copy")}</button></span></div>` : ""}
      ${credStatusRow}
      <div style="display:flex;justify-content:space-between;gap:16px"><span class="k">Daily cap<span class="info-tip" title="Spending cap per day.">${icon("info")}</span></span><span class="v" style="font-size:12px;text-align:right">$${trim1(or.daily_cap_usd)}</span></div>
      ${allowRow}
    </div>
    <div class="card-actions">${enableSwitch({ on: or.enabled, name: "OpenRouter fallback tier", attrs: `id="gwOrToggle"` })}</div>
  </div>`;
  })();

  const effStrategy = effectiveGatewayStrategy(cfg);

  /* ---- Routing section: one section, two views (Flow | Groups).
   * The section top carries the one answer-level fact — the
   * effective global strategy as Label: Value, its methodology in
   * the label's info-tip. ---- */
  const flowGroupHtml = (g: GatewayGroup): string => {
    // Member traffic shares: real only when every member is a
    // backend with a metrics window and the group total is
    // positive. Otherwise the bar encodes configured membership at
    // equal widths, and its info-tip states that basis — equal
    // width never invents traffic.
    const reqs = g.members.map((name) => backendById.get(name)?.requests_1h);
    const totalReq = reqs.reduce<number>((a, r) => a + (r ?? 0), 0);
    const traffic = g.members.length > 0 && reqs.every((r) => r !== undefined) && totalReq > 0;
    const palette = [C.blue, C.purple, C.teal, C.amber, C.green];
    const segs = g.members.map((name, i) => ({
      pct: traffic ? ((reqs[i] ?? 0) / totalReq) * 100 : 100 / g.members.length,
      color: traffic ? palette[i % palette.length] : "#3a4350",
      label: traffic ? `${name} — ${fmtCount(reqs[i] ?? 0)} req (1h)` : name,
    }));
    const keys = g.members.map((name) => {
      const st = memberState(name);
      return st.excluded
        ? `<span style="color:var(--text3)">${esc(name)} <span class="chip status-tag status-off" title="${esc(st.reason)}">excluded</span></span>`
        : `<span>${esc(name)}</span>`;
    }).join(" · ");
    const fallbackLines = g.fallbacks.map((name) => {
      const st = fallbackState(name);
      // An excluded fallback target gets NO connector — a connector
      // must never assert a path the generated config drops.
      return st.excluded
        ? `<div style="font-size:12px;margin-top:2px"><span class="chip status-tag status-off" title="${esc(st.reason)}">${esc(name)}</span></div>`
        : `<div style="font-size:12px;margin-top:2px"><span style="color:var(--text3)">fallback - - \u2192</span> ${esc(name)}</div>`;
    }).join("");
    const sub = g.members.length
      ? `Members enabled: ${g.enabled_members} of ${g.members.length}`
      : `<span title="Omitted from the generated gateway config.">Members: 0</span>`;
    return `<div style="display:flex;gap:10px;align-items:flex-start;padding:8px 0;border-bottom:1px solid var(--border)${g.enabled ? "" : ";opacity:.65"}">
      <div style="flex:0 0 170px;border:1px solid var(--border);border-radius:8px;padding:8px 10px;background:var(--panel2)">
        <div style="font-size:12px;font-weight:600">${esc(g.name)}${g.enabled ? "" : ` <span class="chip status-tag status-off">disabled</span>`}</div>
        <div style="font-size:11px;color:var(--text2);margin-top:2px">${sub}</div>
      </div>
      <span style="color:var(--text3);padding-top:10px">\u2192</span>
      <div style="flex:1;min-width:0">
        ${g.members.length ? `${stackedBar(segs, 600, 10)}${traffic ? "" : `<span class="info-tip" title="Configured membership — no metrics window yet.">${icon("info")}</span>`}
        <div style="font-size:12px;margin-top:4px">${keys}</div>` : ""}
        ${fallbackLines}
      </div>
    </div>`;
  };

  const groupCardHtml = (g: GatewayGroup): string => {
    const m = g.members.length;
    const enabledCount = g.members.filter((name) => !memberExcluded(name)).length;
    // One segment per member, equal width — segments encode
    // membership and state, not a share. Green reaches the
    // generated config; gray is excluded. The fraction lives only
    // in the bar's tooltip; member names are the bar's keys, each
    // in its segment's color.
    const bar = m
      ? `<div style="margin-top:10px"><span title="Enabled: ${enabledCount} of ${m}" style="display:block">${stackedBar(g.members.map((name) => ({
          pct: 100 / m,
          color: memberExcluded(name) ? "#6d7686" : C.green,
          label: name,
        })), 300, 10)}</span>
        <div style="font-size:12px;margin-top:4px;display:flex;gap:10px;flex-wrap:wrap">${g.members.map((name) => {
          const ex = memberExcluded(name);
          return `<span style="color:${ex ? "var(--text3)" : C.green}"${ex ? ` title="Disabled — dropped from the generated gateway config"` : ""}>${esc(name)}</span>`;
        }).join("")}</div></div>`
      : "";
    const fallbacks = g.fallbacks.length
      ? `<div style="font-size:12px;margin-top:8px"><span style="color:var(--text2)">Fallbacks:</span> ${g.fallbacks.map((name) => {
          const st = fallbackState(name);
          const tip = st.reason === "no active members"
            ? "No active members — dropped from the generated gateway config"
            : "Disabled — dropped from the generated gateway config";
          return `<span style="color:${st.excluded ? "var(--text3)" : C.green}"${st.excluded ? ` title="${tip}"` : ""}>${esc(name)}</span>`;
        }).join(" · ")}</div>`
      : "";
    return `<div class="card"${g.enabled ? "" : ` style="opacity:.65"`}>
      <h3>${esc(g.name)}${g.enabled ? "" : ` <span class="chip status-tag status-off" title="No enabled backends — omitted from the generated gateway config">disabled</span>`}
        <span class="spacer" style="flex:1"></span><span class="chip">${esc(g.strategy)}</span></h3>
      ${bar}
      ${fallbacks}
    </div>`;
  };

  const routing = cfg.groups.length
    ? `<div class="card" style="margin-bottom:12px"><div class="sec-title">${icon("gateway")} Routing</div>
      ${effStrategy ? `<div style="font-size:12px;color:var(--text2)">Effective strategy: <b style="color:var(--text)">${esc(effStrategy)}</b><span class="info-tip" title="LiteLLM applies ONE global routing strategy — the strategy chips on the groups are labels, not per-group settings.">${icon("info")}</span></div>` : ""}
      <div class="viewsel" role="tablist" style="margin-top:10px">
        <button data-routing-tab="flow"${routingTab === "flow" ? ` class="active"` : ""}>Flow</button>
        <button data-routing-tab="groups"${routingTab === "groups" ? ` class="active"` : ""}>Groups</button>
      </div>
      ${routingTab === "flow"
        ? `<div>${cfg.groups.map(flowGroupHtml).join("")}</div>`
        : `<div class="grid2" style="margin-top:12px">${cfg.groups.map(groupCardHtml).join("")}</div>`}
    </div>`
    : emptyHtml("No model groups are configured for the gateway.");

  /* ---- Routing rules (routing-rules design §7): the management
   * surface for typed rules. The header renders always — it is the
   * creation surface, not a data block; the body renders when rules
   * exist or the form is open. Typed rules render as Label: Value
   * rows with enable/disable (icon-only), edit (icon-only), and
   * Delete (words + icon, destructive — principle 15); legacy
   * fragment-only rules stay read-only (design §6.1) with their
   * fragment behind a per-row expandable. The source composition
   * bar survives from the read-only card for mixed rule lists. ---- */
  /** A group's hard window, mirroring manager-gateway's baking:
   *  the minimum over its enabled member backends of
   *  min(configured context, the model's rated context). Null when
   *  no enabled backend member bounds the group. */
  const groupHardWindow = (g: GatewayGroup): number | null => {
    let hard: number | null = null;
    for (const member of g.members) {
      const b = backendById.get(member);
      if (!b || !b.enabled || b.n_ctx === undefined) continue;
      const mdl = models.find((m) => sameModelFile(b.model_file, m.gguf_path));
      const cap = mdl?.ctx_max ? mdl.ctx_max : null;
      const w = cap !== null ? Math.min(b.n_ctx, cap) : b.n_ctx;
      hard = hard === null ? w : Math.min(hard, w);
    }
    return hard;
  };
  /** The §6.2 warnings for a rule draft targeting `target`,
   *  computed from the same data the generator bakes from. */
  const ruleWarnings = (target: string): string[] => {
    const warnings: string[] = [];
    const tg = cfg.groups.find((g) => g.name === target);
    if (!tg) return warnings;
    if (tg.enabled_members === 0) {
      warnings.push(`The target group ${target} has no enabled members — rerouted requests will fail until a member backend is enabled.`);
    }
    const targetHard = groupHardWindow(tg);
    if (targetHard !== null) {
      for (const g of cfg.groups) {
        if (g.name === target) continue;
        const h = groupHardWindow(g);
        if (h !== null && targetHard <= h) {
          warnings.push(`The target's window (${fmtCount(targetHard)} tokens) is not larger than ${g.name}'s (${fmtCount(h)} tokens) — the rule cannot help ${g.name}.`);
        }
      }
    }
    for (const g of cfg.groups) {
      for (const member of g.members) {
        const raw = ruleRawFlags.get(member);
        if (!raw) continue;
        const toks = raw.split(/\s+/);
        for (let i = 0; i + 1 < toks.length; i++) {
          if ((toks[i] === "--parallel" || toks[i] === "-np") && Number(toks[i + 1]) > 1) {
            warnings.push(`${member} runs with ${toks[i]} ${toks[i + 1]} — its per-slot context is smaller than the window the rule assumes, so rerouting can trigger later than the rule intends.`);
          }
        }
      }
    }
    return warnings;
  };
  const ruleOptimalText = (r: GatewayRule): string => {
    const k = r.kind as GatewayRuleKind;
    if (k.optimal_tokens !== null) return `${fmtCount(k.optimal_tokens)} tokens`;
    const tg = cfg.groups.find((g) => g.name === k.target_group);
    const hard = tg ? groupHardWindow(tg) : null;
    const pct = `${Math.round(k.optimal_fraction * 100)}%`;
    return hard !== null
      ? `${fmtCount(Math.floor(k.optimal_fraction * hard))} tokens (${pct} of the target window)`
      : `${pct} of the serving window`;
  };
  const typedRuleRow = (r: GatewayRule): string => {
    const k = r.kind as GatewayRuleKind;
    return `<div style="display:flex;gap:14px;align-items:center;padding:6px 0;border-bottom:1px solid var(--border);flex-wrap:wrap">
      <span style="font-size:12px">Type: <b>Context overflow</b></span>
      <span style="font-size:12px">Target: <b>${esc(k.target_group)}</b></span>
      <span style="font-size:12px">Optimal: <b>${esc(ruleOptimalText(r))}</b></span>
      <span style="font-size:12px">Advisory: <b>${k.advisory ? "On" : "Off"}</b></span>
      ${r.description ? `<span class="info-tip" title="${esc(r.description)}">${icon("info")}</span>` : ""}
      ${r.enabled ? "" : `<span class="chip status-tag status-off">Disabled</span>`}
      <span class="spacer" style="flex:1"></span>
      ${enableSwitch({ on: r.enabled, name: `Routing rule ${r.id}`, attrs: `data-rule-toggle="${esc(r.id)}"` })}
      <button class="btn icon-only" data-rule-edit="${esc(r.id)}" title="Edit rule" aria-label="Edit rule ${esc(r.id)}">${icon("pencil")}</button>
      <button class="btn danger${confirmRuleDelete.has(r.id) ? " armed" : ""}" data-rule-delete="${esc(r.id)}">${icon("trash")} ${confirmRuleDelete.has(r.id) ? "Confirm delete" : "Delete"}</button>
    </div>`;
  };
  const legacyRuleRow = (r: GatewayRule): string => {
    const open = openFragments.has(r.id);
    return `<div style="display:flex;gap:8px;align-items:center;padding:6px 0;border-bottom:1px solid var(--border)">
      <span style="font-size:12px">Rule: <span style="font-family:var(--mono)">${esc(r.id)}</span></span>
      <span class="info-tip" title="${esc(r.description)}">${icon("info")}</span>
      <span class="spacer" style="flex:1"></span>
      ${r.litellm_fragment ? `<button class="btn" data-fragment="${esc(r.id)}" aria-expanded="${open}" title="Fragment" aria-label="Fragment" style="padding:2px 8px;font-size:11px">${icon(open ? "chevD" : "chevR")}</button>` : ""}
      <span class="chip">${esc(r.source)}</span></div>
      ${open && r.litellm_fragment ? `<pre style="margin:6px 0;padding:10px 12px;background:var(--panel2);border:1px solid var(--border);border-radius:8px;font-family:var(--mono);font-size:11px;white-space:pre-wrap;word-break:break-word">${esc(r.litellm_fragment)}</pre>` : ""}`;
  };
  const ruleFormHtml = (): string => {
    if (!ruleForm.open) return "";
    const warnings = ruleWarnings(ruleForm.target);
    const targetOptions = cfg.groups
      .map((g) => `<option value="${esc(g.name)}"${g.name === ruleForm.target ? " selected" : ""}>${esc(g.name)}</option>`)
      .join("");
    return `<div style="margin-top:10px;border-top:1px solid var(--border);padding-top:10px">
      <div style="display:flex;gap:16px;align-items:flex-end;flex-wrap:wrap">
        <span style="font-size:12px">Type: <b>Context overflow</b></span>
        <label style="font-size:12px">Target group
          <select id="gwRuleTarget" class="input" style="display:block;margin-top:4px">${targetOptions}</select></label>
        <label style="font-size:12px">Optimal tokens<span class="info-tip" title="Past this conversation size the serving model adds the compaction note. Empty = the default: half the serving model's window.">${icon("info")}</span>
          <input id="gwRuleOptimal" class="input" type="number" min="1" step="1" placeholder="Default (half the window)" value="${esc(ruleForm.optimalTokens)}" style="display:block;margin-top:4px"></label>
        <label style="font-size:12px;display:flex;gap:6px;align-items:center;padding-bottom:6px"><input type="checkbox" id="gwRuleAdvisory"${ruleForm.advisory ? " checked" : ""}> Advisory<span class="info-tip" title="When on, the serving model appends the compaction note past the optimal window. Off = reroute silently.">${icon("info")}</span></label>
      </div>
      ${warnings.map((w) => `<div style="color:var(--amber);font-size:12px;margin-top:6px">${icon("warn")} ${esc(w)}</div>`).join("")}
      <div class="card-actions" style="margin-top:10px">
        <button class="btn" id="gwRuleSaveBtn">${icon("check")} Save rule</button>
        <button class="btn" id="gwRuleCancelBtn">Cancel</button>
      </div>
    </div>`;
  };
  const rules = (() => {
    const bySource = new Map<string, number>();
    for (const r of cfg.routing_rules) bySource.set(r.source, (bySource.get(r.source) ?? 0) + 1);
    const tones = ["#6d7686", "#3a4350", "#a7b0bf", "#545e6e"];
    const showBar = cfg.routing_rules.length >= 2 && bySource.size >= 2;
    const bar = showBar
      ? `<div style="margin-bottom:6px">${stackedBar([...bySource.entries()].map(([src, cnt], i) => ({
          pct: (cnt / cfg.routing_rules.length) * 100,
          color: tones[i % tones.length],
          label: `${src}: ${cnt}`,
        })), 600, 8)}
        <div class="legend" style="margin-top:4px">${[...bySource.entries()].map(([src], i) => `<span><span class="sw" style="background:${tones[i % tones.length]}"></span>${esc(src)}</span>`).join("")}</div></div>`
      : "";
    const rows = cfg.routing_rules.map((r) => (r.kind ? typedRuleRow(r) : legacyRuleRow(r))).join("");
    return `<div class="card" style="margin-top:12px"><div class="sec-title">${icon("gateway")} Routing rules<span class="info-tip" title="A context-overflow rule reroutes a request too large for its model group to the target group instead of failing it, and past the optimal window the serving model adds a one-line note advising a compacted or fresh conversation. Changes take effect when the gateway restarts — save, then apply the Activation Plan.">${icon("info")}</span>
      <span class="spacer" style="flex:1"></span>
      <button class="btn" id="gwRuleAddBtn">${icon("plus")} Add rule</button></div>
      ${bar}${rows}${ruleFormHtml()}</div>`;
  })();

  /* ---- Generated config: one section, two views (design §7) —
   * config.yaml, plus the llmdil_routing.py callback as a second
   * tab when a typed routing rule is enabled. One collapsed row
   * (title, summary values once fetched, Copy / Download / toggle
   * acting on the active tab); the methodology sentences ride
   * info-tips verbatim; a fetch in flight renders "Loading…" in
   * the pre region, never an empty pre; a load failure is a header
   * value with the raw error in its tooltip. ---- */
  const hasRoutingCallback = cfg.routing_rules.some((r) => r.kind !== null && r.enabled);
  const activeGenText = genTab === "routing" ? routingPyText : yamlText;
  const genSummary = activeGenText !== null && !yamlErr
    ? (() => {
        const lines = activeGenText.replace(/\n$/, "").split("\n").length;
        const models = genTab === "yaml" ? (activeGenText.match(/model_name:/g) ?? []).length : 0;
        return `<span style="font-size:12px;color:var(--text2)">Lines: <b style="color:var(--text)">${lines}</b></span>
        ${genTab === "yaml" ? `<span style="font-size:12px;color:var(--text2)">Models: <b style="color:var(--text)">${models}</b></span>` : ""}
        <span style="font-size:12px;color:var(--text2)">Source: current configuration<span class="info-tip" title="The running gateway reads the config file written when it was last (re)started from the Setup Wizard. This preview is regenerated from the current configuration — after edits, the two can differ until the gateway is restarted.">${icon("info")}</span></span>`;
      })()
    : "";
  const genTabs = hasRoutingCallback
    ? `<div class="viewsel" role="tablist" style="margin-top:10px">
        <button data-gen-tab="yaml"${genTab === "yaml" ? ` class="active"` : ""}>config.yaml</button>
        <button data-gen-tab="routing"${genTab === "routing" ? ` class="active"` : ""}>llmdil_routing.py</button>
      </div>`
    : "";
  const yaml = `<div class="card" style="margin-top:12px">
    <div style="display:flex;gap:10px;align-items:center;flex-wrap:wrap">
      <span class="sec-title" style="margin:0">Generated config (preview)</span>
      ${yamlErr ? `<span style="font-size:12px;color:var(--err)" title="${esc(yamlErr)}">Preview: failed to load</span>` : genSummary}
      <span class="spacer" style="flex:1"></span>
      <button class="btn icon-only" id="gwYamlCopyBtn" title="Copy generated config" aria-label="Copy generated config">${icon("copy")}</button>
      <button class="btn icon-only" id="gwYamlDownloadBtn" title="Download ${genTab === "routing" ? "llmdil_routing.py" : "config.yaml"}" aria-label="Download ${genTab === "routing" ? "llmdil_routing.py" : "config.yaml"}">${icon("download")}</button>
      <button class="btn icon-only" id="gwYamlBtn" title="${yamlOpen ? "Hide preview" : "Preview generated config"}" aria-label="${yamlOpen ? "Hide preview" : "Preview generated config"}">${icon(yamlOpen ? "eyeOff" : "eye")}</button>
    </div>
    ${genTabs}
    ${yamlOpen && !yamlErr ? (activeGenText === null
      ? `<pre style="margin-top:10px;padding:12px;background:var(--panel2);border:1px solid var(--border);border-radius:8px;font-family:var(--mono);font-size:11px">Loading\u2026</pre>`
      : `<div style="display:flex;gap:8px;align-items:center;margin-top:10px"><span style="font-size:11px;color:var(--text3);font-family:var(--mono)">${genTab === "routing" ? "llmdil_routing.py" : "config.yaml"}</span>${genTab === "yaml" ? `<span class="info-tip" title="\`api_key: local\` in the generated config is LiteLLM's required non-empty placeholder — it is not a real key.">${icon("info")}</span>` : ""}</div>
         <pre style="margin-top:6px;padding:12px;background:var(--panel2);border:1px solid var(--border);border-radius:8px;font-family:var(--mono);font-size:11px;white-space:pre-wrap;word-break:break-word;max-height:420px;overflow:auto">${esc(activeGenText)}</pre>`) : ""}
  </div>`;

  return activationPlanHtml(plan, planUi, "summary") + backendsNote + summary + gwActions + endpoint + openrouter + routing + rules + yaml;
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
  /** LAN address from the latest load (the LAN base URL copy target). */
  let lastLanIp: string | null = null;
  /** Master key, fetched on first Reveal/Copy; kept in the closure
   *  so repaints do not re-read the credential store. */
  let masterKey: string | null = null;
  let keyRevealed = false;
  /** Gateway activation plan, painted at the top of the view. */
  let lastPlan: ActivationPlanData | null = null;
  const planUi = newPlanUi();
  /** Routing section tab (Flow | Groups), the OpenRouter allowlist
   * expandable, and the routing-rule Fragment expandables — view
   * state held outside the repainted DOM. */
  let routingTab: "flow" | "groups" = "flow";
  let orModelsOpen = false;
  const openFragments = new Set<string>();
  /** Models from the latest load (the routing-rule windows and
   *  form warnings resolve each backend's rated context from
   *  them, mirroring the generator). */
  let lastModels: ModelRow[] = [];
  /** Routing-rules form draft + Delete arming + the lazily fetched
   *  per-backend raw flags behind the form's --parallel warning. */
  const ruleForm: RuleFormState = { open: false, editingId: null, target: "", optimalTokens: "", advisory: true };
  const confirmRuleDelete = new Set<string>();
  const ruleRawFlags = new Map<string, string | null>();
  /** Generated config card: active tab + the routing callback
   *  preview text (fetched with the YAML). */
  let genTab: "yaml" | "routing" = "yaml";
  let routingPyText: string | null = null;

  const gwMsg = (text: string, isErr = false): void => {
    const m = root.querySelector("#gwMsg");
    if (m) m.innerHTML = text ? `<span style="color:${isErr ? "var(--err)" : "var(--text2)"}">${esc(text)}</span>` : "";
  };

  /** Append a manual-command block under the gateway message (a
   * failed start-gateway outcome carries one: the exact commands
   * to start the gateway by hand). #gwMsg lives outside #gwBody,
   * so the block survives the load() repaint that follows. The
   * command is also recorded on the plan UI state so the block's
   * Copy button (data-plan-act="copy-manual") copies this text. */
  const gwShowManual = (cmd: string): void => {
    planUi.manualCommand = cmd;
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
    const [st, cfg, backs, pr, mdls] = await Promise.all([getGatewayStatus(), getGatewayConfig(), getBackends(), getActivationPlan(), getModels()]);
    if (disposed) return;
    if (!pr.error && pr.data) lastPlan = pr.data;
    if (!mdls.error) lastModels = mdls.data;
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
    lastLanIp = st.data.lan_ip ?? null;
    b.innerHTML = gatewayPaint(st.data, cfg.data, backs.data, lastModels, yamlOpen, yamlText, yamlErr, starting, stopping, backs.error ?? null, masterKey, keyRevealed, lastPlan, planUi, routingTab, orModelsOpen, openFragments, ruleForm, confirmRuleDelete, ruleRawFlags, genTab, routingPyText);
  };

  /** Fetch the generated config when a Copy/Download click needs
   * it and none is loaded (or the last fetch failed) — the buttons
   * never act on an empty string. Returns the ACTIVE TAB's text
   * (the routing callback tab when selected), or null. */
  const ensureGen = async (): Promise<string | null> => {
    const have = genTab === "routing" ? routingPyText : yamlText;
    if (have !== null && !yamlErr) return have;
    const r = await getGatewayYaml();
    if (disposed) return null;
    if (r.error) { yamlErr = r.error; await load(); return null; }
    yamlText = r.data.yaml;
    routingPyText = r.data.routing_py;
    yamlErr = "";
    return genTab === "routing" ? routingPyText : yamlText;
  };

  /** Serialize a rule back to the stored config shape for the
   *  update_gateway_config merge-patch. The fragment rides its
   *  untouched raw value, so saving an unrelated rule never
   *  rewrites a legacy rule's stored fragment. */
  const serializeRule = (r: GatewayRule): Record<string, unknown> => ({
    id: r.id,
    description: r.description,
    enabled: r.enabled,
    kind: r.kind,
    litellm_fragment: r.fragment_raw === undefined ? null : r.fragment_raw,
    source: r.source,
  });

  /** Save the full rules array through update_gateway_config (the
   *  generic gateway merge-patch — no dedicated command, design
   *  §7) and repaint with the outcome. */
  const patchRules = async (rules: GatewayRule[], okMsg: string): Promise<void> => {
    const r = await updateGatewayConfig({ routing_rules: rules.map(serializeRule) });
    if (disposed) return;
    if (r.error) {
      gwMsg(`Routing rule change failed: ${r.error}`, true);
    } else if (!r.data.persisted) {
      gwMsg("The change was applied to the in-memory configuration only — it was not saved and will be lost when the app restarts.", true);
    } else {
      gwMsg(okMsg);
    }
    await load();
  };

  /** Open the rule form (add when `editing` is null, else edit that
   *  typed rule). Member backends' raw flags are fetched lazily
   *  for the form's --parallel warning (best-effort). */
  const openRuleForm = async (editing: GatewayRule | null): Promise<void> => {
    if (editing?.kind) {
      ruleForm.editingId = editing.id;
      ruleForm.target = editing.kind.target_group;
      ruleForm.optimalTokens = editing.kind.optimal_tokens !== null ? String(editing.kind.optimal_tokens) : "";
      ruleForm.advisory = editing.kind.advisory;
    } else {
      ruleForm.editingId = null;
      ruleForm.target = lastCfg?.groups[0]?.name ?? "";
      ruleForm.optimalTokens = "";
      ruleForm.advisory = true;
    }
    ruleForm.open = true;
    const ids = new Set((lastCfg?.groups ?? []).flatMap((g) => g.members));
    await Promise.all([...ids].map(async (id) => {
      if (ruleRawFlags.has(id)) return;
      const r = await getBackendConfig(id);
      if (r.data) ruleRawFlags.set(id, r.data.raw_flags);
    }));
    if (!disposed) await load();
  };

  const onClick = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLElement;

    const pt = t.closest("[data-plan-act]") as HTMLElement | null;
    if (pt) {
      await handlePlanAction(pt.getAttribute("data-plan-act") ?? "", planUi, () => { void load(); }, load, lastPlan);
      return;
    }

    const rt = t.closest("[data-routing-tab]") as HTMLElement | null;
    if (rt) {
      routingTab = rt.getAttribute("data-routing-tab") === "groups" ? "groups" : "flow";
      await load();
      return;
    }

    const fr = t.closest("[data-fragment]") as HTMLElement | null;
    if (fr) {
      const id = fr.getAttribute("data-fragment") ?? "";
      if (openFragments.has(id)) openFragments.delete(id); else openFragments.add(id);
      await load();
      return;
    }

    if (t.closest("[data-or-models]")) {
      orModelsOpen = !orModelsOpen;
      await load();
      return;
    }

    if (t.closest("#gwOrCredCopyBtn")) {
      const ref = lastCfg?.openrouter?.cred_ref;
      if (!ref) return;
      try {
        await navigator.clipboard.writeText(ref);
        gwMsg("Credential reference copied to the clipboard.");
      } catch {
        gwMsg("Copy failed — copy the credential reference manually.", true);
      }
      return;
    }

    if (t.closest("#gwYamlCopyBtn")) {
      const text = await ensureGen();
      if (text === null || disposed) return;
      try {
        await navigator.clipboard.writeText(text);
        gwMsg("Generated config copied to the clipboard.");
      } catch {
        gwMsg("Copy failed — open the preview and copy it manually.", true);
      }
      await load();
      return;
    }

    if (t.closest("#gwYamlDownloadBtn")) {
      const text = await ensureGen();
      if (text === null || disposed) return;
      const url = URL.createObjectURL(new Blob([text], { type: "text/yaml" }));
      const a = document.createElement("a");
      a.href = url;
      a.download = genTab === "routing" ? "llmdil_routing.py" : "config.yaml";
      a.click();
      URL.revokeObjectURL(url);
      await load();
      return;
    }

    const gt = t.closest("[data-gen-tab]") as HTMLElement | null;
    if (gt) {
      genTab = gt.getAttribute("data-gen-tab") === "routing" ? "routing" : "yaml";
      await load();
      return;
    }

    if (t.closest("#gwRuleAddBtn")) {
      await openRuleForm(null);
      return;
    }

    const rtog = t.closest("[data-rule-toggle]") as HTMLElement | null;
    if (rtog && lastCfg) {
      const id = rtog.getAttribute("data-rule-toggle") ?? "";
      const rules = lastCfg.routing_rules.map((r) => (r.id === id ? { ...r, enabled: !r.enabled } : r));
      const rule = rules.find((r) => r.id === id);
      await patchRules(rules, rule?.enabled ? `Routing rule ${id} enabled and saved.` : `Routing rule ${id} disabled and saved.`);
      return;
    }

    const redit = t.closest("[data-rule-edit]") as HTMLElement | null;
    if (redit && lastCfg) {
      const rule = lastCfg.routing_rules.find((r) => r.id === (redit.getAttribute("data-rule-edit") ?? ""));
      if (rule) await openRuleForm(rule);
      return;
    }

    const rdel = t.closest("[data-rule-delete]") as HTMLElement | null;
    if (rdel && lastCfg) {
      const id = rdel.getAttribute("data-rule-delete") ?? "";
      if (!confirmRuleDelete.has(id)) {
        confirmRuleDelete.add(id);
        await load();
        return;
      }
      confirmRuleDelete.delete(id);
      await patchRules(lastCfg.routing_rules.filter((r) => r.id !== id), `Routing rule ${id} deleted and saved.`);
      return;
    }

    if (t.closest("#gwRuleCancelBtn")) {
      ruleForm.open = false;
      await load();
      return;
    }

    if (t.closest("#gwRuleSaveBtn")) {
      if (!lastCfg) return;
      const optimalText = ruleForm.optimalTokens.trim();
      const optimal = optimalText === "" ? null : Number(optimalText);
      if (optimal !== null && (!Number.isFinite(optimal) || optimal <= 0 || Math.floor(optimal) !== optimal)) {
        gwMsg("Optimal tokens must be a positive whole number, or empty for the default (half the serving window).", true);
        return;
      }
      const editing = ruleForm.editingId !== null
        ? lastCfg.routing_rules.find((r) => r.id === ruleForm.editingId)
        : undefined;
      const kind: GatewayRuleKind = {
        type: "context-overflow",
        target_group: ruleForm.target,
        optimal_fraction: editing?.kind?.optimal_fraction ?? 0.5,
        optimal_tokens: optimal,
        output_reserve_tokens: editing?.kind?.output_reserve_tokens ?? 1024,
        advisory: ruleForm.advisory,
      };
      const description = `Requests too large for their group are rerouted to ${ruleForm.target}; past the optimal window the serving model advises compacting the conversation`;
      let rules: GatewayRule[];
      if (editing) {
        rules = lastCfg.routing_rules.map((r) => (r.id === editing.id ? { ...r, kind, description } : r));
      } else {
        let id = "context-overflow";
        for (let n = 2; lastCfg.routing_rules.some((r) => r.id === id); n++) id = `context-overflow-${n}`;
        rules = [...lastCfg.routing_rules, { id, description, source: "user", enabled: true, kind, litellm_fragment: undefined, fragment_raw: undefined }];
      }
      ruleForm.open = false;
      await patchRules(rules, editing ? `Routing rule ${editing.id} saved.` : "Routing rule added and saved.");
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
      routingPyText = null;
      yamlErr = "";
      await load();
      const r = await getGatewayYaml();
      if (disposed) return;
      if (r.error) yamlErr = r.error;
      else {
        yamlText = r.data.yaml;
        routingPyText = r.data.routing_py;
      }
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

    if (t.closest("#gwBaseUrlCopyBtn")) {
      try {
        await navigator.clipboard.writeText(`http://127.0.0.1:${lastPort}/v1`);
        gwMsg("Base URL copied to the clipboard.");
      } catch {
        gwMsg("Copy failed \u2014 copy the base URL manually.", true);
      }
      return;
    }

    if (t.closest("#gwLanUrlCopyBtn")) {
      if (!lastLanIp) return;
      try {
        await navigator.clipboard.writeText(`http://${lastLanIp}:${lastPort}/v1`);
        gwMsg("LAN base URL copied to the clipboard.");
      } catch {
        gwMsg("Copy failed \u2014 copy the LAN base URL manually.", true);
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
      // MCP credential prompt (Pete, 2026-10-18): Gateway MCP
      // servers the start would omit for want of a credential are
      // offered registration/storage first — before the stop, so
      // cancelling leaves a running gateway untouched, and before
      // the step, so a stored credential lands in this start's
      // config.
      const proceed = await promptForUnarmedMcpCredentials();
      if (disposed) { starting = false; return; }
      if (!proceed) {
        starting = false;
        gwMsg("Gateway start cancelled at the MCP credential prompt.");
        await load();
        return;
      }
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
  /** Rule-form field changes update the draft in the closure (the
   *  form re-renders from it); Target/Optimal changes repaint so
   *  the §6.2 warnings track the draft. */
  const onRuleFormChange = async (ev: Event): Promise<void> => {
    const el = ev.target as HTMLElement;
    if (el.closest("#gwRuleTarget")) {
      ruleForm.target = (el as HTMLSelectElement).value;
      await load();
    } else if (el.closest("#gwRuleOptimal")) {
      ruleForm.optimalTokens = (el as HTMLInputElement).value;
      await load();
    } else if (el.closest("#gwRuleAdvisory")) {
      ruleForm.advisory = (el as HTMLInputElement).checked;
    }
  };
  root.addEventListener("click", onClick);
  root.addEventListener("change", onBindChange);
  root.addEventListener("change", onRuleFormChange);

  await load();

  // Status poll: a gateway that dies while the view is open must show
  // as down without a manual refresh. The repaint replaces #gwBody's
  // innerHTML wholesale, which destroys the node under the pointer —
  // a native title tooltip on an info-tip inside the body is
  // cancelled before it shows or dismissed while it is read (the
  // Routing test traces tip was reported exactly this way: cursor
  // changes, no tooltip). While the pointer hovers the body the
  // user is reading it, so the periodic repaint waits for the next
  // cycle; click-driven loads are unaffected.
  const timer = window.setInterval(() => {
    if (!disposed && !starting && !stopping && !root.querySelector("#gwBody:hover")) void load();
  }, 5000);

  return () => {
    disposed = true;
    window.clearInterval(timer);
    root.removeEventListener("click", onClick);
    root.removeEventListener("change", onBindChange);
    root.removeEventListener("change", onRuleFormChange);
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
    arrays/objects as counts — never invented detail. Numbers route
    through the shared formatter (one decimal, trailing .0 dropped,
    thousands separators) — this path is the settings view's leak
    point for raw full-precision values. */
function summarizeValue(v: unknown): string {
  if (typeof v === "number") return trim1(v);
  const p = primStr(v);
  if (p !== null) return p;
  if (Array.isArray(v)) return `${v.length} item${v.length === 1 ? "" : "s"}`;
  const r = asRecord(v);
  if (r) return `{ ${Object.keys(r).length} fields }`;
  return "\u2014";
}

/** Config keys are storage names, not labels — humanize them for
 *  display (known keys map to their noun label; anything else gets
 *  its underscores spaced and its first letter capitalized). */
const SETTINGS_LABEL: Record<string, string> = {
  poll_interval_ms: "Poll interval", interval_ms: "Interval", poll_ms: "Poll interval", interval: "Interval",
};
function humanLabel(key: string): string {
  return SETTINGS_LABEL[key] ?? key.replace(/_/g, " ").replace(/^\w/, (c) => c.toUpperCase());
}
function humanEntries(rec: Record<string, unknown>, limit = 24): { key: string; value: string }[] {
  return Object.entries(rec).slice(0, limit).map(([key, v]) => ({ key: humanLabel(key), value: summarizeValue(v) }));
}

type SettingsRow = { key: string; value: string; tip?: string; copy?: string };

/** Label: Value rows. An empty list renders NOTHING (principle 13 —
 *  no "Nothing reported." placeholder). A row may carry an info-tip
 *  (narrative) and a Copy button (addresses: paths). */
function settingsRows(rows: SettingsRow[]): string {
  if (!rows.length) return "";
  return `<div class="kv" style="gap:10px">${rows.map((r) => `<div style="display:flex;justify-content:space-between;gap:16px"><span class="k">${esc(r.key)}${r.tip ? `<span class="info-tip" title="${esc(r.tip)}">${icon("info")}</span>` : ""}</span><span class="v" style="font-size:12px;text-align:right">${esc(r.value)}${r.copy !== undefined ? ` <button class="btn" data-copy-value="${esc(r.copy)}" title="Copy" aria-label="Copy value" style="padding:1px 7px;font-size:11px">${icon("copy")}</button>` : ""}</span></div>`).join("")}</div>`;
}

const MXC_MODES: { mode: string; label: string; def: string }[] = [
  { mode: "learning", label: "Learning", def: "Learning — observe and log only; nothing is blocked." },
  { mode: "enforced", label: "Enforced", def: "Enforced — execution outside the policy is blocked." },
  { mode: "disabled", label: "Disabled", def: "Disabled — sandbox off; tool execution is not mediated." },
];

const FS_ACCESS_LABEL: Record<string, string> = { readwrite: "read-write" };
const ACTIVITY_KIND_LABEL: Record<string, string> = {
  FileRead: "file read", FileWrite: "file write", Network: "network",
};

/** The Active policy body: the policy's actual rules, with the
 *  provenance in the heading's info-tip — a fetch failure is
 *  "unavailable — <error>", never conflated with a genuinely absent
 *  policy (which renders no block at all). The three rule lists are
 *  ONE "Policy rules" block behind a segmented filter whose keys
 *  carry the counts; a long list is complete inside a collapsed
 *  expandable — truncation is gone, every rule is reachable. */
function mxcPolicyInner(
  policyR: { data: MxcPolicyData | null; error?: string; live: boolean },
  rulesFilter: "fs" | "net" | "cred",
  rulesOpen: Set<string>,
  policyFile: string | null,
): string {
  if (policyR.error) {
    return `<div style="font-size:12px;color:var(--err);margin-top:10px">Policy unavailable \u2014 ${esc(policyR.error)}</div>`;
  }
  const data = policyR.data;
  if (!data || !data.policy) return "";
  const p = data.policy;
  const srcLine = data.source === "default"
    ? "No policy file exists \u2014 the built-in default policy is in effect."
    : "Loaded from the configured policy file.";
  const basics: SettingsRow[] = [
    ...(policyFile !== null ? [{ key: "Policy file", value: policyFile, copy: policyFile } as SettingsRow] : []),
    { key: "Policy version", value: String(p.version) },
    { key: "Default", value: p.default_deny ? "deny" : "allow", ...(p.default_deny ? { tip: "anything not explicitly allowed is denied" } : {}) },
    { key: "UI access", value: p.ui_access ? "enabled" : "disabled" },
  ];
  const lists: { id: "fs" | "net" | "cred"; label: string; rows: SettingsRow[] }[] = [
    { id: "fs", label: "Filesystem", rows: (p.filesystem ?? []).map((r) => ({ key: r.path, value: FS_ACCESS_LABEL[r.access] ?? r.access })) },
    { id: "net", label: "Network", rows: (p.network ?? []).map((r) => ({ key: r.ports && r.ports.length ? `${r.host}:${r.ports.join(",")}` : r.host, value: r.decision })) },
    { id: "cred", label: "Credentials", rows: (p.credentials ?? []).map((r) => ({ key: r.name, value: r.target })) },
  ];
  const active = lists.find((l) => l.id === rulesFilter) ?? lists[0];
  const LONG = 12;
  const listHtml = active.rows.length > LONG && !rulesOpen.has(active.id)
    ? `<button class="btn" data-rules-toggle="${active.id}" title="Show all ${active.rows.length} rules" aria-label="Show all ${active.rows.length} rules" style="padding:2px 8px;font-size:11px">${icon("chevR")}</button>`
    : `${active.rows.length > LONG ? `<button class="btn" data-rules-toggle="${active.id}" title="Hide rules" aria-label="Hide rules" style="padding:2px 8px;font-size:11px;margin-bottom:6px">${icon("chevD")}</button>` : ""}${settingsRows(active.rows)}`;
  return `<div style="font-size:12px;color:var(--text2);margin:14px 0 6px">Active policy<span class="info-tip" title="${esc(srcLine)}">${icon("info")}</span></div>
    ${settingsRows(basics)}
    <div style="font-size:12px;color:var(--text2);margin:12px 0 6px">Policy rules</div>
    <div class="viewsel" role="tablist" style="margin-bottom:8px">${lists.map((l) => `<button data-rules-filter="${l.id}"${l.id === active.id ? ` class="active"` : ""}>${l.label} (${l.rows.length})</button>`).join("")}</div>
    ${listHtml}`;
}

/** The Recent activity body: structured rows (time, decision chip
 *  colored per principle 11, what was accessed, count) under a
 *  decision-mix segmented bar; the entry description rides the
 *  row's tooltip. Fetch failure stays distinct from an empty
 *  report, and an empty report renders no block at all. */
function mxcActivityInner(activityR: { data: MxcActivityData | null; error?: string; live: boolean }): string {
  if (activityR.error) {
    return `<div style="font-size:12px;color:var(--err);margin-top:10px">Activity unavailable \u2014 ${esc(activityR.error)}</div>`;
  }
  const data = activityR.data;
  if (!data || !data.entries.length) return "";
  const totals = new Map<string, number>();
  for (const e of data.entries) totals.set(e.decision, (totals.get(e.decision) ?? 0) + (e.count || 1));
  const totalAll = [...totals.values()].reduce((a, b) => a + b, 0);
  const decColor = (d: string): string => d === "allow" ? C.green : d === "deny" ? "#f85149" : d === "prompt" ? C.amber : "#6d7686";
  const mixBar = totalAll > 0
    ? `<div style="margin-bottom:6px">${stackedBar([...totals.entries()].map(([d, n]) => ({ pct: (n / totalAll) * 100, color: decColor(d), label: `${d}: ${n.toLocaleString()}` })), 600, 8)}
       <div class="legend" style="margin-top:4px">${[...totals.entries()].map(([d, n]) => `<span><span class="sw" style="background:${decColor(d)}"></span>${esc(d)} ${n.toLocaleString()}</span>`).join("")}</div></div>`
    : "";
  const decChip = (d: string): string => d === "allow" ? "badge-ok" : d === "deny" ? "badge-err" : d === "prompt" ? "badge-warn" : "";
  const rows = data.entries.map((e) => {
    const kind = e.kind ? (ACTIVITY_KIND_LABEL[e.kind] ?? e.kind) : "";
    const target = e.path_or_host ? e.path_or_host + (e.port !== null ? `:${e.port}` : "") : "";
    const what = [kind, target].filter(Boolean).join(" \u00b7 ");
    return `<div style="padding:4px 0;border-bottom:1px solid var(--border);font-size:12px"${e.description ? ` title="${esc(e.description)}"` : ""}>
      <span title="${esc(fmtDateTime(e.timestamp))}">${esc(fmtTime(e.timestamp))}</span>
      <span class="chip ${decChip(e.decision)}">${esc(e.decision)}</span>
      ${what ? `<span>${esc(what)}</span>` : ""}
      ${e.count > 1 ? `<span style="color:var(--text3)">\u00d7${e.count}</span>` : ""}
    </div>`;
  }).join("");
  return `<div style="font-size:12px;color:var(--text2);margin:14px 0 6px">Recent activity</div>${mixBar}${rows}`;
}

type SettingsTab = "general" | "remote" | "updates" | "notifications" | "mxc" | "supervision";

interface SettingsUi {
  tab: SettingsTab;
  rulesFilter: "fs" | "net" | "cred";
  rulesOpen: Set<string>;
  allOpen: boolean;
}

/** The LLMdil self-update block at the top of the Updates tab:
 *  the two operator switches, the check state as Label: Value rows,
 *  and the check / download actions. Fed by the app_update_status
 *  payload, not the config: last-checked, available, and staged
 *  are runtime state. The per-component pins below it are a
 *  separate mechanism (the wizard's component updates) and stay
 *  separate here too. Applying a staged update is the shell
 *  banner's Restart, not a control on this tab. */
function appUpdateInner(upd: AppUpdateInfo | null, live: boolean): string {
  const head = `<div style="font-size:12px;color:var(--text2)">LLMdil updates<span class="info-tip" title="Checks GitHub Releases for a newer LLMdil. Auto-update downloads and verifies the installer in the background; it is applied only when you restart from the banner — the running installation is never replaced in place.">${icon("info")}</span></div>`;
  if (!live) {
    return `${head}<div style="font-size:12px;color:var(--text2);margin-top:6px">Update checking is available in the installed app.</div>`;
  }
  if (!upd) {
    return `${head}<div style="font-size:12px;color:var(--text2);margin-top:6px">Update state unavailable.</div>`;
  }
  const fmtMs = (ms: number): string => fmtDateTime(new Date(ms).toISOString());
  const switchRow = (key: string, label: string, desc: string, on: boolean, disabled: boolean): string =>
    `<div style="display:flex;justify-content:space-between;align-items:center;gap:16px;padding:4px 0;border-bottom:1px solid var(--border)">
      <span class="k">${esc(label)}<span class="info-tip" title="${esc(desc)}">${icon("info")}</span></span>
      ${enableSwitch({ on, name: label, attrs: `data-appupd-toggle="${key}"`, disabled })}</div>`;
  const rows: { key: string; value: string }[] = [
    { key: "Current version", value: upd.current_version },
  ];
  if (upd.last_checked_unix_ms !== null) {
    const outcome = upd.last_check_outcome === "update_available" ? "update available"
      : upd.last_check_outcome === "up_to_date" ? "up to date"
      : upd.last_check_outcome === "failed" ? `failed${upd.last_check_error ? ` — ${upd.last_check_error}` : ""}`
      : "";
    rows.push({ key: "Last checked", value: `${fmtMs(upd.last_checked_unix_ms)}${outcome ? ` · ${outcome}` : ""}` });
  }
  if (upd.available) rows.push({ key: "Available version", value: upd.available.version });
  if (upd.staged) rows.push({ key: "Staged", value: `${upd.staged.version} · staged ${fmtMs(upd.staged.staged_at_unix_ms)} — restart to apply` });
  const applyNote = upd.last_apply && !upd.last_apply.ok
    ? `<div style="font-size:12px;color:var(--err);margin-top:6px">Update to ${esc(upd.last_apply.version)} failed: ${esc(upd.last_apply.message)}</div>`
    : "";
  const stageErr = upd.last_stage_error
    ? `<div style="font-size:12px;color:var(--err);margin-top:6px">Download failed: ${esc(upd.last_stage_error)}</div>`
    : "";
  const canDownload = !!upd.available && !upd.staged && !upd.auto_update;
  return `${head}
    ${switchRow("checking", "Update checking", "Check GitHub Releases for a newer LLMdil at startup and every 6 hours while running.", upd.check_enabled, false)}
    ${switchRow("auto", "Auto-update", "Download and verify a new release automatically when a check finds one. Applying still takes a restart.", upd.auto_update, !upd.check_enabled)}
    <div style="margin-top:6px">${settingsRows(rows)}</div>
    ${applyNote}${stageErr}
    <div class="card-actions" style="margin-top:8px"><button class="btn" data-appupd-check>${icon("search")} Check now</button>
    ${canDownload ? `<button class="btn" data-appupd-download>${icon("download")} Download ${esc(upd.available!.version)}</button>` : ""}</div>`;
}

/** One Settings section, six tab views (principle 2) — page length
 *  is the longest tab, not the sum of six stacked cards. The
 *  Notifications tab renders only when notification settings
 *  exist. All narrative rides info-tips; empty blocks render
 *  nothing. */

/** The Supervision card body: mode toggle, shim/account state, and
 *  one row per managed service with its registration + SCM state and
 *  Register/Unregister actions. Registration is the per-service
 *  opt-in; the mode alone changes nothing until a service is
 *  registered (docs/windows-services-design.md). */
function supervisionInner(svcR: { data: ServicesInfo; error?: string; live: boolean }): string {
  if (svcR.error) {
    return `<div style="font-size:12px;color:var(--err)">Supervision unavailable — ${esc(svcR.error)}</div>`;
  }
  if (!svcR.live) {
    return `<div style="font-size:12px;color:var(--text2)">Service supervision is available in the installed app.</div>`;
  }
  const info = svcR.data;
  const mode = info.mode === "service" ? "service" : "process";
  const rows: { key: string; value: string }[] = [
    { key: "Service account", value: info.account ?? "—" },
    { key: "Service host (llm-service-host.exe)", value: info.host_present ? info.host_path : `missing — expected at ${info.host_path}` },
  ];
  const svcRows = info.services.map((s) => {
    const state = s.registered
      ? (typeof s.state === "string" ? s.state : "unknown")
      : "not registered";
    const btn = s.registered
      ? `<button class="btn" data-svc-unregister="${esc(s.target)}">Unregister</button>`
      : `<button class="btn" data-svc-register="${esc(s.target)}" ${info.host_present ? "" : "disabled"}>Register</button>`;
    return `<div style="display:flex;justify-content:space-between;align-items:center;gap:16px;padding:4px 0;border-bottom:1px solid var(--border);font-size:12px">
      <span>${esc(s.target === "gateway" ? "Gateway" : s.target)} <span style="color:var(--text3)">${esc(s.service_name)}</span></span>
      <span style="display:flex;align-items:center;gap:10px"><span>${esc(state)}</span>${btn}</span></div>`;
  }).join("");
  const anyUnregistered = info.services.some((s) => !s.registered);
  const pwdRow = anyUnregistered
    ? `<div style="display:flex;align-items:center;gap:10px;margin-top:10px;font-size:12px">
        <span class="k">Account password</span>
        <input type="password" id="svcPwd" autocomplete="off" style="flex:1" placeholder="Windows password for ${esc(info.account ?? "the service account")}">
      </div>
      <div style="font-size:11px;color:var(--text3);margin-top:4px">Registration runs elevated (one UAC prompt per service). Windows stores the password as the service's logon credential; the app does not keep it.</div>`
    : "";
  return `${settingsRows(rows)}
    <div class="card-actions">${["process", "service"].map((m) =>
      `<button class="btn${mode === m ? " primary" : ""}" data-sup-mode="${m}">${m === "process" ? "Process" : "Service"}</button>`).join("")}</div>
    <div style="margin-top:10px">${svcRows}</div>
    ${pwdRow}`;
}

function settingsPaint(
  cfg: Record<string, unknown>,
  policyR: { data: MxcPolicyData | null; error?: string; live: boolean },
  activityR: { data: MxcActivityData | null; error?: string; live: boolean },
  selfTestHtml: string,
  ui: SettingsUi,
  svcR: { data: ServicesInfo; error?: string; live: boolean },
  remoteHtml = "",
  appUpdateHtml = "",
): string {
  const notif = asRecord(cfg.notifications);
  const tabs: { id: SettingsTab; label: string }[] = [
    { id: "general", label: "General" },
    { id: "remote", label: "Remote access" },
    { id: "updates", label: "Updates & diagnostics" },
    ...(notif ? [{ id: "notifications" as SettingsTab, label: "Notifications" }] : []),
    { id: "mxc", label: "MXC sandbox" },
    { id: "supervision", label: "Supervision" },
  ];
  const tab = tabs.some((t) => t.id === ui.tab) ? ui.tab : "general";

  // --- General ---
  const generalRows: SettingsRow[] = [];
  const extraRows: SettingsRow[] = [];
  const dataDir = primStr(cfg.data_dir);
  if (dataDir !== null) generalRows.push({ key: "Data directory", value: dataDir, copy: dataDir });
  const configPath = primStr(cfg.config_path);
  if (configPath !== null) generalRows.push({ key: "Configuration file", value: configPath, copy: configPath });
  const tel = asRecord(cfg.telemetry);
  if (tel) {
    const intervalKey = ["poll_interval_ms", "interval_ms", "poll_ms", "interval"].find((k) => typeof tel[k] === "number");
    if (intervalKey) generalRows.push({ key: "Telemetry poll interval", value: `${(tel[intervalKey] as number).toLocaleString()} ms` });
    // Raw telemetry subkeys are detail, not summary: they move to
    // the collapsed "All configuration values" expandable below.
    for (const [k, v] of Object.entries(tel)) {
      if (k === intervalKey) continue;
      extraRows.push({ key: `Telemetry \u00b7 ${humanLabel(k)}`, value: summarizeValue(v) });
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
  const KNOWN_CFG = new Set(["data_dir", "config_path", "telemetry", "winml", "setup_completed", "updates", "app_update", "notifications", "mxc"]);
  for (const [k, v] of Object.entries(cfg)) {
    if (KNOWN_CFG.has(k)) continue;
    extraRows.push({ key: humanLabel(k), value: summarizeValue(v) });
  }
  const generalPanel = `${settingsRows(generalRows)}
    ${extraRows.length ? `<div style="margin-top:12px"><button class="btn" data-all-toggle aria-expanded="${ui.allOpen}" title="All configuration values" aria-label="All configuration values" style="padding:2px 8px;font-size:11px">${icon(ui.allOpen ? "chevD" : "chevR")}</button>
      ${ui.allOpen ? `<div style="margin-top:8px">${settingsRows(extraRows)}</div>` : ""}</div>` : ""}`;

  // --- Updates & diagnostics ---
  const updates = asRecord(cfg.updates);
  let policyBlock = "";
  if (updates) {
    const { components, ...restUpdates } = updates;
    const comps = asRecord(components);
    const compHtml = comps ? Object.entries(comps).map(([name, v]) => {
      const c = asRecord(v);
      if (!c) return `<div style="display:flex;justify-content:space-between;gap:16px;padding:4px 0"><span class="k">${esc(name)}</span><span style="font-size:12px">${esc(summarizeValue(v))}</span></div>`;
      const pinned = primStr(c.pinned_version);
      const checked = primStr(c.last_checked);
      return `<div style="display:flex;justify-content:space-between;gap:16px;padding:4px 0;border-bottom:1px solid var(--border)">
        <span class="k">${esc(name)}</span>
        <span style="font-size:12px;text-align:right">Version: <b>${pinned !== null ? `pinned ${esc(pinned)}` : "unpinned"}</b> \u00b7 Auto-update: ${c.auto_update ? "on" : "off"}${checked !== null ? ` \u00b7 Checked: ${esc(fmtDate(checked))}` : ""}</span></div>`;
    }).join("") : "";
    const restRows = humanEntries(restUpdates);
    // No policy content at all -> the policy block is omitted
    // entirely; the diagnostics actions below remain (they are the
    // tab's content, not an empty state).
    policyBlock = `${restRows.length ? settingsRows(restRows) : ""}
      ${compHtml}`;
  }
  const updatesPanel = `${appUpdateHtml}
    ${policyBlock ? `<div style="font-size:12px;color:var(--text2);margin-top:14px">Components<span class="info-tip" title="Per-component version pins the setup wizard manages (LiteLLM, llama.cpp). Separate from LLMdil's own updates above.">${icon("info")}</span></div>${policyBlock}` : ""}
    <div style="font-size:12px;color:var(--text2);margin-top:14px">Diagnostics</div>
    <div class="card-actions"><a class="btn" href="#/wizard">${icon("search")} Open Setup Wizard</a>
    <button class="btn icon-only" id="setExportBtn" title="Export diagnostics bundle" aria-label="Export diagnostics bundle">${icon("download")}</button><span class="info-tip" title="The bundle contains raw, unredacted process logs. Review it before sharing.">${icon("info")}</span>
    <button class="btn icon-only" id="setOpenFolder" title="Open folder" aria-label="Open folder" style="display:none">${icon("folder")}</button></div>`;

  // --- Notifications: master switch + one toggle per category,
  // each persisted through notifications_set_config. A missing key
  // reads as on (the Rust serde defaults agree), so a config written
  // before the category switches existed renders its true state.
  // Category toggles disable while the master is off (they keep
  // their stored value; the backend ANDs them with the master). ---
  const notifOn = (k: string): boolean => notif?.[k] !== false;
  const notifMaster = notifOn("enabled");
  const notifRow = (key: string, label: string, desc: string): string => {
    const on = notifOn(key);
    const dis = key !== "enabled" && !notifMaster;
    return `<div style="display:flex;justify-content:space-between;align-items:center;gap:16px;padding:4px 0;border-bottom:1px solid var(--border)">
      <span class="k">${esc(label)}<span class="info-tip" title="${esc(desc)}">${icon("info")}</span></span>
      ${enableSwitch({ on, name: key === "enabled" ? "Notifications" : `${label} notifications`, attrs: `data-notif-toggle="${key}"`, disabled: dis })}</div>`;
  };
  const notifPanel = notif ? `${notifRow("enabled", "Notifications", "Master switch for Windows toast notifications. Off silences every category below.")}
    ${notifRow("operations", "Operations", "Long-running work finishing: a setup step completing, a model download completing.")}
    ${notifRow("failures", "Failures", "A backend stopping unexpectedly, the gateway going down or failing to start, a setup step failing, a model download failing.")}
    ${notifRow("activation", "Activation", "An activation plan finishing applying, or failing to apply.")}
    <div class="card-actions" style="margin-top:10px"><button class="btn" data-notif-test>${icon("bell")} Send test notification</button></div>
    <div style="font-size:12px;color:var(--text2);margin-top:6px">The test fires immediately, even with the master switch off — it verifies Windows can show this app's toasts.</div>` : "";

  // --- MXC sandbox: summary first — Mode control, Active policy,
  // Recent activity, self-test. The button group is the ONE home
  // for the mode (no Mode row); each button's definition is its
  // tooltip; the methodology is the Mode label's info-tip. ---
  const mxc = asRecord(cfg.mxc);
  const mode = (mxc ? primStr(mxc.mode) : null) ?? "";
  const modeLc = mode.toLowerCase();
  const policyFile = mxc ? primStr(mxc.path) : null;
  const modeBlock = `<div style="font-size:12px;color:var(--text2)">Mode<span class="info-tip" title="The mode is saved configuration: this app records it, and the MXC sandbox runtime applies it when that runtime executes tools.">${icon("info")}</span></div>
    <div class="card-actions" style="margin-top:6px">${MXC_MODES.map((m) =>
      `<button class="btn${modeLc === m.mode ? " primary" : ""}" data-mxc-mode="${m.mode}" title="${esc(m.def)}">${m.label}</button>`).join("")}</div>`;
  const selfTest = `<div style="font-size:12px;color:var(--text2);margin-top:14px">Self-test</div>
    <div class="card-actions"><button class="btn icon-only" data-mxc-selftest title="Run self-test" aria-label="Run self-test">${icon("shield")}</button></div>
    <div id="mxcSelfTest">${selfTestHtml}</div>`;
  const mxcPanel = `${modeBlock}${mxcPolicyInner(policyR, ui.rulesFilter, ui.rulesOpen, policyFile)}${mxcActivityInner(activityR)}${selfTest}`;

  const supervisionPanel = supervisionInner(svcR);
  const panel = tab === "general" ? generalPanel
    : tab === "remote" ? remoteHtml
    : tab === "updates" ? updatesPanel
    : tab === "notifications" ? notifPanel
    : tab === "supervision" ? supervisionPanel
    : mxcPanel;
  return `<div class="card" style="margin-bottom:12px"><div class="sec-title">${icon("gear")} Settings</div>
    <div class="viewsel" role="tablist" style="margin-bottom:12px">${tabs.map((t) => `<button data-set-tab="${t.id}"${t.id === tab ? ` class="active"` : ""}>${t.label}</button>`).join("")}</div>
    ${panel}</div>`;
}


/** Remote Access tab: the admin API's state, its users, and
 *  device pairing. Values are Label: Value rows with copy buttons
 *  (the global [data-copy-value] handler serves them); state the
 *  API has not produced yet renders nothing. One-time values (a
 *  temporary password, an enrollment payload) show until the next
 *  action replaces them. */
function remoteAccessHtml(
  status: AdminApiStatus | null,
  users: AdminApiUser[],
  temp: { user: string; password: string } | null,
  enroll: (AdminApiPairing & { user: string }) | null,
  audit: AdminApiAuditEntry[] = [],
): string {
  if (!status) return loadingHtml("Loading remote access\u2026");
  const stateText = status.running
    ? `<span style="color:var(--ok)">Running</span>`
    : status.enabled
      ? `<span style="color:var(--err)">Enabled \u2014 not running</span>`
      : `<span style="color:var(--text2)">Disabled</span>`;
  const fwText = status.firewall === "ok" ? "Allowed: local subnet + Tailscale"
    : status.firewall === "missing" ? "Rule missing"
    : status.firewall === "drifted" ? "Rule differs from expected"
    : status.firewall === "unsupported" ? "Not reported on this platform"
    : "Unknown";
  const rows: SettingsRow[] = [
    { key: "Port", value: String(status.port) },
    { key: "Bind", value: status.bind },
    { key: "Firewall rule", value: fwText },
    { key: "Users", value: String(status.user_count) },
  ];
  if (status.base_url) rows.unshift({ key: "Base URL", value: status.base_url, copy: status.base_url });
  if (status.cert_sha256) rows.push({ key: "Certificate SHA-256", value: status.cert_sha256, copy: status.cert_sha256 });
  const roles = ["viewer", "operator", "configurator", "admin"];
  const userRows = users.map((u) => `<div style="display:flex;justify-content:space-between;gap:16px;align-items:center;padding:3px 0">
    <span style="font-size:12px">${esc(u.username)}${u.disabled ? ` <span style="color:var(--text2)">(disabled)</span>` : ""}${u.must_change_password ? ` <span style="color:var(--warn)">(must change password)</span>` : ""}</span>
    <span style="display:flex;gap:6px;align-items:center">
      <select data-ra-role-for="${esc(u.id)}" style="font-size:12px">${roles.map((r) => `<option value="${r}"${r === u.role ? " selected" : ""}>${r}</option>`).join("")}</select>
      <button class="btn" data-ra-apply-role="${esc(u.id)}" style="padding:1px 7px;font-size:11px">Apply</button>
      ${enableSwitch({ on: !u.disabled, name: `User ${u.username}`, attrs: `data-ra-toggle-disabled="${esc(u.id)}"` })}
      <button class="btn" data-ra-reset="${esc(u.id)}" data-ra-name="${esc(u.username)}" style="padding:1px 7px;font-size:11px">Reset password</button>
      <button class="btn" data-ra-enroll="${esc(u.id)}" data-ra-name="${esc(u.username)}" style="padding:1px 7px;font-size:11px">Pairing code</button>
      <button class="btn" data-ra-delete="${esc(u.id)}" data-ra-name="${esc(u.username)}" style="padding:1px 7px;font-size:11px">Delete</button>
    </span></div>`).join("");
  return `<div style="font-size:12px;color:var(--text2)">Admin API<span class="info-tip" title="Serves the LLMdil control API to devices on this network. Access requires a user account and is limited by the firewall rule to the local subnet and Tailscale.">${icon("info")}</span></div>
    <div style="display:flex;justify-content:space-between;gap:16px;margin:6px 0"><span class="k">Status</span><span class="v" style="font-size:12px">${stateText}</span></div>
    ${settingsRows(rows)}
    ${status.note ? `<div style="font-size:12px;color:var(--err);margin-top:6px">${esc(status.note)}</div>` : ""}
    <div class="card-actions" style="margin-top:10px">${enableSwitch({ on: status.enabled, name: "Admin API", attrs: "data-ra-toggle" })}
    <button class="btn" data-ra-regen>Regenerate certificate</button></div>
    <div style="font-size:12px;color:var(--text2);margin-top:16px">${users.length === 0 ? "First administrator" : "Users"}<span class="info-tip" title="Roles: viewer reads status; operator runs backends and the gateway; configurator also edits configuration; admin manages users and the API itself. Passwords need at least 12 characters.">${icon("info")}</span></div>
    ${userRows}
    <div style="display:flex;gap:6px;align-items:center;margin-top:8px;flex-wrap:wrap">
      <input id="raUser" type="text" placeholder="Username" style="font-size:12px" />
      <input id="raPass" type="password" placeholder="Password (12+ characters)" style="font-size:12px" />
      <select id="raRole" style="font-size:12px">${roles.map((r) => `<option value="${r}"${r === "admin" && users.length === 0 ? " selected" : ""}>${r}</option>`).join("")}</select>
      <button class="btn" data-ra-create style="padding:1px 7px;font-size:11px">Create user</button>
    </div>
    ${audit.length ? `<div style="font-size:12px;color:var(--text2);margin-top:16px">Audit log<span class="info-tip" title="Every API mutation and sign-in event, newest first. Full log: logs/admin-api-audit.jsonl in the data directory.">${icon("info")}</span></div>
    <div style="font-size:12px">${audit.map((e) => `<div style="display:flex;justify-content:space-between;gap:16px;padding:2px 0"><span>${esc(new Date(e.ts_ms).toLocaleString())} \u00b7 ${esc(e.actor)} \u00b7 ${esc(e.action)}${e.target ? ` \u00b7 ${esc(e.target)}` : ""}</span><span style="color:${e.result.startsWith("2") ? "var(--ok)" : "var(--err)"}">${esc(e.result)}</span></div>`).join("")}</div>` : ""}
    ${temp ? `<div style="margin-top:12px">${settingsRows([{ key: `Temporary password \u00b7 ${temp.user}`, value: temp.password, copy: temp.password, tip: "Shown once. The user must change it at first sign-in." }])}</div>` : ""}
    ${enroll ? `<div style="margin-top:12px"><div style="font-size:12px;color:var(--text2)">Pairing \u00b7 ${esc(enroll.user)}<span class="info-tip" title="Enter these values in the LLMdil app to pair this device. The code is single-use and expires in 10 minutes.">${icon("info")}</span></div>${settingsRows([
      { key: "Base URL", value: enroll.base_url, copy: enroll.base_url },
      { key: "Certificate SHA-256", value: enroll.cert_sha256 ?? "", copy: enroll.cert_sha256 ?? undefined },
      { key: "Enrollment code", value: enroll.enrollment_code ?? "", copy: enroll.enrollment_code ?? undefined },
    ])}</div>` : ""}`;
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
  /** Tab + MXC rules-filter/expandable state, held outside the
   *  repainted DOM. */
  const setUi: SettingsUi = { tab: "general", rulesFilter: "fs", rulesOpen: new Set<string>(), allOpen: false };

  let lastExportPath = "";
  /** Last loaded notifications object, so a toggle click can send
   *  the complete replacement (the command replaces the section). */
  let lastNotif: Record<string, unknown> | null = null;
  // Remote Access tab state, held outside the repainted DOM.
  let raStatus: AdminApiStatus | null = null;
  let raUsers: AdminApiUser[] = [];
  let raTemp: { user: string; password: string } | null = null;
  let raEnroll: (AdminApiPairing & { user: string }) | null = null;
  let raAudit: AdminApiAuditEntry[] = [];
  const loadRemote = async (): Promise<void> => {
    const [st, us, au] = await Promise.all([adminApiGetStatus(), adminApiListUsers(), adminApiGetAudit(20)]);
    if (disposed) return;
    raStatus = st.data;
    raUsers = us.data;
    raAudit = au.data;
  };
  const setMsg = (text: string, isErr = false): void => {
    const m = root.querySelector("#setMsg");
    if (m) m.innerHTML = text ? `<span style="color:${isErr ? "var(--err)" : "var(--text2)"}">${esc(text)}</span>` : "";
  };

  /** Last app-update status payload, for the Updates tab block and
   *  its toggle handlers (the switch sends the negation of the
   *  rendered state). */
  let lastAppUpd: AppUpdateInfo | null = null;
  let appUpdLive = false;
  const load = async (): Promise<void> => {
    const [cfgR, polR, actR, svcR, updR] = await Promise.all([getAppConfig(), getMxcPolicy(), getMxcActivity(), servicesList(), getAppUpdateStatus()]);
    if (disposed) return;
    lastAppUpd = updR.error ? lastAppUpd : updR.data;
    appUpdLive = updR.live;
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
    lastNotif = asRecord(cfgR.data.notifications);
    // Policy/activity CallResults go in whole: their .error renders as
    // "unavailable — <error>", distinct from genuine absence.
    if (setUi.tab === "remote") await loadRemote();
    if (disposed) return;
    b.innerHTML = settingsPaint(cfgR.data, polR, actR, selfTestHtml, setUi, svcR,
      remoteAccessHtml(raStatus, raUsers, raTemp, raEnroll, raAudit),
      appUpdateInner(lastAppUpd, appUpdLive));
  };

  const onClick = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLElement;

    const tabBtn = t.closest("[data-set-tab]") as HTMLElement | null;
    if (tabBtn) {
      setUi.tab = (tabBtn.getAttribute("data-set-tab") ?? "general") as SettingsTab;
      await load();
      return;
    }

    const filterBtn = t.closest("[data-rules-filter]") as HTMLElement | null;
    if (filterBtn) {
      setUi.rulesFilter = (filterBtn.getAttribute("data-rules-filter") ?? "fs") as SettingsUi["rulesFilter"];
      await load();
      return;
    }

    const rulesToggle = t.closest("[data-rules-toggle]") as HTMLElement | null;
    if (rulesToggle) {
      const id = rulesToggle.getAttribute("data-rules-toggle") ?? "";
      if (setUi.rulesOpen.has(id)) setUi.rulesOpen.delete(id); else setUi.rulesOpen.add(id);
      await load();
      return;
    }

    if (t.closest("[data-all-toggle]")) {
      setUi.allOpen = !setUi.allOpen;
      await load();
      return;
    }


    const raRegen = t.closest("[data-ra-regen]") as HTMLButtonElement | null;
    if (raRegen) {
      if (!confirm("Regenerate the API certificate? The fingerprint changes and every paired device must pair again.")) return;
      raRegen.disabled = true;
      const r = await adminApiRegenerateCert();
      if (disposed) return;
      if (r.error) setMsg(`Certificate regeneration failed: ${r.error}`, true);
      else setMsg("Certificate regenerated. Paired devices must pair again.");
      await load();
      return;
    }

    const raToggle = t.closest("[data-ra-toggle]") as HTMLButtonElement | null;
    if (raToggle && raStatus) {
      raToggle.disabled = true;
      const r = await adminApiSetEnabled(!raStatus.enabled);
      if (disposed) return;
      if (r.error) setMsg(`Admin API change failed: ${r.error}`, true);
      await load();
      return;
    }

    if (t.closest("[data-ra-create]")) {
      const nameEl = root.querySelector("#raUser") as HTMLInputElement | null;
      const passEl = root.querySelector("#raPass") as HTMLInputElement | null;
      const roleEl = root.querySelector("#raRole") as HTMLSelectElement | null;
      const r = await adminApiCreateUser(nameEl?.value ?? "", passEl?.value ?? "", roleEl?.value ?? "viewer");
      if (disposed) return;
      if (r.error) setMsg(`Create user failed: ${r.error}`, true);
      else setMsg(`User ${r.data?.username ?? ""} created.`);
      await load();
      return;
    }

    const raApplyRole = t.closest("[data-ra-apply-role]") as HTMLElement | null;
    if (raApplyRole) {
      const id = raApplyRole.getAttribute("data-ra-apply-role") ?? "";
      const sel = root.querySelector(`[data-ra-role-for="${id}"]`) as HTMLSelectElement | null;
      const r = await adminApiUpdateUser(id, sel?.value);
      if (disposed) return;
      if (r.error) setMsg(`Role change failed: ${r.error}`, true);
      await load();
      return;
    }

    const raDis = t.closest("[data-ra-toggle-disabled]") as HTMLElement | null;
    if (raDis) {
      const id = raDis.getAttribute("data-ra-toggle-disabled") ?? "";
      const u = raUsers.find((x) => x.id === id);
      const r = await adminApiUpdateUser(id, undefined, !(u?.disabled ?? false));
      if (disposed) return;
      if (r.error) setMsg(`User change failed: ${r.error}`, true);
      await load();
      return;
    }

    const raReset = t.closest("[data-ra-reset]") as HTMLElement | null;
    if (raReset) {
      const id = raReset.getAttribute("data-ra-reset") ?? "";
      const name = raReset.getAttribute("data-ra-name") ?? "";
      const r = await adminApiResetPassword(id);
      if (disposed) return;
      if (r.error) setMsg(`Password reset failed: ${r.error}`, true);
      else if (r.data) raTemp = { user: name, password: r.data };
      await load();
      return;
    }

    const raEnrollBtn = t.closest("[data-ra-enroll]") as HTMLElement | null;
    if (raEnrollBtn) {
      const id = raEnrollBtn.getAttribute("data-ra-enroll") ?? "";
      const name = raEnrollBtn.getAttribute("data-ra-name") ?? "";
      const r = await adminApiCreateEnrollment(id);
      if (disposed) return;
      if (r.error) setMsg(`Pairing code failed: ${r.error}`, true);
      else if (r.data) raEnroll = { ...r.data, user: name };
      await load();
      return;
    }

    const raDelete = t.closest("[data-ra-delete]") as HTMLElement | null;
    if (raDelete) {
      const id = raDelete.getAttribute("data-ra-delete") ?? "";
      const name = raDelete.getAttribute("data-ra-name") ?? "";
      if (!confirm(`Delete user ${name}? Their sessions and API tokens are revoked.`)) return;
      const r = await adminApiDeleteUser(id);
      if (disposed) return;
      if (r.error) setMsg(`Delete failed: ${r.error}`, true);
      await load();
      return;
    }

    const copyBtn = t.closest("[data-copy-value]") as HTMLElement | null;
    if (copyBtn) {
      const value = copyBtn.getAttribute("data-copy-value") ?? "";
      try {
        await navigator.clipboard.writeText(value);
        setMsg("Copied to the clipboard.");
      } catch {
        setMsg("Copy failed — copy the value manually.", true);
      }
      return;
    }

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
          <div>Self-test: ${s.passed}/${s.total} probes passed${s.failed ? ` \u2014 ${s.failed} failed` : ""}.${failed.length ? `<span class="info-tip" title="To fix a failing case, edit the policy file at the Policy file path shown above and restart the app to apply it.">${icon("info")}</span>` : ""}</div>
          ${failed.map((c) => `<div style="color:var(--err)">${esc(c.name)}: expected ${esc(c.expected)}, got ${esc(c.actual)} \u2014 ${esc(c.description)}</div>`).join("")}
        </div>`;
      }
      await load();
      return;
    }

    const appUpdBtn = t.closest("[data-appupd-toggle]") as HTMLButtonElement | null;
    if (appUpdBtn) {
      const key = appUpdBtn.getAttribute("data-appupd-toggle") ?? "";
      if (!lastAppUpd || !["checking", "auto"].includes(key)) return;
      appUpdBtn.disabled = true;
      const r = key === "checking"
        ? await appUpdateSetChecking(!lastAppUpd.check_enabled)
        : await appUpdateSetAuto(!lastAppUpd.auto_update);
      if (disposed) return;
      if (r.error) setMsg(`Update settings not saved: ${r.error}`, true);
      else setMsg("Update settings saved.");
      await load();
      return;
    }

    if (t.closest("[data-appupd-check]")) {
      const btn = t.closest("[data-appupd-check]") as HTMLButtonElement;
      btn.disabled = true;
      setMsg("Checking GitHub Releases…");
      const r = await appUpdateCheck();
      if (disposed) return;
      if (r.error) setMsg(`Update check failed: ${r.error}`, true);
      else if (r.data.last_check_outcome === "failed") setMsg(`Update check failed: ${r.data.last_check_error ?? "unknown error"}`, true);
      else if (r.data.available) setMsg(`LLMdil ${r.data.available.version} is available.`);
      else setMsg("LLMdil is up to date.");
      await load();
      return;
    }

    if (t.closest("[data-appupd-download]")) {
      const btn = t.closest("[data-appupd-download]") as HTMLButtonElement;
      btn.disabled = true;
      setMsg("Downloading the update…");
      const r = await appUpdateDownload();
      if (disposed) return;
      if (r.error) setMsg(`Download failed: ${r.error}`, true);
      else if (r.data.staged) setMsg(`LLMdil ${r.data.staged.version} downloaded and verified — restart from the banner to apply it.`);
      else if (r.data.last_stage_error) setMsg(`Download failed: ${r.data.last_stage_error}`, true);
      await load();
      return;
    }

    const notifBtn = t.closest("[data-notif-toggle]") as HTMLButtonElement | null;
    if (notifBtn) {
      const key = notifBtn.getAttribute("data-notif-toggle") ?? "";
      if (!lastNotif || !["enabled", "operations", "failures", "activation"].includes(key)) return;
      // Missing keys read as on (the serde defaults agree).
      const next = {
        enabled: lastNotif.enabled !== false,
        operations: lastNotif.operations !== false,
        failures: lastNotif.failures !== false,
        activation: lastNotif.activation !== false,
      };
      const k = key as keyof typeof next;
      next[k] = !next[k];
      notifBtn.disabled = true;
      const r = await setNotificationConfig(next);
      if (disposed) return;
      if (r.error) setMsg(`Notification settings not saved: ${r.error}`, true);
      else setMsg("Notification settings saved.");
      await load();
      return;
    }

    const supBtn = t.closest("[data-sup-mode]") as HTMLButtonElement | null;
    if (supBtn) {
      const m = supBtn.getAttribute("data-sup-mode") ?? "process";
      supBtn.disabled = true;
      const r = await setSupervisionMode(m);
      if (disposed) return;
      if (r.error) {
        setMsg(`Supervision mode change failed: ${r.error}`, true);
        supBtn.disabled = false;
        return;
      }
      setMsg(`Supervision mode set to ${m}.`);
      await load();
      return;
    }

    const testBtn = t.closest("[data-notif-test]") as HTMLButtonElement | null;
    if (testBtn) {
      testBtn.disabled = true;
      const r = await sendTestNotification();
      testBtn.disabled = false;
      if (disposed) return;
      if (r.error) setMsg(`Test notification failed: ${r.error}`, true);
      else setMsg("Test notification sent. If no toast appeared, check Windows notification settings for this app.");
      return;
    }

    const regBtn = t.closest("[data-svc-register]") as HTMLButtonElement | null;
    if (regBtn) {
      const target = regBtn.getAttribute("data-svc-register") ?? "";
      const pwd = (root.querySelector("#svcPwd") as HTMLInputElement | null)?.value ?? "";
      if (!pwd) {
        setMsg("Enter the account password before registering.", true);
        return;
      }
      regBtn.disabled = true;
      setMsg(`Registering ${target} — approve the elevation prompt…`);
      const r = await servicesRegister(target, pwd);
      if (disposed) return;
      if (r.error) {
        setMsg(`Registration of ${target} failed: ${r.error}`, true);
        regBtn.disabled = false;
        return;
      }
      setMsg(`Registered ${r.data.service_name} as ${r.data.account}.`);
      await load();
      return;
    }

    const unregBtn = t.closest("[data-svc-unregister]") as HTMLButtonElement | null;
    if (unregBtn) {
      const target = unregBtn.getAttribute("data-svc-unregister") ?? "";
      unregBtn.disabled = true;
      setMsg(`Unregistering ${target} — approve the elevation prompt…`);
      const r = await servicesUnregister(target);
      if (disposed) return;
      if (r.error) {
        setMsg(`Unregister of ${target} failed: ${r.error}`, true);
        unregBtn.disabled = false;
        return;
      }
      setMsg(`Unregistered ${r.data.service_name}; it returns to process supervision.`);
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

function wizardStepRow(s: WizardStepState, live: boolean, open: boolean): string {
  // Running is a transitional state — amber, per principle 11 (the
  // segbar's running segment and .step.running match in styles.css).
  const stIcon = s.state === "done" || s.state === "manual" ? `<span style="color:var(--ok)">${icon("check")}</span>`
    : s.state === "running" ? `<span style="color:var(--warn)">${icon("clock")}</span>`
    : s.state === "failed" ? `<span style="color:var(--err)">${icon("err")}</span>`
    : `<span style="color:var(--text3)">\u25cb</span>`;
  const stateTxt = s.state === "done" ? `done \u00b7 ${esc(s.duration ?? "")}`
    : s.state === "manual" ? "manual"
    : s.state === "running" ? esc(s.note ?? "running\u2026")
    : esc(s.state);
  const helpBtn = s.state === "failed" && s.helpUrl
    ? `<button class="btn icon-only" data-help-url="${esc(s.helpUrl)}" title="Open driver download" aria-label="Open driver download">${icon("external")}</button>` : "";
  const runBtn = live && (s.state === "queued" || s.state === "failed")
    ? `<button class="btn" data-run-step="${esc(s.id)}">${s.state === "failed" ? `${icon("restart")} Retry` : `${icon("play")} Run`}</button>` : "";
  const actions = helpBtn || runBtn ? `<div style="margin-top:6px;display:flex;gap:6px">${helpBtn}${runBtn}</div>` : "";
  // The outcome's own manual block wins over the generic planned
  // commands: it is built for this install and this failure (real
  // paths, the PostgreSQL pre-step when installed, the secret env
  // named), and its first line already says what it is.
  const manualText = s.state === "failed" ? (s.manualCommand ?? s.manualCommands) : undefined;
  const hasDetails = !!(s.detail || (s.note && s.state !== "running") || (s.state === "failed" && (s.remediation || (s.logTail && s.logTail.length) || manualText)));
  /* Details expandable: ALL narrative lives here, as Label: Value
   * lines — Outcome (the step message), Note, Fix (remediation,
   * with the help URL stripped from the sentence when the Open
   * button already carries it), the log tail, and the manual
   * commands with their Copy button beside the label. */
  let details = "";
  if (open && hasDetails) {
    const fixText = s.state === "failed" && s.remediation
      ? (s.helpUrl ? s.remediation.split(s.helpUrl).join("").replace(/\s*[—–-]\s*download:\s*$/, "").replace(/download:\s*$/, "").trim() : s.remediation)
      : "";
    details = `<div class="sdetails">
      ${s.detail ? `<div><span class="k">Outcome:</span> ${esc(s.detail)}</div>` : ""}
      ${s.note && s.state !== "running" ? `<div><span class="k">Note:</span> ${esc(s.note)}</div>` : ""}
      ${fixText ? `<div><span class="k">Fix:</span> ${esc(fixText)}</div>` : ""}
      ${s.state === "failed" && s.logTail && s.logTail.length ? `<div><span class="k">Log (last 3 lines):</span> <a href="#/logs?scope=wizard" style="font-size:11px">Open in Logs view</a><div style="margin-top:2px;font-family:var(--mono);font-size:11px;color:var(--text3)">${s.logTail.slice(-3).map((l) => esc(l)).join("<br>")}</div></div>` : ""}
      ${manualText ? `<div><div style="display:flex;gap:8px;align-items:center">${s.manualCommand ? `<span></span>` : `<span class="k">Manual commands (PowerShell):</span>`}<span style="flex:1"></span><button class="btn" data-copy-step="${esc(s.id)}" title="Copy commands" aria-label="Copy commands" style="padding:2px 8px;font-size:11px">${icon("copy")}</button></div>
        <pre style="margin:4px 0;font-family:var(--mono);font-size:11px;white-space:pre-wrap;word-break:break-all;background:var(--panel);border:1px solid var(--border);border-radius:6px;padding:8px">${esc(manualText)}</pre></div>` : ""}
    </div>`;
  }
  // A done step with a note signals it with an amber info icon in
  // the row — the note text itself lives in the expandable.
  const noteIcon = s.note && s.state === "done" ? `<span class="info-tip" style="color:var(--warn)" title="${esc(s.note)}">${icon("info")}</span> ` : "";
  const chevron = hasDetails
    ? `<button class="btn" data-wiz-details="${esc(s.id)}" aria-expanded="${open}" title="Details" aria-label="Details" style="padding:2px 8px;font-size:11px">${icon(open ? "chevD" : "chevR")}</button>`
    : "<span></span>";
  return `<div class="step ${s.state}">${stIcon}
    <div><div class="sname">${esc(s.name)}</div>${actions}</div>
    <div class="sstate">${noteIcon}${stateTxt}</div>${chevron}${details}</div>`;
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
  /** Steps whose Details expandable the operator opened, and steps
   *  whose auto-open (blocked step) the operator dismissed. */
  expandedSteps?: Set<string>;
  suppressAutoOpen?: Set<string>;
}

function paintWizard(root: HTMLElement, steps: WizardStepState[], live: boolean, opts: WizardPaintOpts = {}): void {
  // The backend's Running state cannot be observed mid-step (the run
  // command holds the wizard lock for the whole execution, and the
  // state query needs the same lock), so the view marks the step it
  // invoked as running itself. While a step runs, its box must not
  // keep showing the previous session's result: it renders blank,
  // labeled "running", in the amber running style — whatever its prior
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
  const progress = root.querySelector("#wizProgress");
  if (progress) {
    // Progress is stated once as the done count and once as the
    // segmented bar — no percentage text (numbers in one place).
    progress.innerHTML = `<b>${done}</b><span style="color:var(--text3);font-size:12px">of ${eff.length} steps</span>
      <div class="segbar">${eff.map((s) => `<i class="${s.state === "done" || s.state === "manual" ? "done" : s.state === "running" ? "run" : ""}"></i>`).join("")}</div>`
      + (opts.statusLine ? `<div style="margin-top:4px;font-size:12px;color:var(--text2)">${esc(opts.statusLine)}</div>` : "");
  }
  // The step a run is blocked on (the first failed step, when no
  // run is in flight) auto-opens its Details — unless the operator
  // dismissed it.
  const blockedId = !opts.busy ? (eff.find((s) => s.state === "failed")?.id ?? null) : null;
  const stepOpen = (s: WizardStepState): boolean =>
    (opts.expandedSteps?.has(s.id) ?? false)
    || (s.id === blockedId && !(opts.suppressAutoOpen?.has(s.id) ?? false));
  const list = root.querySelector("#wizSteps");
  if (list) {
    list.innerHTML = `<div class="sec-title">Setup checklist <span style="color:var(--text3);font-weight:400">\u2014 runs in order</span></div>`
      + eff.map((s) => wizardStepRow(s, live, stepOpen(s))).join("");
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
      <button class="btn icon-only" id="auditBtn" title="Re-run audit" aria-label="Re-run audit">${icon("search")}</button>
      <button class="btn primary" id="runBtn">${icon("play")} Run setup</button></div>
    <div class="wiz-progress" id="wizProgress"></div>
    <div class="wiz-body">
      <div class="wiz-side"><p>Steps run in order. A failed step pauses the run \u2014 retry it to continue.</p>
        <div style="margin-top:14px;display:grid;gap:6px">
          <div>${icon("shield")} checksums verified per file</div>
          <div>${icon("server")} the gateway starts at its step; backends start before the smoke test</div></div></div>
      <div class="wiz-steps" id="wizSteps"></div>
    </div>
    <div class="wiz-foot"><button class="btn icon-only" title="Back" aria-label="Back" disabled>${icon("chevL")}</button><span class="spacer"></span>
      <button class="btn primary" id="finishBtn">${icon("check")} Finish</button></div>
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
  /** Details-expandable state per step: opened by the operator, and
   *  auto-opens dismissed by the operator. */
  const wizExpanded = new Set<string>();
  const wizAutoSuppressed = new Set<string>();

  const paint = (): void => paintWizard(root, steps, liveNow, { busy, runningStepId, statusLine, hideStale, freshIds, expandedSteps: wizExpanded, suppressAutoOpen: wizAutoSuppressed });

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
    const detBtn = t.closest("[data-wiz-details]") as HTMLElement | null;
    if (detBtn) {
      const id = detBtn.getAttribute("data-wiz-details") ?? "";
      const isOpen = detBtn.getAttribute("aria-expanded") === "true";
      if (isOpen) {
        // Closing: if it was open only via auto-open, suppress that;
        // if the operator opened it, just close it.
        if (wizExpanded.has(id)) wizExpanded.delete(id);
        else wizAutoSuppressed.add(id);
      } else {
        wizExpanded.add(id);
        wizAutoSuppressed.delete(id);
      }
      paint();
      return;
    }
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
