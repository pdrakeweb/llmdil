/* GPUs view (split from static.ts). */

import { getAppConfig, getBackends, getGpuHistory, getGpuTelemetry, getModels, resetTelemetryPeaks, saveTelemetrySnapshot, setTelemetryInterval } from "../api";
import type { BackendRow, GpuHistoryData, GpuHistoryPoint, ModelRow } from "../api";
import { fmtCount, fmtGb, gaugeRing, historyChart, historyChartX, historyChartY, sparkline, stackedBar, trim1 } from "../charts";
import type { HistoryChartGeom, HistoryChartSeries } from "../charts";
import { esc, keepFocus } from "../html";
import { pageState } from "../viewState";
import { icon } from "../icons";
import { META, fromWire } from "../lifecycle";
import type { GpuInfo } from "../types";
import { C, SELECT_STYLE, bannerHtml, baseName, cadenceBucket, dot, emptyHtml, loadingHtml, modelDisplayName, onOvGoto, ovStatusBtn, setBanner, topbar } from "./common";

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
  // The peak is a value of its own (Reset peaks clears it).
  const peak = g.temp_peak_c !== undefined ? ` <span style="color:var(--text3)" title="Highest temperature since the last reset or app start">\u00b7 peak ${trim1(g.temp_peak_c)} \u00b0C</span>` : "";
  const memUtil = g.mem_util_pct !== undefined
    ? `<div class="mrow"><span class="mlabel">Memory util</span><span class="mval">${trim1(g.mem_util_pct)}%</span></div>` : "";
  const fan = g.fan_pct !== undefined
    ? `<div class="mrow"><span class="mlabel">Fan</span><span class="mval">${trim1(g.fan_pct)}%</span></div>` : "";
  const tempSpark = g.temp_spark && g.temp_spark.length
    ? `<div class="mrow"><span></span>${sparkline(g.temp_spark, 240, 26, C.amber, 0.18, 10)}</div>` : "";
  const powerSpark = g.power_w != null && g.power_spark && g.power_spark.length
    ? `<div class="mrow"><span></span>${sparkline(g.power_spark, 240, 26, C.green, 0.18, 20)}</div>` : "";
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
      <span style="font-size:12px;font-family:var(--mono)">${measured ? `${fmtCount(p.vram_mib as number)} MiB` : "not measured"}</span></div>`;
    const detail = open ? `<div style="padding:6px 6px 8px 24px;font-size:11px;color:var(--text2);display:grid;gap:2px;border-bottom:1px solid #232a34">
      <div>Process: ${esc(p.process)}</div>
      <div>PID: ${p.pid}</div>
      <div>VRAM on GPU ${esc(g.id)}: ${measured ? `${fmtCount(p.vram_mib as number)} MiB` : "not measured — per-process accounting is unavailable for this GPU"}</div>
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
  // Power: a measured draw renders with the limit alongside (the
  // dashboard idiom). A null draw renders the "not measured" value
  // only when the payload records the driver-N/A state
  // (power_absence) — that wording belongs to the hardware state. A
  // missing field (bug state) or an unclassified absence keeps the
  // row absent (principle 13), never borrowing it.
  const powerRow = g.power_w != null
    ? `<div class="mrow"><span class="mlabel">Power</span><span class="mval">${g.power_limit_w !== undefined ? `${trim1(g.power_w)} / ${trim1(g.power_limit_w)} W` : `${trim1(g.power_w)} W`}</span></div>${powerSpark}`
    : g.power_absence === "driver_not_available"
    ? `<div class="mrow"><span class="mlabel">Power</span><span class="mval">not measured</span></div>` : "";
  return `<div class="card" id="card-gpu-${esc(g.id)}">
      <div class="gpu-head">${dot(g.status === "down" ? "err" : g.status)} ${esc(g.name)}${pci}${staleChip}${downChip}
        <span class="spacer" style="flex:1"></span>${driver}</div>
      <div${down ? ` style="opacity:.65"` : ""}>
      ${vramHtml}
      <div style="display:flex;gap:18px;justify-content:center;margin:12px 0">
        ${gaugeRing(g.util_pct, 84, C.blue, trim1(g.util_pct), "UTIL")}
      </div>
      <div class="gpu-metrics">
        <div class="mrow"><span class="mlabel">Temperature</span><span class="mval">${trim1(g.temp_c)} \u00b0C${peak}</span></div>
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

/** The page's list controls, kept across visits. */
const gpusView = pageState("gpus", { sort: "index", filter: "" });

export function renderGpus(): string {
  return `${topbar("GPUs", `
    <label style="font-size:12px;color:var(--text2);display:inline-flex;align-items:center;gap:6px" title="Sets the sampler cadence; applies on restart">Telemetry poll
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
  <div id="gpuMsg" style="font-size:12px;min-height:16px;margin-bottom:8px"></div>
  <div id="gpuOverview" style="margin-bottom:10px"></div>
  <div style="display:flex;gap:10px;align-items:center;margin-bottom:10px;flex-wrap:wrap">
    <label style="font-size:12px;color:var(--text2)">Sort
      <select id="gpuSortSel" style="${SELECT_STYLE}">
        ${[["index", "GPU index"], ["util", "Utilization"], ["vram", "VRAM used"], ["temp", "Temperature"]].map(([v, l]) => `<option value="${v}"${gpusView.sort === v ? " selected" : ""}>${l}</option>`).join("")}
      </select></label>
    <input id="gpuFilter" type="text" value="${esc(gpusView.filter)}" placeholder="Filter by GPU name" aria-label="Filter GPUs" style="min-width:180px">
  </div>
  <div id="gpuBody">${loadingHtml("Loading GPU telemetry\u2026")}</div>
  <div id="gpuHistory"></div></div>`;
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
  const paintBody = (): void => keepFocus(root, paintBodyNow);
  const paintBodyNow = (): void => {
    const f = gpusView.filter.trim().toLowerCase();
    let list = lastGpus.filter((g) => !f || g.name.toLowerCase().includes(f) || `gpu ${g.id}`.includes(f));
    if (gpusView.sort === "util") list = [...list].sort((a, z) => z.util_pct - a.util_pct);
    else if (gpusView.sort === "vram") list = [...list].sort((a, z) => z.vram_used_gb - a.vram_used_gb);
    else if (gpusView.sort === "temp") list = [...list].sort((a, z) => z.temp_c - a.temp_c);
    const ov = root.querySelector("#gpuOverview");
    if (ov) {
      ov.innerHTML = list.length ? `<div class="ovlist ov-g">
        <div class="ov-head"><span></span><span>GPU</span><span>Util</span><span>VRAM</span><span>Temp</span><span>Power</span><span>Processes</span></div>
        ${list.map((g) => {
          const btn = g.status === "ok" ? ovStatusBtn("ok", "check", `card-gpu-${g.id}`, `${g.name}: ok — go to card`)
            : g.status === "down" ? ovStatusBtn("err", "err", `card-gpu-${g.id}`, `${g.name}: down — go to card`)
            : g.status === "degraded" || g.status === "starting" ? ovStatusBtn("warn", "warn", `card-gpu-${g.id}`, `${g.name}: ${g.status} — go to card`)
            : ovStatusBtn("idle", "pause", `card-gpu-${g.id}`, `${g.name}: ${g.status} — go to card`);
          return `<div class="ov-row">${btn}<span class="ov-name">${esc(g.name)}</span><span class="ov-dim">${trim1(g.util_pct)}%</span><span class="ov-dim">${fmtGb(g.vram_used_gb)} / ${fmtGb(g.vram_total_gb)} GiB</span><span class="ov-dim">${trim1(g.temp_c)} \u00b0C</span><span class="ov-dim">${g.power_w == null ? (g.power_absence === "driver_not_available" ? "not measured" : "N/A") : `${trim1(g.power_w)} W`}</span><span class="ov-dim">${(g.processes ?? []).length}</span></div>`;
        }).join("")}</div>` : "";
    }
    const b = root.querySelector("#gpuBody");
    if (!b) return;
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

  /* ---------------- History charts ----------------
     Recorded per-GPU history (temperature, utilization, throughput,
     power) from get_gpu_history: sampled server-side every 15 s and
     persisted across restarts. One chart per metric, one series per
     GPU; the range selector switches the dataset. Hover shows the
     exact value + timestamp at the nearest recorded point. Breaks
     in a line are unrecorded or unmeasured intervals — nothing is
     interpolated. */
  const HIST_RANGES = ["1h", "24h", "7d"];
  let histRange = "24h";
  const histCharts = new Map<string, { geom: HistoryChartGeom; series: HistoryChartSeries[] }>();
  const HIST_METRICS: { key: string; title: string; unit: string; fixed?: [number, number]; get: (p: GpuHistoryPoint) => number | null }[] = [
    { key: "temp", title: "Temperature (°C)", unit: "°C", get: (p) => p.temp_c },
    { key: "util", title: "Utilization (%)", unit: "%", fixed: [0, 100], get: (p) => p.util_pct },
    { key: "tps", title: "Throughput (tok/s)", unit: "tok/s", get: (p) => p.throughput_toks },
    { key: "power", title: "Power (W)", unit: "W", get: (p) => p.power_w },
  ];
  const HIST_TIP = "Throughput is each running backend's gateway token rate, attributed to the GPUs its placement uses: the full rate on a single GPU, pro-rated by tensor-split share across a split. A backend that is not running contributes nothing, and a GPU with no contributing backend shows no throughput value. Power appears only where the card reports it. Breaks in a line are intervals with no recording — the app was off, or the value was not measured.";
  const fmtHistTime = (ts: number): string => {
    const d = new Date(ts);
    const hm = d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
    return histRange === "7d" ? `${d.toLocaleDateString([], { weekday: "short" })} ${hm}` : hm;
  };
  const paintHistory = (data: GpuHistoryData, error?: string): void => {
    const host = root.querySelector("#gpuHistory");
    if (!host) return;
    histCharts.clear();
    const palette = [C.blue, C.purple, C.teal, C.amber, C.green];
    const rangeBtns = HIST_RANGES.map((r) => `<button class="btn" data-hist-range="${r}" style="${r === histRange ? "border-color:#4a9eff;color:#4a9eff" : ""}">${r}</button>`).join("");
    const emptyNote = `<div style="font-size:12px;color:var(--text2)">No GPU history recorded yet — history accumulates while the app runs.</div>`;
    let body: string;
    if (error) {
      body = `<div style="font-size:12px;color:var(--text2)">GPU history unavailable: ${esc(error)}</div>`;
    } else if (!data.gpus.some((g) => g.points.length)) {
      body = emptyNote;
    } else {
      const cards = HIST_METRICS.map((m) => {
        const series: HistoryChartSeries[] = data.gpus.map((g, i) => ({
          label: g.name,
          color: palette[i % palette.length],
          intervalMs: g.interval_ms,
          points: g.points.map((p) => ({ ts: p.ts_ms, v: m.get(p) })),
        }));
        // A metric no GPU measured in this window renders no chart
        // (principles 12/13): e.g. Power before any card reports it.
        if (!series.some((s) => s.points.some((p) => p.v !== null))) return "";
        const { svg, geom } = historyChart(series, { fixedRange: m.fixed, fmtTime: fmtHistTime });
        if (!svg) return "";
        histCharts.set(m.key, { geom, series });
        const legend = series.map((s) => `<span><span class="sw" style="background:${s.color}"></span>${esc(s.label)}</span>`).join("");
        return `<div class="card">
          <div style="font-size:12px;color:var(--text2)">${esc(m.title)}</div>
          <div class="legend" style="margin:2px 0 4px">${legend}</div>
          <div class="hist-wrap" data-hist-metric="${m.key}" style="position:relative">${svg}<div class="hist-tip" style="position:absolute;display:none;pointer-events:none;background:var(--panel2);border:1px solid var(--line);border-radius:6px;padding:6px 8px;font-size:11px;color:var(--text);z-index:5;white-space:nowrap"></div></div>
        </div>`;
      }).join("");
      body = cards ? `<div class="cards-fluid">${cards}</div>` : emptyNote;
    }
    host.innerHTML = `<div style="display:flex;align-items:center;gap:10px;margin:14px 0 10px">
        <span style="font-size:14px;font-weight:600">History</span>
        <span style="display:inline-flex;gap:4px">${rangeBtns}</span>
        <span class="info-tip" title="${esc(HIST_TIP)}">${icon("info")}</span>
      </div>${body}`;
    attachHistHover();
  };
  const attachHistHover = (): void => {
    root.querySelectorAll(".hist-wrap").forEach((wrap) => {
      const metric = wrap.getAttribute("data-hist-metric") ?? "";
      const st = histCharts.get(metric);
      const svg = wrap.querySelector("svg");
      const tip = wrap.querySelector(".hist-tip") as HTMLElement | null;
      const m = HIST_METRICS.find((x) => x.key === metric);
      if (!st || !svg || !tip || !m) return;
      const zone = svg.querySelector(".hist-zone");
      const guide = svg.querySelector(".hist-guide");
      const markers = svg.querySelector(".hist-markers");
      if (!zone || !markers) return;
      const clear = (): void => {
        tip.style.display = "none";
        markers.innerHTML = "";
        guide?.setAttribute("opacity", "0");
      };
      zone.addEventListener("mouseleave", clear);
      zone.addEventListener("mousemove", (ev: Event) => {
        const e = ev as MouseEvent;
        const rect = svg.getBoundingClientRect();
        if (!rect.width) return;
        const x = ((e.clientX - rect.left) / rect.width) * st.geom.w;
        const ts = st.geom.t0 + ((x - st.geom.padL) / Math.max(1, st.geom.w - st.geom.padL - st.geom.padR)) * (st.geom.t1 - st.geom.t0);
        // Nearest recorded point per series; a series whose nearest
        // point is far (> 1.5 intervals) or unmeasured shows nothing.
        let guideTs: number | null = null;
        const rows: string[] = [];
        let circles = "";
        for (const s of st.series) {
          let best: { ts: number; v: number | null } | null = null;
          for (const p of s.points) {
            if (!best || Math.abs(p.ts - ts) < Math.abs(best.ts - ts)) best = p;
          }
          if (!best || best.v === null || Math.abs(best.ts - ts) > 1.5 * s.intervalMs) continue;
          if (guideTs === null) guideTs = best.ts;
          circles += `<circle cx="${historyChartX(st.geom, best.ts).toFixed(1)}" cy="${historyChartY(st.geom, best.v).toFixed(1)}" r="3" fill="${s.color}" stroke="var(--panel)" stroke-width="1.5"/>`;
          rows.push(`<div><span style="display:inline-block;width:8px;height:8px;border-radius:2px;background:${s.color};margin-right:5px"></span>${esc(s.label)}: ${trim1(best.v)} ${esc(m.unit)}</div>`);
        }
        if (guideTs === null || !rows.length) { clear(); return; }
        const gx = historyChartX(st.geom, guideTs);
        guide?.setAttribute("x1", gx.toFixed(1));
        guide?.setAttribute("x2", gx.toFixed(1));
        guide?.setAttribute("opacity", "0.6");
        markers.innerHTML = circles;
        tip.innerHTML = `<div style="color:var(--text3);margin-bottom:2px">${esc(fmtHistTime(guideTs))}</div>${rows.join("")}`;
        tip.style.display = "block";
        const wrapW = (wrap as HTMLElement).clientWidth;
        const px = (gx / st.geom.w) * wrapW;
        tip.style.left = px > wrapW * 0.62 ? "" : `${Math.round(px + 10)}px`;
        tip.style.right = px > wrapW * 0.62 ? `${Math.round(wrapW - px + 10)}px` : "";
        tip.style.top = "6px";
      });
    });
  };
  let histLoading = false;
  const loadHistory = async (): Promise<void> => {
    if (histLoading) return;
    histLoading = true;
    try {
      const r = await getGpuHistory(histRange);
      if (disposed) return;
      paintHistory(r.data, r.error);
    } finally {
      histLoading = false;
    }
  };
  const onHistRange = (ev: Event): void => {
    const t = (ev.target as HTMLElement).closest("[data-hist-range]");
    if (!t) return;
    const r = t.getAttribute("data-hist-range");
    if (!r || r === histRange) return;
    histRange = r;
    void loadHistory();
  };
  root.addEventListener("click", onHistRange);

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
      if (!b.configured || !META[fromWire(b.lifecycle ?? b.state)].isLive) continue;
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
        rows.push({ model: modelDisplayName(b.model_file, gpuModels), backend: b.id, pct: Math.round(frac * 100), mib: measured });
      } else if (!split.length && measured !== null && measured >= 1024) {
        rows.push({ model: modelDisplayName(b.model_file, gpuModels), backend: b.id, pct: 100, mib: measured });
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
  gpuSortSel?.addEventListener("change", () => { gpusView.sort = gpuSortSel.value; paintBody(); });
  gpuFilterInput?.addEventListener("input", () => { gpusView.filter = gpuFilterInput.value; paintBody(); });
  // Named so the cleanup can remove it: `root` is the persistent #view
  // container (code review v0.1.60 UI-7).
  const onProcToggle = (ev: Event): void => {
    if (disposed) return;
    const t = (ev.target as HTMLElement).closest("[data-proc-toggle]");
    if (!t) return;
    const key = t.getAttribute("data-proc-toggle") ?? "";
    if (!key) return;
    if (expandedProcs.has(key)) expandedProcs.delete(key); else expandedProcs.add(key);
    paintBody();
  };
  root.addEventListener("click", onProcToggle);
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
  void loadHistory();

  // Live refresh: telemetry ages even when the user does nothing.
  const timer = window.setInterval(() => {
    if (!disposed) { void load(); }
  }, 2000);
  // History refreshes on its own slower cadence — the recorder
  // ticks every 15 s server-side.
  const histTimer = window.setInterval(() => {
    if (!disposed) { void loadHistory(); }
  }, 30000);

  return () => {
    disposed = true;
    window.clearInterval(timer);
    window.clearInterval(histTimer);
    root.removeEventListener("click", onProcToggle);
    root.removeEventListener("click", onOvGoto);
    root.removeEventListener("click", onHistRange);
    refreshBtn?.removeEventListener("click", onRefresh);
    resetBtn?.removeEventListener("click", onResetPeaks);
    snapshotBtn?.removeEventListener("click", onSnapshot);
    pollSel?.removeEventListener("change", onPollChange);
  };
}
