/* Secondary views — faithful static layouts from the approved mockups.
   Sample data throughout; each carries a "live data wiring pending" banner.
   The dashboard is the fully wired view; these follow as the backend
   commands grow (spec §14). */

import { mockBackends, mockDashboard, mockGpus, mockWizardSteps, GPU_COLORS } from "../mock";
import { gaugeRing, meter, multiLine, sparkline, stackedBar, walk } from "../charts";
import { icon } from "../icons";
import { esc } from "../html";
import { runWizardStep, isTauri, openUrl, markSetupComplete } from "../api";

const C = { blue: "#4a9eff", purple: "#a371f7", teal: "#39c5cf", green: "#3fb950", amber: "#d29922", red: "#f85149", grey: "#6d7686" };

function topbar(title: string, extra = ""): string {
  return `<div class="topbar"><h1>${title}</h1>
    <span class="status-pill"><span class="dot ok"></span> All systems normal</span>
    <span class="spacer"></span><span class="updated">sample data</span>${extra}</div>
    <div style="padding:12px 20px 0"><div class="banner">${icon("warn")} live data wiring pending — layout from approved mockup, sample data</div></div>`;
}

function dot(s: string): string {
  return `<span class="dot ${s === "ok" ? "ok" : s === "warn" || s === "starting" ? "warn" : s === "err" ? "err" : "idle"}"></span>`;
}

/* ---------------- GPUs ---------------- */
export function renderGpus(): string {
  const gpus = mockGpus();
  const cards = gpus.map((g) => {
    const vramPct = Math.round((g.vram_used_gb / g.vram_total_gb) * 100);
    return `<div class="card">
      <div class="gpu-head">${dot(g.status)} ${esc(g.name)} <span class="port" style="color:var(--text3);font-family:var(--mono);font-size:11px">${esc(g.pci)}</span>
        <span class="spacer" style="flex:1"></span><span style="color:var(--text3);font-size:11px;font-family:var(--mono)">${esc(g.driver)}</span></div>
      <div style="display:flex;gap:18px;justify-content:center;margin:12px 0">
        ${gaugeRing(g.util_pct, 84, C.blue, String(g.util_pct), "UTIL")}
        ${gaugeRing(vramPct, 84, C.purple, g.vram_used_gb.toFixed(1), "GB VRAM")}
      </div>
      <div style="text-align:center;font-size:11px;color:var(--text3);margin-bottom:8px">UTILIZATION&nbsp;&nbsp;&nbsp;${g.vram_used_gb.toFixed(1)} / ${g.vram_total_gb.toFixed(1)} GB</div>
      <div class="gpu-metrics">
        <div class="mrow"><span class="mlabel">Temperature</span><span class="mval">${g.temp_c} °C · peak ${g.temp_peak_c}</span></div>
        <div class="mrow"><span></span>${sparkline(g.temp_spark, 240, 26, C.amber)}</div>
        <div class="mrow"><span class="mlabel">Power</span><span class="mval">${g.power_w} / ${g.power_limit_w} W</span></div>
        <div class="mrow"><span></span>${sparkline(g.power_spark, 240, 26, C.green)}</div>
      </div>
      <table class="tbl" style="margin-top:10px"><thead><tr><th>Process</th><th>PID</th><th>VRAM</th><th>Backend</th></tr></thead>
      <tbody>${g.processes.map((p) => `<tr><td>${esc(p.process)}</td><td>${p.pid}</td><td>${p.vram_mib} MiB</td><td>${esc(p.backend)}</td></tr>`).join("")}</tbody></table>
      <div style="margin-top:8px;font-size:11px;color:var(--text3);display:grid;gap:4px">
        ${g.models.map((m) => `<div>${icon("layers")} ${esc(m.model)} · ${esc(m.layers)}</div>`).join("")}
      </div>
    </div>`;
  }).join("");

  const place = [
    { model: "qwen3.8-27b", cfg: "60/40", obs: "60/40", segs: [{ pct: 60, color: GPU_COLORS.a4000 }, { pct: 40, color: GPU_COLORS["p100-0"] }], flag: false },
    { model: "qwen3-coder-30b", cfg: "45/35/20", obs: "41/35/24", segs: [{ pct: 45, color: GPU_COLORS.a4000 }, { pct: 35, color: GPU_COLORS["p100-0"] }, { pct: 20, color: GPU_COLORS["p100-1"] }], flag: true },
    { model: "qwen2.5-coder-7b", cfg: "P100-0", obs: "P100-0", segs: [{ pct: 100, color: GPU_COLORS["p100-0"] }], flag: false },
    { model: "qwen3-8b", cfg: "P100-1", obs: "P100-1", segs: [{ pct: 100, color: GPU_COLORS["p100-1"] }], flag: false },
  ];
  const placeHtml = place.map((p) => `<div class="place-row"><span class="mname">${esc(p.model)}</span>
    <div>${stackedBar(p.segs)}</div>
    <div style="font-size:11px;font-family:var(--mono);color:${p.flag ? C.amber : "var(--text3)"}">${p.flag ? `${icon("warn")} ` : ""}${esc(p.cfg)} → ${esc(p.obs)}${p.flag ? " — flags edited?" : ""}</div></div>`).join("");

  return `${topbar("GPUs", `<button class="btn" disabled>Poll: 1s</button> <button class="btn" disabled>${icon("download")} Snapshot</button>`)}
  <div class="view"><div class="cards3">${cards}</div>
  <div class="card" style="margin-top:12px"><div class="sec-title">Placement map — configured vs observed
    <span style="flex:1"></span><button class="btn" disabled>Rebalance</button></div>${placeHtml}
    <div class="legend"><span><span class="sw" style="background:${GPU_COLORS.a4000}"></span>A4000</span>
    <span><span class="sw" style="background:${GPU_COLORS["p100-0"]}"></span>P100-0</span>
    <span><span class="sw" style="background:${GPU_COLORS["p100-1"]}"></span>P100-1</span></div></div></div>`;
}

/* ---------------- Backends ---------------- */
export function renderBackends(): string {
  const backs = mockBackends().filter((b) => b.id !== "gateway");
  const cards = backs.map((b) => {
    const ticks = Array.from({ length: 20 }, (_, i) => {
      const bad = b.id === "tool-runner" && i > 14;
      return `<span style="display:inline-block;width:8px;height:18px;border-radius:2px;background:${bad ? C.amber : C.green};margin-right:3px;opacity:${bad ? 1 : 0.85}"></span>`;
    }).join("");
    return `<div class="card">
      <h3>${dot(b.status)} ${esc(b.name)} <span class="port">:${b.port}</span>
        <span class="spacer" style="flex:1"></span><span class="chip">${esc(b.model)} · ${esc(b.quant)}</span></h3>
      <div style="display:grid;grid-template-columns:repeat(4,1fr);gap:10px;margin:12px 0">
        ${[["Throughput", `${b.throughput_toks.toFixed(1)} tok/s`], ["VRAM held", `${b.vram_held_gb.toFixed(1)} GB`],
           ["Health", `${b.health}`], ["Uptime", b.id === "tool-runner" ? "starting" : "3d 4h"]]
          .map(([k, v]) => `<div class="kv"><span class="k">${k}</span><span class="v" style="font-size:13px">${v}</span></div>`).join("")}
      </div>
      <div class="spark-wrap"><div class="cap">Tokens/s · 1h</div>${sparkline(b.spark, 300, 40, C.blue)}</div>
      <div style="margin-top:10px"><div class="cap" style="font-size:10px;color:var(--text3);margin-bottom:4px">Health checks · 20 probes</div>${ticks}</div>
      <div class="card-actions"><button class="btn" disabled>${icon("restart")} Restart</button>
        <button class="btn" disabled>${icon("logs")} Logs</button>
        <span class="chip">llama-server · cuda-12.4</span></div>
    </div>`;
  }).join("");
  return `${topbar("Backends")}
  <div class="view"><div class="grid2">${cards}</div></div>`;
}

/* ---------------- Models ---------------- */
export function renderModels(): string {
  const rows = [
    { name: "qwen3.8-27b", file: "qwen3-27b-q4_k_m.gguf", size: "16.2 GB", quant: "Q4_K_M", fit: 82, fitColor: C.green, sha: "verified", backend: "planner :8081", dl: false },
    { name: "qwen3-coder-30b", file: "qwen3-coder-30b-q4_k_m.gguf", size: "18.1 GB", quant: "Q4_K_M", fit: 91, fitColor: C.amber, sha: "verified", backend: "coder-30b :8082", dl: false },
    { name: "qwen2.5-coder-7b", file: "qwen2.5-coder-7b-q5_k_m.gguf", size: "5.2 GB", quant: "Q5_K_M", fit: 34, fitColor: C.green, sha: "verified", backend: "coder-7b :8083", dl: true },
    { name: "qwen3-8b", file: "qwen3-8b-q8_0.gguf", size: "8.6 GB", quant: "Q8_0", fit: 54, fitColor: C.green, sha: "queued", backend: "tool-runner :8084", dl: true },
  ];
  const html = rows.map((r) => `<div class="card" style="margin-bottom:12px">
    <h3>${icon("box")} ${esc(r.name)} <span class="chip">${esc(r.quant)}</span>
      <span class="spacer" style="flex:1"></span>
      ${r.sha === "verified"
        ? `<span class="chip badge-ok">${icon("shield")} sha256 verified</span>`
        : `<span class="chip badge-warn">${icon("clock")} checksum queued</span>`}</h3>
    <div style="display:grid;grid-template-columns:1fr 1fr 1fr;gap:12px;margin:12px 0;align-items:center">
      <div class="kv"><span class="k">File</span><span class="v" style="font-size:12px">${esc(r.file)}</span></div>
      <div class="kv"><span class="k">Size</span><span class="v" style="font-size:12px">${esc(r.size)}</span></div>
      <div><div class="cap" style="font-size:10px;color:var(--text3);margin-bottom:4px">VRAM fit</div>${meter(r.fit, 200, 8, r.fitColor)}</div>
    </div>
    ${r.dl ? `<div class="taskbar" style="border:1px solid var(--border);border-radius:8px;margin-bottom:10px">${icon("download")} Downloading ${esc(r.file)}<span class="bar"><i style="width:62%"></i></span><span class="mono">11.2 / 18.1 GB · 62%</span></div>` : ""}
    <div style="font-size:12px;color:var(--text2)">Backend: <span style="font-family:var(--mono)">${esc(r.backend)}</span></div>
  </div>`).join("");
  return `${topbar("Models", `<button class="btn" disabled>${icon("download")} Add model</button>`)}
  <div class="view" style="max-width:900px">${html}</div>`;
}

/* ---------------- Logs ---------------- */
const LOG_LINES: [string, string, string, string][] = [
  ["06:42:03", "info", "planner", "request done · 42 tok · ttft 210ms"],
  ["06:41:54", "info", "gateway", ":4000 · 4 groups · 0 validation errors"],
  ["06:40:11", "warn", "mxc", "mcp-gdrive reconnect 3 · backoff 30s"],
  ["06:38:02", "info", "coder-7b", "queue normalized · depth 0"],
  ["06:35:47", "error", "models", "checksum mismatch qwen2.5-coder-7b · resumed ok"],
  ["06:30:22", "warn", "models", "retry 1/3 step 4 · checksum mismatch · resumed ok"],
  ["06:18:40", "info", "models", "4/4 models verified (46.5 GB)"],
  ["06:02:15", "info", "telemetry", "nvidia-smi poll ok · 3 gpus"],
  ["05:58:31", "info", "supervisor", "llama-server :8082 health ok · 96"],
  ["05:30:00", "info", "supervisor", "health sweep · all responding"],
];

export function renderLogs(): string {
  const dist = [
    { label: "info", n: 1204, color: C.blue }, { label: "warn", n: 53, color: C.amber }, { label: "error", n: 2, color: C.red },
  ];
  const total = dist.reduce((a, d) => a + d.n, 0);
  const lines = LOG_LINES.map(([t, lvl, scope, msg]) =>
    `<div class="ln"><span class="t">${esc(t)}</span> <span class="lvl-${lvl}">${lvl.padEnd(5)}</span> <span style="color:var(--text3)">[${esc(scope)}]</span> ${esc(msg)}</div>`).join("");
  return `${topbar("Logs", `<button class="btn" disabled>${icon("download")} Export</button>`)}
  <div class="view"><div class="card" style="margin-bottom:12px"><div class="sec-title">Level distribution · 60 min</div>
    <div style="display:flex;gap:16px;align-items:center">${stackedBar(dist.map((d) => ({ pct: (d.n / total) * 100, color: d.color, label: `${d.label} ${d.n}` })))}
    <span style="font-size:11px;color:var(--text3);white-space:nowrap">${dist.map((d) => `${d.label} ${d.n.toLocaleString()}`).join(" · ")}</span></div></div>
  <div class="logstream">${lines}</div></div>`;
}

/* ---------------- Gateway ---------------- */
export function renderGateway(): string {
  const rules = [
    { rule: "planner/*", group: "planner-group", strategy: "least-busy", target: "planner :8081", lat: "212ms" },
    { rule: "coder-fast/*", group: "coder-group", strategy: "latency-based", target: "coder-7b :8083", lat: "96ms" },
    { rule: "coder/*", group: "coder-group", strategy: "simple-shuffle", target: "coder-30b :8082", lat: "188ms" },
    { rule: "tool-runner/*", group: "tools-group", strategy: "least-busy", target: "tool-runner :8084", lat: "—" },
  ];
  const cards = rules.map((r) => `<div class="card">
    <h3><span class="chip">${esc(r.rule)}</span><span style="color:var(--text3)">→</span>
      <span class="chip" style="border-color:var(--accent)">${esc(r.group)}</span></h3>
    <div style="display:flex;gap:10px;align-items:center;margin:10px 0;font-size:12px">
      <span class="chip">${esc(r.strategy)}</span><span style="color:var(--text3)">→</span>
      <span style="font-family:var(--mono)">${esc(r.target)}</span>
      <span class="spacer" style="flex:1"></span><span style="font-family:var(--mono);color:var(--text2)">p50 ${esc(r.lat)}</span>
    </div>
    <div class="spark-wrap"><div class="cap">Latency · 1h</div>${sparkline(walk(40, 150, 60, 301), 280, 34, C.teal)}</div>
  </div>`).join("");
  return `${topbar("Gateway", `<span class="chip">LiteLLM :4000</span>`)}
  <div class="view">
    <div class="card" style="margin-bottom:12px"><div class="sec-title">${icon("gateway")} Routing flow</div>
      <div style="font-family:var(--mono);font-size:12px;color:var(--text2);line-height:2">
        planner/* <span style="color:var(--text3)">→</span> <span class="chip">planner-group</span> <span style="color:var(--text3)">→</span> planner :8081<br/>
        coder-fast/* <span style="color:var(--text3)">→</span> <span class="chip">coder-group</span> <span style="color:var(--text3)">→</span> coder-7b :8083<br/>
        coder/* <span style="color:var(--text3)">→</span> <span class="chip">coder-group</span> <span style="color:var(--text3)">→</span> coder-30b :8082<br/>
        <span style="color:var(--text3)">fallback - - →</span> <span class="chip">openrouter-cloud</span> <span style="color:var(--text3)">(overflow / failover)</span>
      </div></div>
    <div class="grid2">${cards}</div></div>`;
}

/* ---------------- Settings ---------------- */
export function renderSettings(): string {
  const tiles = [
    ["Filesystem", "7 paths", "2 allowed · 5 read-only"],
    ["Network", "6 rules", "loopback :4000 :8081–8084 · deny rest"],
    ["Credentials", "3 refs", "Credential Manager · per-invocation"],
    ["UI access", "disabled", "no tool needs it"],
  ];
  return `${topbar("Settings")}
  <div class="view" style="max-width:960px">
    <div class="card" style="margin-bottom:12px"><div class="sec-title">${icon("gear")} General</div>
      <div class="kv" style="gap:10px">
        <div style="display:flex;justify-content:space-between"><span class="k">Poll interval</span><span class="v" style="font-size:12px">5s</span></div>
        <div style="display:flex;justify-content:space-between"><span class="k">Log retention</span><span class="v" style="font-size:12px">7 days</span></div>
        <div style="display:flex;justify-content:space-between"><span class="k">Model directory</span><span class="v" style="font-size:12px">D:\\llm\\models</span></div>
        <div style="display:flex;justify-content:space-between"><span class="k">llama.cpp build</span><span class="v" style="font-size:12px">b7721 · cuda-12.4</span></div>
      </div></div>
    <div class="card" style="margin-bottom:12px"><div class="sec-title">${icon("shield")} MXC policies <span class="chip badge-warn">Learning mode</span></div>
      <div class="cards3" style="grid-template-columns:repeat(4,1fr)">
        ${tiles.map(([t, a, b]) => `<div style="border:1px solid var(--border);border-radius:8px;padding:10px 12px">
          <div style="font-weight:600;font-size:12px;margin-bottom:6px">${esc(t)}</div>
          <div class="kv"><span class="v" style="font-size:12px">${esc(a)}</span><span class="k">${esc(b)}</span></div></div>`).join("")}
      </div>
      <div style="margin-top:10px;font-size:12px;color:var(--text2)">Deny: <span style="font-family:var(--mono)">.ssh · credential stores · unrelated Documents paths</span></div>
      <div class="card-actions"><button class="btn" disabled>Review learning-mode denials (3)</button>
      <button class="btn" disabled>Enforce policy</button></div></div>
    <div class="card"><div class="sec-title">${icon("wrench")} Updates & diagnostics</div>
      <div class="card-actions"><a class="btn" href="#/wizard">${icon("search")} Re-run audit</a>
      <button class="btn" disabled>Check for app updates</button></div></div>
  </div>`;
}

/* ---------------- Setup wizard ---------------- */
export function renderWizard(): string {
  const steps = mockWizardSteps();
  const done = steps.filter((s) => s.state === "done").length;
  const pct = Math.round((done / steps.length) * 100);
  const rows = steps.map((s) => {
    const stIcon = s.state === "done" ? `<span style="color:var(--ok)">${icon("check")}</span>`
      : s.state === "running" ? `<span style="color:var(--accent)">${icon("clock")}</span>`
      : s.state === "failed" ? `<span style="color:var(--err)">${icon("err")}</span>`
      : `<span style="color:var(--text3)">○</span>`;
    const stateTxt = s.state === "done" ? `done · ${esc(s.duration ?? "")}` : s.state === "running" ? esc(s.note ?? "running…") : esc(s.state);
    const helpBtn = s.state === "failed" && s.helpUrl
      ? `<div style="margin-top:6px"><button class="btn" data-help-url="${esc(s.helpUrl)}">Open driver download</button></div>` : "";
    return `<div class="step ${s.state}">${stIcon}${icon("wrench")}
      <div><div class="sname">${esc(s.name)}</div><div class="sdetail">${esc(s.detail)}</div>${helpBtn}</div>
      <div class="sstate">${s.note && s.state === "done" ? `<span class="chip badge-warn">${esc(s.note)}</span> ` : ""}${stateTxt}</div></div>`;
  }).join("");
  return `<div class="topbar"><h1>Setup</h1><span class="spacer"></span><a class="btn" href="#/dashboard">Back to dashboard</a></div>
  <div class="view"><div class="banner">${icon("warn")} live data wiring pending — wizard step execution via run_wizard_step when under Tauri</div>
  <div class="wiz">
    <div class="wiz-head">${icon("wrench")}<h2>Set up Local LLM Service</h2><span class="spacer" style="flex:1"></span>
      <button class="btn" id="auditBtn">${icon("search")} Re-run audit</button></div>
    <div class="wiz-progress"><b>${done}</b><span style="color:var(--text3);font-size:12px">of ${steps.length} steps · ${pct}%</span>
      <div class="segbar">${steps.map((s) => `<i class="${s.state === "done" ? "done" : s.state === "running" ? "run" : ""}"></i>`).join("")}</div>
      <span class="updated">14m 09s elapsed · ~4m left</span></div>
    <div class="wiz-body">
      <div class="wiz-side"><p>Steps run in order. A failed step pauses the run — retry it to continue.</p>
        <div style="margin-top:14px;display:grid;gap:6px">
          <div><span style="font-family:var(--mono)">D:\\llm\\</span> · 46.5 GB</div>
          <div>${icon("shield")} checksums verified per file</div>
          <div>${icon("server")} services start at step 5</div></div></div>
      <div class="wiz-steps"><div class="sec-title">Setup checklist <span style="color:var(--text3);font-weight:400">— runs automatically, in order</span></div>${rows}</div>
    </div>
    <div class="wiz-foot"><button class="btn" disabled>Back</button><span class="spacer"></span>
      <button class="btn" disabled>Pause</button><button class="btn primary" id="finishBtn">Finish</button></div>
    <div class="logstream" style="border:0;border-top:1px solid var(--border);border-radius:0">
      <div class="ln"><span class="t">06:42:03</span> <span class="lvl-info">step 6</span> running: installing Continue.dev 1.4.2…</div>
      <div class="ln"><span class="t">06:41:54</span> <span class="lvl-info">ok</span> step 5: gateway :4000 — 4 groups · 0 validation errors</div>
      <div class="ln"><span class="t">06:30:22</span> <span class="lvl-warn">retry 1/3</span> step 4: checksum mismatch qwen2.5-coder-7b — resumed ok</div>
      <div class="ln"><span class="t">06:18:40</span> <span class="lvl-info">ok</span> step 4: 4/4 models verified (46.5 GB)</div>
    </div>
  </div></div>`;
}

export function wireWizard(root: HTMLElement): void {
  const audit = root.querySelector("#auditBtn");
  audit?.addEventListener("click", async () => {
    if (!isTauri()) { alert("Sample mode: audit would re-run all checks here."); return; }
    try {
      const r = await runWizardStep("audit");
      alert(`Audit accepted: ${r.data.task_id}`);
    } catch (e) { alert(`run_wizard_step failed: ${String(e)}`); }
  });
  root.querySelector("#finishBtn")?.addEventListener("click", async () => {
    if (isTauri()) {
      const r = await markSetupComplete();
      if (r.error) { alert(`Could not mark setup complete: ${r.error}`); return; }
    }
    location.hash = "#/dashboard";
  });
  root.addEventListener("click", async (ev) => {
    const btn = (ev.target as HTMLElement).closest("[data-help-url]") as HTMLElement | null;
    if (!btn) return;
    const url = btn.getAttribute("data-help-url") ?? "";
    btn.setAttribute("disabled", "");
    const r = await openUrl(url);
    btn.removeAttribute("disabled");
    if (r.error) alert(`Could not open the download page: ${r.error}`);
  });
}

/* ---------------- Throughput chart data for dashboard (shared) ---------------- */
export function throughputSeriesFor(names: string[]): { name: string; color: string; values: number[] }[] {
  const colors = [C.blue, C.purple, C.teal, C.grey];
  return names.map((n, i) => ({ name: n, color: colors[i % colors.length], values: walk(48, 10 + i * 8, 6, 500 + i) }));
}

export { multiLine };
