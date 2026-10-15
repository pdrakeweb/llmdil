/* Secondary views — all wired to the backend through api.ts.
   Each view follows the Setup wizard pattern: renderX() returns a shell
   with a loading placeholder; wireX(root) loads live data, paints it, and
   attaches handlers. Under Tauri the data is real; in a plain browser
   (live=false) views show a sample-mode banner and honest empty states
   instead of fabricated data. */

import { gaugeRing, sparkline } from "../charts";
import { icon } from "../icons";
import { esc } from "../html";
import {
  runWizardStep, isTauri, openUrl, markSetupComplete, getWizardSteps, runAudit,
  getGpuTelemetry, setTelemetryInterval, resetTelemetryPeaks, saveTelemetrySnapshot,
  getBackends, startBackend, stopBackend, restartBackend, getBackendLaunchCommand,
  getBackendConfig, applyBackendConfig, testBackend, placementPropose, placementApply,
  modelAssign,
  getModels, downloadModel, cancelDownload, pauseModelDownload, resumeModelDownload,
  verifyModel, deleteModel,
  getLogs, exportDiagnostics, getGatewayConfig, getGatewayStatus, getGatewayYaml,
  updateGatewayConfig, effectiveGatewayStrategy, gatewayStop,
  getAppConfig, getMxcPolicy, getMxcActivity, setMxcMode, mxcRunSelfTest,
} from "../api";
import type { BackendRow, GatewayConfigData, GatewayStatusData, LogRow, ModelRow, MxcActivityData, MxcPolicyData, PlacementProjectionData } from "../api";
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
  return `${v >= 100 ? v.toFixed(0) : v.toFixed(1)} ${units[u]}`;
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

function gpuCard(g: GpuInfo): string {
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
  const memUtil = g.mem_util_pct !== undefined
    ? `<div class="mrow"><span class="mlabel">Memory util</span><span class="mval">${g.mem_util_pct}%</span></div>` : "";
  const fan = g.fan_pct !== undefined
    ? `<div class="mrow"><span class="mlabel">Fan</span><span class="mval">${g.fan_pct}%</span></div>` : "";
  const tempSpark = g.temp_spark && g.temp_spark.length
    ? `<div class="mrow"><span></span>${sparkline(g.temp_spark, 240, 26, C.amber)}</div>` : "";
  const powerSpark = g.power_spark && g.power_spark.length
    ? `<div class="mrow"><span></span>${sparkline(g.power_spark, 240, 26, C.green)}</div>` : "";
  const procs = g.processes && g.processes.length
    ? `<table class="tbl" style="margin-top:10px"><thead><tr><th>Process</th><th>PID</th><th>VRAM</th><th>Backend</th></tr></thead>
      <tbody>${g.processes.map((p) => `<tr><td>${esc(p.process)}</td><td>${p.pid}</td><td>${p.vram_mib} MiB</td><td>${esc(p.backend)}</td></tr>`).join("")}</tbody></table>` : "";
  const models = g.models && g.models.length
    ? `<div style="margin-top:8px;font-size:11px;color:var(--text3);display:grid;gap:4px">
        ${g.models.map((m) => `<div>${icon("layers")} ${esc(m.model)} \u00b7 ${esc(m.layers)}</div>`).join("")}
      </div>` : "";
  return `<div class="card">
      <div class="gpu-head">${dot(g.status)} ${esc(g.name)}${pci}${staleChip}
        <span class="spacer" style="flex:1"></span>${driver}</div>
      <div style="display:flex;gap:18px;justify-content:center;margin:12px 0">
        ${gaugeRing(g.util_pct, 84, C.blue, String(g.util_pct), "UTIL")}
        ${gaugeRing(vramPct, 84, C.purple, g.vram_used_gb.toFixed(1), "GiB VRAM")}
      </div>
      <div class="gpu-metrics">
        <div class="mrow"><span class="mlabel">UTILIZATION</span><span class="mval">${g.util_pct}%</span></div>
        <div class="mrow"><span class="mlabel">VRAM</span><span class="mval">${g.vram_used_gb.toFixed(1)} / ${g.vram_total_gb.toFixed(1)} GiB</span></div>
        <div class="mrow"><span class="mlabel">Temperature</span><span class="mval">${g.temp_c} \u00b0C${peak}</span></div>
        ${tempSpark}
        <div class="mrow"><span class="mlabel">Power</span><span class="mval">${g.power_w} W${limit}</span></div>
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
  <div id="gpuBody">${loadingHtml("Loading GPU telemetry\u2026")}</div>
  <div id="gpuPlacement" style="margin-top:12px"></div></div>`;
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

export async function wireGpus(root: HTMLElement): Promise<() => void> {
  let disposed = false;
  let loading = false;
  /** Latest live GPU list (index order as reported), for placement math. */
  let lastGpus: GpuInfo[] = [];
  /** Age anchor for the placement snapshot label when the projection
   *  itself carries no snapshot_age_ms. */
  let lastAtMs: number | null = null;

  const msg = (text: string, isErr = false): void => {
    const m = root.querySelector("#gpuMsg");
    if (m) m.innerHTML = text ? `<span style="color:${isErr ? "var(--err)" : "var(--text2)"}">${esc(text)}</span>` : "";
  };

  const load = async (): Promise<void> => {
    if (loading) return;
    loading = true;
    try {
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
      b.innerHTML = `<div class="cards3">${r.data.gpus.map(gpuCard).join("")}</div>`;
    } finally {
      loading = false;
    }
  };

  /* ---------------- Placement ----------------
     Lives in its own container: telemetry repaints never touch it, so
     the poll cannot clobber the form or a rendered projection.
     The backend<->model pairing is stated by the operator (two
     selects), never inferred from file names: every configured
     backend and every configured model is selectable, and Apply
     aligns the pair through model_assign when the backend does not
     already serve the chosen model. */
  const placeEl = root.querySelector("#gpuPlacement");
  let placeBackendRows: BackendRow[] = [];
  let placeModelRows: ModelRow[] = [];
  let projection: { backendId: string; modelId: string; data: PlacementProjectionData } | null = null;
  let placeBusy = false;

  /** Does the backend's model_file already resolve to the model's
   *  gguf_path? model_assign writes the gguf_path verbatim, so an
   *  exact match is the assigned state; a basename match covers
   *  configs written with a different directory prefix. */
  const sameModelFile = (modelFile: string, ggufPath: string): boolean =>
    modelFile !== "" && (modelFile === ggufPath || baseName(modelFile) === baseName(ggufPath));

  const placeMsg = (text: string, isErr = false): void => {
    const m = root.querySelector("#placeMsg");
    if (m) m.innerHTML = text ? `<span style="color:${isErr ? "var(--err)" : "var(--text2)"}">${esc(text)}</span>` : "";
  };

  const paintProjection = (): void => {
    const res = root.querySelector("#placeResult");
    if (!res) return;
    if (!projection) { res.innerHTML = ""; return; }
    const p = projection.data;
    const rows = p.split.map((frac, i) => {
      const gpuIdx = p.gpu_indices?.[i] ?? i;
      const gpu = lastGpus.find((g) => Number(g.id) === gpuIdx) ?? lastGpus[i];
      // Free VRAM comes from the server's projection (the same
      // snapshot the fit verdict used) when it carries it; the view's
      // own latest telemetry is only a fallback basis.
      const serverFree = p.free_mib?.[i];
      const freeMib = serverFree !== undefined
        ? serverFree
        : gpu ? Math.max(0, Math.round((gpu.vram_total_gb - gpu.vram_used_gb) * 1024)) : null;
      const fits = p.per_gpu_fits[i];
      return `<tr><td>${esc(gpu ? gpu.name : `GPU ${gpuIdx}`)}</td>
        <td>${Math.round(frac * 100)}%</td>
        <td>${p.per_gpu_mib[i]} MiB</td>
        <td>${freeMib !== null ? `${freeMib} MiB` : "\u2014"}</td>
        <td>${fits ? `<span style="color:var(--ok)">fits</span>` : `<span style="color:var(--err)">does not fit</span>`}</td></tr>`;
    }).join("");
    const ageMs = p.snapshot_age_ms ?? (lastAtMs !== null ? Math.max(0, Date.now() - lastAtMs) : null);
    const agePart = ageMs !== null
      ? `Based on telemetry from ${Math.round(ageMs / 1000)}s ago \u00b7 fractions map to GPUs in nvidia-smi index order.`
      : "Based on the latest telemetry snapshot \u00b7 fractions map to GPUs in nvidia-smi index order.";
    res.innerHTML = `<div style="margin-top:10px">
      <div style="font-size:12px;color:var(--text2)">${p.fits
        ? "The proposed split fits the free VRAM on every GPU."
        : "The proposed split does not fit \u2014 at least one GPU lacks the free VRAM."}</div>
      <table class="tbl" style="margin-top:8px"><thead><tr><th>GPU</th><th>Fraction</th><th>Required</th><th>Free</th><th>Fit</th></tr></thead>
      <tbody>${rows}</tbody></table>
      <div style="font-size:12px;color:var(--text2);margin-top:8px">Weights only \u2014 excludes KV cache and runtime overhead; a split that fits here can still fail at load.</div>
      <div style="font-size:12px;color:var(--text3);margin-top:4px">${esc(agePart)}</div>
    </div>`;
  };

  const paintPlacement = (): void => {
    if (!placeEl) return;
    if (!placeBackendRows.length) {
      placeEl.innerHTML = `<div class="card"><div class="sec-title">${icon("layers")} Placement</div>
        <div style="font-size:12px;color:var(--text2)">No configured backends. Placement assigns a model to a backend and applies a tensor split to it.</div></div>`;
      return;
    }
    // Every configured backend is selectable; the label shows the
    // model it currently serves (the basename of its model_file).
    const backOpts = placeBackendRows.map((b) =>
      `<option value="${esc(b.id)}">${esc(b.id)} \u2014 ${esc(b.model_file ? baseName(b.model_file) : "no model assigned")}</option>`).join("");
    const modelOpts = placeModelRows.map((mdl) =>
      `<option value="${esc(mdl.id)}">${esc(mdl.id)} \u2014 ${esc(baseName(mdl.gguf_path))}</option>`).join("");
    placeEl.innerHTML = `<div class="card"><div class="sec-title">${icon("layers")} Placement</div>
      <div style="display:flex;gap:10px;align-items:center;flex-wrap:wrap">
        <label style="font-size:12px;color:var(--text2);display:inline-flex;align-items:center;gap:6px">Backend
          <select id="placeBackend" style="${SELECT_STYLE}">${backOpts}</select></label>
        <label style="font-size:12px;color:var(--text2);display:inline-flex;align-items:center;gap:6px">Model
          <select id="placeModel" style="${SELECT_STYLE}">${modelOpts}</select></label>
        <button class="btn" id="placePropose">Propose split</button>
        <button class="btn" id="placeApply" disabled>Apply split</button>
      </div>
      ${placeModelRows.length ? "" : `<div style="font-size:12px;color:var(--text2);margin-top:8px">No models are configured \u2014 a model is needed before a split can be proposed or applied.</div>`}
      <div id="placeMsg" style="font-size:12px;margin-top:8px"></div>
      <div id="placeResult"></div>
    </div>`;
    const sel = placeEl.querySelector("#placeBackend") as HTMLSelectElement | null;
    const modelSel = placeEl.querySelector("#placeModel") as HTMLSelectElement | null;
    const proposeBtn = placeEl.querySelector("#placePropose") as HTMLButtonElement | null;
    const applyBtn = placeEl.querySelector("#placeApply") as HTMLButtonElement | null;
    sel?.addEventListener("change", onPlaceSelectionChange);
    modelSel?.addEventListener("change", onPlaceSelectionChange);
    proposeBtn?.addEventListener("click", onPropose);
    applyBtn?.addEventListener("click", onApply);
  };

  const onPlaceSelectionChange = (): void => {
    projection = null;
    placeMsg("");
    paintProjection();
    const applyBtn = root.querySelector("#placeApply") as HTMLButtonElement | null;
    if (applyBtn) applyBtn.disabled = true;
  };

  const onPropose = async (): Promise<void> => {
    const sel = root.querySelector("#placeBackend") as HTMLSelectElement | null;
    const modelSel = root.querySelector("#placeModel") as HTMLSelectElement | null;
    const backendId = sel?.value ?? "";
    const modelId = modelSel?.value ?? "";
    if (!backendId) { placeMsg("Select a backend first.", true); return; }
    if (!modelId) { placeMsg("Select a model first.", true); return; }
    if (placeBusy) return;
    placeBusy = true;
    const proposeBtn = root.querySelector("#placePropose") as HTMLButtonElement | null;
    if (proposeBtn) proposeBtn.disabled = true;
    try {
      // Propose the backend's configured split when it has one; a fresh
      // even split across the reported GPUs otherwise.
      let split: number[] = [];
      const cfgR = await getBackendConfig(backendId);
      const flags = cfgR.data?.flags as { tensor_split?: unknown } | undefined;
      if (Array.isArray(flags?.tensor_split)
        && flags.tensor_split.length > 0
        && flags.tensor_split.every((x) => typeof x === "number" && Number.isFinite(x))) {
        split = flags.tensor_split as number[];
      }
      if (!split.length) {
        const n = lastGpus.length;
        if (!n) { placeMsg("No GPU telemetry yet \u2014 a split cannot be proposed until telemetry reports the GPUs.", true); return; }
        split = Array.from({ length: n }, () => 1 / n);
      }
      const r = await placementPropose(modelId, split);
      if (disposed) return;
      if (r.error || !r.data) {
        projection = null;
        paintProjection();
        placeMsg(`Propose failed: ${r.error ?? "no projection returned"}`, true);
        return;
      }
      projection = { backendId, modelId, data: r.data };
      placeMsg("");
      paintProjection();
      const applyBtn = root.querySelector("#placeApply") as HTMLButtonElement | null;
      if (applyBtn) applyBtn.disabled = false;
    } finally {
      placeBusy = false;
      const btn = root.querySelector("#placePropose") as HTMLButtonElement | null;
      if (btn) btn.disabled = false;
    }
  };

  const onApply = async (): Promise<void> => {
    if (!projection || placeBusy) return;
    placeBusy = true;
    const applyBtn = root.querySelector("#placeApply") as HTMLButtonElement | null;
    if (applyBtn) applyBtn.disabled = true;
    try {
      const { backendId, modelId } = projection;
      const split = projection.data.split;
      const backend = placeBackendRows.find((b) => b.id === backendId);
      const model = placeModelRows.find((mdl) => mdl.id === modelId);
      let assignedNow = false;
      // When the backend does not already resolve to the selected
      // model, align it first: model_assign sets the backend's
      // model_file to the model's gguf_path and persists. If the
      // assignment fails, stop — placement_apply must not run against
      // a pairing the operator did not get.
      if (backend && model && !sameModelFile(backend.model_file, model.gguf_path)) {
        placeMsg(`Assigning ${modelId} to ${backendId}\u2026`);
        const ar = await modelAssign(backendId, modelId, split);
        if (disposed) return;
        if (ar.error) {
          placeMsg(`Assign failed: ${ar.error}`, true);
          return;
        }
        if (!ar.data.persisted) {
          placeMsg("The assignment was applied to the in-memory configuration only \u2014 it was not saved (persisted: false) and will be lost when the app restarts.", true);
          return;
        }
        assignedNow = true;
        // The persisted state changed under the form; the local row
        // follows it so the backend's "currently assigned" label and
        // the next signature check agree with the backend.
        backend.model_file = model.gguf_path;
      }
      const r = await placementApply(modelId, split);
      if (disposed) return;
      if (r.error) {
        placeMsg(r.error, true);
        return;
      }
      if (r.data.persisted) {
        // Repaint before messaging (the repaint rebuilds #placeMsg).
        if (assignedNow) repaintPlacementPreserving();
        placeMsg(assignedNow
          ? `Assigned ${modelId} to ${backendId} and saved the split. Restart the backend to use it.`
          : "Split saved to the configuration. Restart the backend to use it.");
      } else {
        placeMsg("The split was applied to the in-memory configuration only \u2014 it was not saved (persisted: false) and will be lost when the app restarts.", true);
      }
    } finally {
      placeBusy = false;
      const btn = root.querySelector("#placeApply") as HTMLButtonElement | null;
      if (btn && projection) btn.disabled = false;
    }
  };

  /** (Re)build the placement form's data from fetched rows. */
  const applyPlacementData = (backs: BackendRow[], models: ModelRow[]): void => {
    placeBackendRows = backs.filter((b) => b.configured);
    placeModelRows = models;
  };

  const placeSignature = (): string =>
    placeBackendRows.map((b) => `${b.id}:${b.model_file}`).join("|")
    + `#${placeModelRows.map((mdl) => mdl.id).join("|")}`;

  /** Repaint the placement form after a data refresh, preserving the
   *  operator's selected backend and model and any rendered
   *  projection. */
  const repaintPlacementPreserving = (): void => {
    const bSelBefore = root.querySelector("#placeBackend") as HTMLSelectElement | null;
    const mSelBefore = root.querySelector("#placeModel") as HTMLSelectElement | null;
    const selBackend = bSelBefore?.value ?? "";
    const selModel = mSelBefore?.value ?? "";
    paintPlacement();
    const bSel = root.querySelector("#placeBackend") as HTMLSelectElement | null;
    if (bSel && selBackend && Array.from(bSel.options).some((o) => o.value === selBackend)) {
      bSel.value = selBackend;
    }
    const mSel = root.querySelector("#placeModel") as HTMLSelectElement | null;
    if (mSel && selModel && Array.from(mSel.options).some((o) => o.value === selModel)) {
      mSel.value = selModel;
    }
    paintProjection();
    const applyBtn = root.querySelector("#placeApply") as HTMLButtonElement | null;
    if (applyBtn) applyBtn.disabled = !projection;
  };

  /** Refresh the form's backend/model data on the poll tick: new or
   *  changed backends appear without a remount, but an unchanged data
   *  set never repaints (the operator's interaction is untouched). */
  const refreshPlacement = async (): Promise<void> => {
    if (!placeEl || placeBusy) return;
    const [backs, models] = await Promise.all([getBackends(), getModels()]);
    if (disposed || !backs.live || backs.error) return;
    const before = placeSignature();
    applyPlacementData(backs.data, models.data);
    if (placeSignature() === before) return;
    repaintPlacementPreserving();
  };

  const initPlacement = async (): Promise<void> => {
    if (!placeEl) return;
    const [backs, models] = await Promise.all([getBackends(), getModels()]);
    if (disposed) return;
    if (!backs.live) {
      placeEl.innerHTML = `<div class="card"><div class="sec-title">${icon("layers")} Placement</div>
        <div style="font-size:12px;color:var(--text2)">Placement is available in the installed app with live data.</div></div>`;
      return;
    }
    applyPlacementData(backs.data, models.data);
    paintPlacement();
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
  void initPlacement();

  // Live refresh: telemetry ages even when the user does nothing. The
  // placement container is repainted only when its data set changes,
  // and then with selection + projection preserved.
  const timer = window.setInterval(() => {
    if (!disposed) { void load(); void refreshPlacement(); }
  }, 2000);

  return () => {
    disposed = true;
    window.clearInterval(timer);
    refreshBtn?.removeEventListener("click", onRefresh);
    resetBtn?.removeEventListener("click", onResetPeaks);
    snapshotBtn?.removeEventListener("click", onSnapshot);
    pollSel?.removeEventListener("change", onPollChange);
    const sel = placeEl?.querySelector("#placeBackend");
    const modelSel = placeEl?.querySelector("#placeModel");
    const proposeBtn = placeEl?.querySelector("#placePropose");
    const applyBtn = placeEl?.querySelector("#placeApply");
    sel?.removeEventListener("change", onPlaceSelectionChange);
    modelSel?.removeEventListener("change", onPlaceSelectionChange);
    proposeBtn?.removeEventListener("click", onPropose);
    applyBtn?.removeEventListener("click", onApply);
  };
}

/* ---------------- Backends ---------------- */

type CardNote = { text: string; err: boolean };

/** Quote an argv element for display when it contains whitespace or a
 *  quote, so the rendered launch command stays copy-paste accurate. */
function quoteArg(a: string): string {
  return /[\s"]/.test(a) ? `"${a.replace(/"/g, '\\"')}"` : a;
}

function backendCard(
  b: BackendRow,
  openArgv: Set<string>,
  argvCache: Map<string, string[]>,
  msgs: Map<string, CardNote>,
  logTails: Map<string, LogRow[]>,
  inFlight: Set<string>,
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
  const disableBtn = `<button class="btn" data-back-act="disable" data-back-id="${esc(b.id)}"${dis}>Disable</button>`;
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
  const actions = parts.join("\n        ");
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
  return `<div class="card">
      <h3>${stateDot(b.state)} ${esc(b.id)} <span class="port">:${b.port}</span>
        <span class="spacer" style="flex:1"></span>
        ${b.configured ? "" : `<span class="chip badge-warn">not in config</span>`}
        ${b.configured ? `<span class="chip">${b.enabled ? "enabled" : "disabled"}</span>` : ""}
        <span class="chip">${esc(b.state)}</span></h3>
      <div style="font-size:12px;color:var(--text2);margin-top:6px">Model: <span style="font-family:var(--mono)">${esc(b.model_file)}</span></div>
      ${orphanNote}
      <div style="display:grid;grid-template-columns:repeat(3,1fr);gap:10px;margin:12px 0">
        ${[["State", b.state], ["PID", b.pid !== null ? String(b.pid) : "\u2014"],
           ["Uptime", fmtUptime(b.uptime_secs)]]
          .map(([k, v]) => `<div class="kv"><span class="k">${k}</span><span class="v" style="font-size:13px">${esc(v)}</span></div>`).join("")}
      </div>
      <div class="card-actions">${actions}
        <button class="btn" data-back-act="argv" data-back-id="${esc(b.id)}"${dis}>${icon("logs")} Launch command</button></div>
      <div class="back-msg" style="margin-top:8px;font-size:12px">${msgHtml}</div>
      ${logsBlock}
      ${argv}
    </div>`;
}

export function renderBackends(): string {
  return `${topbar("Backends", `<button class="btn" id="backRefresh">${icon("restart")} Refresh</button>`)}
  <div class="view"><div id="backBanner"></div>
  <div id="backBody">${loadingHtml("Loading backends\u2026")}</div></div>`;
}

export async function wireBackends(root: HTMLElement): Promise<() => void> {
  const openArgv = new Set<string>();
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
  let actionInFlight = false;
  let disposed = false;
  /** Load sequence: only the most recently started load may paint. */
  let loadSeq = 0;

  const paint = (): void => {
    const b = root.querySelector("#backBody");
    if (!b) return;
    b.innerHTML = `<div class="grid2">${lastRows.map((row) => backendCard(row, openArgv, argvCache, msgs, logTails, inFlight)).join("")}</div>`;
  };

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
    const r = await getBackends();
    if (disposed || seq !== loadSeq) return; // a newer load supersedes this one
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
      if (prev !== undefined && prev !== row.state) msgs.delete(row.id);
      // A backend that is no longer failed gets a fresh tail fetch if it
      // fails again.
      if (row.state !== "failed") { logTails.delete(row.id); tailsWanted.delete(row.id); }
    }
    paint();
    void fetchLogTails();
  };

  const refreshBtn = root.querySelector("#backRefresh");
  const onRefresh = (): void => { void load(); };
  refreshBtn?.addEventListener("click", onRefresh);

  const onClick = async (ev: Event): Promise<void> => {
    const t = (ev.target as HTMLElement).closest("[data-back-act]") as HTMLElement | null;
    if (!t) return;
    const id = t.getAttribute("data-back-id") ?? "";
    const act = t.getAttribute("data-back-act") ?? "";

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
  root.addEventListener("click", onClick);

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

function modelCard(m: ModelRow, downloading: Set<string>, notes: Map<string, ModelNote>): string {
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
  const sizeText = m.on_disk
    ? esc(fmtBytes(m.size_bytes))
    : partial > 0 ? `${esc(fmtBytes(partial))} (partial)` : "not downloaded";
  return `<div class="card" style="margin-bottom:12px">
    <h3>${icon("box")} ${esc(m.id)} <span class="chip">${esc(m.quant)}</span>
      <span class="spacer" style="flex:1"></span>
      ${modelShaBadge(m)}</h3>
    <div style="display:grid;grid-template-columns:1fr 1fr 1fr;gap:12px;margin:12px 0;align-items:center">
      <div class="kv"><span class="k">File</span><span class="v" style="font-size:12px">${esc(m.gguf_path)}</span></div>
      <div class="kv"><span class="k">Size</span><span class="v" style="font-size:12px">${sizeText}</span></div>
      ${m.params_b > 0 ? `<div class="kv"><span class="k">Parameters</span><span class="v" style="font-size:12px">${m.params_b}B</span></div>` : ""}
    </div>
    <div style="font-size:11px;color:var(--text3);font-family:var(--mono)">sha256: ${esc(m.sha256 ? `${m.sha256.slice(0, 16)}…` : "—")}</div>
    <div class="card-actions">${actions}</div>
    ${dlErrHtml}
    ${noteHtml}
  </div>`;
}

export function renderModels(): string {
  return `${topbar("Models")}
  <div class="view" style="max-width:900px"><div id="modelBanner"></div>
  <div id="modelBody">${loadingHtml("Loading models…")}</div></div>`;
}

export async function wireModels(root: HTMLElement): Promise<() => void> {
  /** Downloads started from this view whose spawn invoke has not settled
   *  yet; the backend's `downloading` flag carries the state after that. */
  const downloading = new Set<string>();
  const notes = new Map<string, ModelNote>();
  let lastRows: ModelRow[] = [];
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
    setBanner(root, "modelBanner", bannerHtml(r));
    const b = root.querySelector("#modelBody");
    if (!b) return;
    if (r.error) { b.innerHTML = emptyHtml(`Models unavailable: ${r.error}`); return; }
    if (!r.data.length) {
      lastRows = [];
      b.innerHTML = emptyHtml(r.live
        ? "No models are configured. Models are defined in the app configuration."
        : "No model data in sample mode.");
      syncPoll();
      return;
    }
    lastRows = r.data;
    b.innerHTML = lastRows.map((m) => modelCard(m, downloading, notes)).join("");
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

  await load();

  return () => {
    disposed = true;
    if (pollTimer !== null) window.clearInterval(pollTimer);
    root.removeEventListener("click", onClick);
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
  </div>
  <div style="font-size:11px;color:var(--text3);margin-top:8px">The bundle contains raw, unredacted process logs. Review it before sharing.</div>
  <div id="logMsg" style="font-size:12px;margin-top:6px"></div></div>
  <div id="logMeta" style="font-size:12px;color:var(--text2);margin-bottom:8px"></div>
  <div id="logBody">${loadingHtml("Loading logs\u2026")}</div></div>`;
}

export async function wireLogs(root: HTMLElement, initialScope?: string): Promise<() => void> {
  let disposed = false;
  let loading = false;
  let followTimer: number | null = null;
  let searchTimer: number | null = null;
  const scopeSel = root.querySelector("#logScope") as HTMLSelectElement | null;
  const levelSel = root.querySelector("#logLevel") as HTMLSelectElement | null;
  const searchInput = root.querySelector("#logSearch") as HTMLInputElement | null;
  const limitSel = root.querySelector("#logLimit") as HTMLSelectElement | null;
  const followChk = root.querySelector("#logFollow") as HTMLInputElement | null;
  const refreshBtn = root.querySelector("#logRefresh");
  const exportBtn = root.querySelector("#logExport") as HTMLButtonElement | null;

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

  const filtersActive = (): boolean => Boolean(levelSel?.value || searchInput?.value.trim());

  const load = async (): Promise<void> => {
    if (loading) return;
    loading = true;
    try {
      const scope = scopeSel?.value ?? "all";
      const limit = Number(limitSel?.value ?? 200);
      const level = levelSel?.value || undefined;
      const search = searchInput?.value.trim() || undefined;
      const r = await getLogs(scope, { limit, level, search });
      if (disposed) return;
      setBanner(root, "logBanner", bannerHtml(r));
      mergeScopeOptions(r.data.scopes.map((s) => ({
        value: s.id,
        label: s.previous ? `${s.id} (incl. previous run)` : s.id,
      })));
      const meta = root.querySelector("#logMeta");
      if (meta) {
        const trunc = r.data.truncated
          ? `Showing the latest ${r.data.lines.length} of ${r.data.total_matching} matching lines. ` : "";
        meta.textContent = `${trunc}Logs are kept in memory for this session only \u2014 up to 10,000 lines per process; one previous run per process is kept.`;
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

  const onChange = (): void => { void load(); };
  const onSearch = (): void => {
    if (searchTimer !== null) window.clearTimeout(searchTimer);
    searchTimer = window.setTimeout(() => { searchTimer = null; void load(); }, 300);
  };
  const onFollow = (): void => {
    if (followTimer !== null) { window.clearInterval(followTimer); followTimer = null; }
    if (followChk?.checked) {
      followTimer = window.setInterval(() => { if (!disposed) void load(); }, 2000);
      void load();
    }
  };
  const onExport = async (): Promise<void> => {
    if (exportBtn) exportBtn.disabled = true;
    const r = await exportDiagnostics();
    if (exportBtn) exportBtn.disabled = false;
    if (disposed) return;
    if (r.error) { msg(`Export failed: ${r.error}`, true); return; }
    msg(r.data.path ? `Diagnostics bundle written to ${r.data.path}.` : "Diagnostics bundle written.");
  };
  scopeSel?.addEventListener("change", onChange);
  levelSel?.addEventListener("change", onChange);
  limitSel?.addEventListener("change", onChange);
  searchInput?.addEventListener("input", onSearch);
  followChk?.addEventListener("change", onFollow);
  refreshBtn?.addEventListener("click", onChange);
  exportBtn?.addEventListener("click", onExport);

  await load();

  return () => {
    disposed = true;
    if (followTimer !== null) window.clearInterval(followTimer);
    if (searchTimer !== null) window.clearTimeout(searchTimer);
    scopeSel?.removeEventListener("change", onChange);
    levelSel?.removeEventListener("change", onChange);
    limitSel?.removeEventListener("change", onChange);
    searchInput?.removeEventListener("input", onSearch);
    followChk?.removeEventListener("change", onFollow);
    refreshBtn?.removeEventListener("click", onChange);
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

  const summary = `<div class="card" style="margin-bottom:12px"><div class="sec-title">${dot(status.up ? "ok" : "err")} Gateway \u2014 LiteLLM \u00b7 ${status.up ? "up" : "down"}</div>
    <div style="display:grid;grid-template-columns:repeat(4,1fr);gap:10px">
      ${[["Port", `:${port}`], ["Groups", String(status.groups)], ["Routing rules", String(status.routing_rules)], ["Routing test traces", `${status.routing_log_len} recorded`]]
        .map(([k, v]) => `<div class="kv"><span class="k">${k}</span><span class="v" style="font-size:13px">${esc(v)}</span></div>`).join("")}
    </div>
    <div style="font-size:12px;color:var(--text2);margin-top:8px">Routing test traces are dry-run decisions recorded from routing tests this session \u2014 live requests are not logged.</div></div>`;

  const endpoint = `<div class="card" style="margin-bottom:12px"><div class="sec-title">${icon("gateway")} Client endpoint</div>
    <div style="font-size:12px;color:var(--text2);display:grid;gap:4px">
      <div>Base URL: <span style="font-family:var(--mono);color:var(--text)">http://127.0.0.1:${port}/v1</span></div>
      <div>Bound to loopback (127.0.0.1) only \u2014 reachable from this machine, not from the LAN.</div>
      <div>No API key is required from local clients.</div>
    </div>
    <div class="card-actions">
      <button class="btn" id="gwStartBtn"${starting || stopping ? " disabled" : ""}>${icon("restart")} ${status.up ? "Restart gateway" : "Start gateway"}</button>
      <button class="btn" id="gwStopBtn"${status.up && !starting && !stopping ? "" : " disabled"}>${icon("pause")} Stop gateway</button>
      <a class="btn" href="#/wizard">${icon("wrench")} Setup Wizard</a>
    </div>
    <div style="font-size:12px;color:var(--text2);margin-top:6px">Runs the Setup Wizard's start-gateway step: LiteLLM binds 127.0.0.1. When the OpenRouter tier is enabled, the fallback is armed from Windows Credential Manager.</div></div>`;

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
        ${cfg.groups.map((g) => `<span class="chip">${esc(g.name)}</span> <span style="color:var(--text3)">\u2192</span> ${g.members.map(memberHtml).join(", ") || "\u2014"}${g.fallbacks.length ? `<br/><span style="color:var(--text3)">fallback - - \u2192</span> ${g.fallbacks.map(fallbackHtml).join(", ")}` : ""}`).join("<br/>")}
      </div>${strategyNote}</div>`
    : "";

  const groups = cfg.groups.length
    ? `<div class="grid2">${cfg.groups.map((g) => `<div class="card">
      <h3><span class="chip" style="border-color:var(--accent)">${esc(g.name)}</span>
        <span class="spacer" style="flex:1"></span><span class="chip">${esc(g.strategy)}</span></h3>
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

  return backendsNote + summary + endpoint + openrouter + flow + groups + rules + yaml;
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

  const gwMsg = (text: string, isErr = false): void => {
    const m = root.querySelector("#gwMsg");
    if (m) m.innerHTML = text ? `<span style="color:${isErr ? "var(--err)" : "var(--text2)"}">${esc(text)}</span>` : "";
  };

  const load = async (): Promise<void> => {
    const [st, cfg, backs] = await Promise.all([getGatewayStatus(), getGatewayConfig(), getBackends()]);
    if (disposed) return;
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
    b.innerHTML = gatewayPaint(st.data, cfg.data, backs.data, yamlOpen, yamlText, yamlErr, starting, stopping, backs.error ?? null);
  };

  const onClick = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLElement;

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
        } else {
          const o = r.data.outcome;
          const tail = o.log_tail.length ? ` ${o.log_tail[o.log_tail.length - 1]}` : "";
          gwMsg(`Start-gateway step ${o.state} \u2014 ${o.message}${tail}`, o.state === "failed");
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
  root.addEventListener("click", onClick);

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
    <button class="btn" id="setExportBtn">${icon("download")} Export diagnostics bundle</button></div>
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
      else setMsg(r.data.path ? `Diagnostics bundle written to ${r.data.path}.` : "Diagnostics bundle written.");
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
  return `<div class="step ${s.state}">${stIcon}${icon("wrench")}
    <div><div class="sname">${esc(s.name)}</div><div class="sdetail">${esc(s.detail)}</div>${remediation}${logTail}${actions}</div>
    <div class="sstate">${s.note && s.state === "done" ? `<span class="chip badge-warn">${esc(s.note)}</span> ` : ""}${stateTxt}</div></div>`;
}

interface WizardPaintOpts {
  busy?: boolean;
  runningStepId?: string | null;
  statusLine?: string | null;
}

function paintWizard(root: HTMLElement, steps: WizardStepState[], live: boolean, opts: WizardPaintOpts = {}): void {
  // The backend's Running state cannot be observed mid-step (the run
  // command holds the wizard lock for the whole execution, and the
  // state query needs the same lock), so the view marks the step it
  // invoked as running itself. The override applies only while the
  // server state is still queued — a finished step keeps its real
  // state.
  const eff = opts.runningStepId
    ? steps.map((s) => (s.id === opts.runningStepId && s.state === "queued" ? { ...s, state: "running" as const } : s))
    : steps;
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
          <div>${icon("server")} services start at the gateway step</div></div></div>
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

  const paint = (): void => paintWizard(root, steps, liveNow, { busy, runningStepId, statusLine });

  const refresh = async (): Promise<void> => {
    const r = await getWizardSteps();
    if (disposed) return;
    steps = r.data;
    liveNow = r.live;
    paint();
    if (r.error) logLine(`could not load setup state: ${r.error}`, "lvl-err");
  };

  const runOne = async (stepId: string): Promise<boolean> => {
    logLine(`running step: ${stepId}`);
    const r = await runWizardStep(stepId);
    if (r.error) { logLine(`step ${stepId} error: ${r.error}`, "lvl-err"); await refresh(); return false; }
    const o = r.data.outcome;
    logLine(`step ${stepId}: ${o.state} \u2014 ${o.message}`, o.state === "done" || o.state === "manual" ? "lvl-info" : "lvl-err");
    await refresh();
    return o.state === "done" || o.state === "manual" || o.state === "skipped";
  };

  const runBtn = root.querySelector("#runBtn") as HTMLButtonElement | null;
  runBtn?.addEventListener("click", async () => {
    if (!isTauri()) { alert("Sample mode: setup runs in the installed app."); return; }
    if (busy) return;
    busy = true;
    paint();
    try {
      await refresh();
      let paused: string | null = null;
      for (const s of [...steps]) {
        if (s.state === "done" || s.state === "manual" || s.state === "skipped") continue;
        runningStepId = s.id;
        statusLine = `Running step: ${s.name}\u2026`;
        paint();
        const ok = await runOne(s.id);
        runningStepId = null;
        if (!ok) { paused = s.name; break; }
      }
      if (paused) {
        statusLine = `Setup paused \u2014 ${paused} failed. Fix the cause and retry.`;
        logLine("setup paused \u2014 fix the failed step and retry", "lvl-warn");
      } else {
        statusLine = "Setup run finished.";
      }
    } finally {
      busy = false;
      runningStepId = null;
      paint();
    }
  });

  const audit = root.querySelector("#auditBtn");
  audit?.addEventListener("click", async () => {
    if (!isTauri()) { alert("Sample mode: audit would re-run all checks here."); return; }
    if (busy) return;
    busy = true;
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
      // Paint the audit's own per-step results: refresh() would reload
      // the install wizard's state, which an audit never changes — that
      // is why the old handler appeared to do nothing.
      steps = r.data.steps;
      statusLine = `Audit complete: ${r.data.done}/${r.data.total} checks passed \u2014 the checklist shows the audit results.`;
      logLine(`audit complete: ${r.data.done}/${r.data.total} checks passed`);
    } finally {
      busy = false;
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
    const runStepBtn = t.closest("[data-run-step]") as HTMLElement | null;
    if (runStepBtn) {
      if (busy) return;
      const id = runStepBtn.getAttribute("data-run-step") ?? "";
      busy = true;
      runningStepId = id;
      statusLine = `Running step: ${steps.find((s) => s.id === id)?.name ?? id}\u2026`;
      paint();
      try {
        await runOne(id);
      } finally {
        busy = false;
        runningStepId = null;
        statusLine = null;
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
