/* Bench view (llmdil-bench-design §5.2, §7).
   renderBench() returns the shell; wireBench(root) loads prompts,
   results, backends, and models, paints, and attaches handlers.
   One section, three tabs (Run | Results | Prompts) per design
   rule 2. The sweep preview is computed client-side from the same
   expansion rules for display only — the server's expansion at
   bench_run_start is authoritative. */

import {
  benchPromptDelete, benchPromptGenerate, benchPromptGenerateCancel, benchPromptSave,
  benchPromptsList, benchResultDelete, benchResultsList, benchRunCancel, benchRunStart,
  getAppConfig, getBackends, getModels, isTauri,
} from "../api";
import type {
  BackendOffload, BackendRow, BenchArmRecord, BenchArmVariant, BenchPromptEntry, BenchSweepRecord,
  BenchSweepRequest, ModelRow,
} from "../api";
import { fmtCount, stackedBar, trim1 } from "../charts";
import { esc } from "../html";
import { icon } from "../icons";

export function renderBench(): string {
  return `<div class="topbar"><h1>Bench</h1><span class="spacer"></span><a class="btn icon-only" href="#/logs?scope=bench&follow=1" title="Logs" aria-label="Bench logs">${icon("logs")}</a></div>
  <div class="view"><div id="benchRoot"><div class="card" style="color:var(--text2)">${icon("clock")} Loading…</div></div></div>`;
}

const SELECT_STYLE = "background:var(--panel);color:var(--text);border:1px solid var(--border);border-radius:8px;padding:6px 8px;font-size:12px";
const INPUT_STYLE = "background:var(--panel);color:var(--text);border:1px solid var(--border);border-radius:8px;padding:6px 8px;font-size:12px";
const TARGET_MINUTES = 20;

function fmtDateMs(ms: number | null | undefined): string {
  if (ms == null) return "—";
  const d = new Date(ms);
  return Number.isNaN(d.getTime()) ? "—" : d.toLocaleString("en-GB", { dateStyle: "medium", timeStyle: "short" });
}

function fmtElapsed(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  const m = Math.floor(s / 60);
  return m > 0 ? `${m} min ${s % 60} s` : `${s} s`;
}

function fmtNum(v: number | null | undefined): string {
  return v == null ? "—" : trim1(v);
}

function fmtVram(mib: number | null): string {
  if (mib == null) return "—";
  return mib >= 1024 ? `${trim1(mib / 1024)} GiB` : `${fmtCount(mib)} MiB`;
}

function infoTip(text: string): string {
  return `<span class="info-tip" title="${esc(text)}">${icon("info")}</span>`;
}

function kv(label: string, value: string): string {
  return `<div class="opt-kv"><span class="opt-kv-label">${esc(label)}:</span> <span class="opt-kv-value">${esc(value)}</span></div>`;
}

/* ---------------- Arm expansion (client preview) ---------------- */

const KV_PAIRS: [string, string][] = [
  ["f16", "f16"], ["q8_0", "q8_0"], ["f16", "q8_0"], ["q4_0", "q4_0"],
  ["q8_0", "f16"], ["q4_0", "f16"], ["f16", "q4_0"], ["q8_0", "q4_0"], ["q4_0", "q8_0"],
];
const FLASH_VALUES = ["auto", "on", "off"];
const DEPTH_VALUES = [0, 8192, 30720];
const SPEC_VALUES = ["none", "ngram-mod", "draft-simple", "combined"];
const NMIN_VALUES = [4, 16, 48];

function armLabel(v: BenchArmVariant): string {
  const parts: string[] = [];
  if (v.cache_k || v.cache_v) parts.push(`KV ${v.cache_k ?? "—"}/${v.cache_v ?? "—"}`);
  if (v.flash_attn) parts.push(`FA ${v.flash_attn}`);
  if (v.depth != null) parts.push(`Depth ${fmtCount(v.depth)}`);
  if (v.n_ctx != null) parts.push(`Ctx ${fmtCount(v.n_ctx)}`);
  const spec = v.spec ?? "none";
  if (spec === "none") parts.push("No spec");
  else if (spec === "ngram-mod") parts.push(`ngram-mod n-min ${v.ngram_n_min ?? 16}`);
  else if (spec === "draft-simple") parts.push(`draft n-max ${v.draft_n_max ?? 3}`);
  else if (spec === "combined") parts.push(`combined n-min ${v.ngram_n_min ?? 16} · draft n-max ${v.draft_n_max ?? 3}`);
  if (v.offload_expr != null) parts.push(v.offload_expr === "" ? "Offload none" : `Offload ${v.offload_expr}`);
  return parts.length ? parts.join(" · ") : "Baseline configuration";
}

/** The canonical offload expression for a position (mirrors
 *  manager-bench's offload_expr_for_layers): 0 layers → no offload
 *  (the empty string, deliberately not null — the arm sets the
 *  position), all layers → full expert offload, otherwise the
 *  partial --n-cpu-moe spelling. */
function offloadExprFor(layers: number, moeLayers: number): string {
  if (layers <= 0) return "";
  if (layers >= moeLayers) return "-ot exps=CPU";
  return `--n-cpu-moe ${layers}`;
}

/** The position an offload expression names, in layers (null when
 *  the expression is a custom form the preview cannot judge). */
function offloadLayersOf(expr: string, moeLayers: number): number | null {
  const t = expr.trim();
  if (t === "") return 0;
  const ncmoe = /^(?:--n-cpu-moe|-ncmoe)[=\s](\d+)$/.exec(t);
  if (ncmoe) return Math.min(Number(ncmoe[1]), moeLayers);
  if (/^(?:--cpu-moe|-cmoe)$/.test(t)) return moeLayers;
  if (/^-ot\s+(.+)$/.test(t)) {
    const parts = t.replace(/^-ot\s+/, "").split(",").map((p) => p.trim());
    return parts.length && parts.every((p) => /^exps\s*=\s*CPU$/i.test(p)) ? moeLayers : null;
  }
  return null;
}

/** Sweep positions offered for an MoE backend: no offload,
 *  quarter steps of the MoE layer count, full offload, plus the
 *  backend's current position when it is not already one of them. */
function offloadChoices(off: BackendOffload): number[] {
  const m = off.moe_layers;
  const set = new Set<number>([0, Math.round(m / 4), Math.round(m / 2), Math.round((3 * m) / 4), m, off.layers]);
  return [...set].sort((a, b) => a - b);
}

function offloadPosLabel(n: number, off: BackendOffload): string {
  if (n === 0) return "No offload";
  if (n === off.moe_layers) return "All experts";
  return `-ncmoe ${n}`;
}

/** The client-knowable pruning rules (§3.2); the server's
 *  validation remains authoritative at run time. */
function pruneReason(v: BenchArmVariant, requestDraftModelId: string | null, off: BackendOffload | null = null): string | null {
  const cv = v.cache_v ?? null;
  if ((cv === "q8_0" || cv === "q4_0") && v.flash_attn === "off") {
    return "Quantized V cache with flash attention off — llama.cpp refuses this pairing";
  }
  const spec = v.spec ?? "none";
  if ((spec === "draft-simple" || spec === "combined") && !(v.draft_model_id ?? requestDraftModelId)) {
    return "Draft speculative kind without a draft model";
  }
  // The endpoints-only offload guardrail, mirrored for the preview:
  // a catalog pin forbids partial expert offload on the placed
  // GPU's compute capability, so a swept partial position prunes.
  // Arms without an offload variant sit at the backend's current
  // position, which is judged the same way (the bench temp carries
  // no journaled override). The server's prune is authoritative.
  if (off?.guardrail && off.guardrail.allowed === "endpoints_only"
    && off.guardrail.placed_ccs.includes(off.guardrail.compute_capability)) {
    const position = v.offload_expr != null ? offloadLayersOf(v.offload_expr, off.moe_layers) : off.layers;
    if (position != null && position > 0 && position < off.moe_layers) {
      return `Partial expert offload is forbidden on compute capability ${off.guardrail.compute_capability} — only no offload or full expert offload is permitted; nearest permitted position: ${off.layers >= off.moe_layers / 2 ? off.moe_layers : 0} of ${off.moe_layers} layers`;
    }
  }
  return null;
}

function expandDims(dims: BenchArmVariant[][]): BenchArmVariant[] {
  let arms: BenchArmVariant[] = [{}];
  for (const dim of dims) {
    if (!dim.length) continue;
    const next: BenchArmVariant[] = [];
    for (const a of arms) for (const v of dim) next.push({ ...a, ...v });
    arms = next;
  }
  return arms;
}

function presetArms(preset: "kv" | "spec" | "combined", draftModelId: string, draftNMax: number, draftDevice: string): BenchArmVariant[] {
  if (preset === "kv") {
    return [["f16", "f16"], ["q8_0", "q8_0"], ["f16", "q8_0"], ["q4_0", "q4_0"]]
      .map(([k, v]) => ({ cache_k: k, cache_v: v, depth: 8192, spec: "none" }));
  }
  if (preset === "spec") {
    const draft = draftModelId ? { draft_model_id: draftModelId } : {};
    return [
      { spec: "none", depth: 8192 },
      { spec: "ngram-mod", ngram_n_min: 16, depth: 8192 },
      { spec: "draft-simple", draft_n_max: draftNMax, draft_device: draftDevice, depth: 8192, ...draft },
      { spec: "combined", ngram_n_min: 16, draft_n_max: draftNMax, draft_device: draftDevice, depth: 8192, ...draft },
    ];
  }
  const draft = draftModelId ? { draft_model_id: draftModelId } : {};
  return [
    { spec: "none", depth: 8192 },
    { spec: "none", depth: 30720 },
    { spec: "combined", ngram_n_min: 16, draft_n_max: draftNMax, draft_device: draftDevice, depth: 8192, ...draft },
    { spec: "combined", ngram_n_min: 16, draft_n_max: draftNMax, draft_device: draftDevice, depth: 30720, ...draft },
  ];
}

/* ---------------- Status presentation ---------------- */

function sweepStatusChip(s: BenchSweepRecord): string {
  const map: Record<string, [string, string]> = {
    running: ["var(--warn)", "Running"],
    complete: ["var(--ok)", "Complete"],
    partial: ["var(--warn)", "Partial"],
    failed: ["var(--err)", "Failed"],
  };
  const [color, label] = map[s.status] ?? ["var(--text2)", s.status];
  return `<span class="chip" style="color:${color}">${esc(label)}</span>`;
}

function armStatusChip(a: BenchArmRecord): string {
  const map: Record<string, [string, string]> = {
    measured: ["var(--ok)", "Measured"],
    running: ["var(--warn)", "Running"],
    pending: ["var(--text3)", "Pending"],
    pruned: ["var(--text3)", "Pruned"],
    preflight_failed: ["var(--err)", "Preflight failed"],
    failed: ["var(--err)", "Failed"],
    cancelled: ["var(--text3)", "Cancelled"],
  };
  const [color, label] = map[a.status] ?? ["var(--text2)", a.status];
  const tip = a.detail ? ` title="${esc(a.detail)}"` : "";
  return `<span class="chip" style="color:${color}"${tip}>${esc(label)}</span>`;
}

function originChip(origin: string): string {
  const color = origin === "shipped" ? "var(--text2)" : origin === "generated" ? "var(--purple)" : "var(--accent)";
  return `<span class="chip" style="color:${color}">${esc(origin)}</span>`;
}

function depthKey(a: BenchArmRecord): string {
  return a.spec?.depth != null ? String(a.spec.depth) : "—";
}

/* ---------------- Horizontal bars chart ----------------
   Bar lengths are proportional to the values (rule 6); the exact
   figure rides the bar's label and the arms table. */

interface BarItem { label: string; value: number | null; baseline: boolean; winner: boolean; dim: boolean }

function barsChart(items: BarItem[], unit: string): string {
  const vals = items.map((i) => i.value).filter((v): v is number => v != null);
  if (!vals.length) return "";
  const max = Math.max(...vals) || 1;
  const rows = items.map((i) => {
    const pct = i.value != null ? Math.max(1.5, (i.value / max) * 100) : 0;
    const color = i.winner ? "var(--ok)" : i.baseline ? "var(--purple)" : "var(--accent)";
    return `<div style="display:flex;align-items:center;gap:8px;padding:2px 0;min-width:0">
      <span style="flex:0 1 240px;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font-size:12px;${i.dim ? "color:var(--text3)" : ""}" title="${esc(i.label)}">${esc(i.label)}${i.baseline ? ` <span class="chip">Baseline</span>` : ""}${i.winner ? ` <span class="chip" style="color:var(--ok)">Winner</span>` : ""}</span>
      <span style="flex:1;height:10px;background:#2a313c;border-radius:3px;overflow:hidden">${i.value != null ? `<i style="display:block;height:100%;width:${pct.toFixed(1)}%;background:${color}"></i>` : ""}</span>
      <span style="flex:0 0 110px;text-align:right;font-family:var(--mono);font-size:12px">${i.value != null ? `${trim1(i.value)} ${esc(unit)}` : "not measured"}</span>
    </div>`;
  }).join("");
  return `<div style="margin:8px 0">${rows}</div>`;
}

/* ---------------- Prompt-purpose inference ---------------- */

/** The purpose tag a backend's role implies: its config purpose
 *  text first, then its id, matched against coder/planner/tool. */
function inferredPurpose(backendId: string, cfgPurpose: string | undefined): string {
  const hay = `${cfgPurpose ?? ""} ${backendId}`.toLowerCase();
  if (hay.includes("cod")) return "coder";
  if (hay.includes("plan")) return "planner";
  if (hay.includes("tool")) return "tool-runner";
  return "general";
}

interface PromptDraft {
  id: string | null;
  name: string;
  purpose: string;
  body: string;
  origin: string;
  version: number;
  sha256: string;
  updatedMs: number;
  isNew: boolean;
  origName: string;
  origBody: string;
  genBackend: string | null;
  genModel: string | null;
}

export async function wireBench(root: HTMLElement): Promise<() => void> {
  const host = root.querySelector<HTMLElement>("#benchRoot");
  if (!host) return () => {};
  const hostEl: HTMLElement = host;

  let disposed = false;
  let live = isTauri();
  let loadError: string | null = null;
  let tab: "run" | "results" | "prompts" = "run";
  let statusLine = "";
  let statusErr = false;

  let prompts: BenchPromptEntry[] = [];
  let sweeps: BenchSweepRecord[] = [];
  let backends: BackendRow[] = [];
  let models: ModelRow[] = [];
  const cfgPurposes = new Map<string, string>();

  /* Run-tab form state (kept in JS so poll repaints cannot lose it). */
  let selBackend = "";
  let selPrompt = "";
  let preset: "kv" | "spec" | "combined" | "custom" = "kv";
  let temperature = 0;
  let confirm06 = false;
  let correctness: boolean | null = null;
  let baselineIdx = 0;
  let draftModelId = "";
  let draftNMax = 3;
  let draftDevice = "cpu";
  const dims = {
    kvOn: true, kvPairs: new Set<string>(["f16/f16", "q8_0/q8_0", "f16/q8_0", "q4_0/q4_0"]),
    flashOn: false, flash: new Set<string>(["auto"]),
    depthOn: true, depths: new Set<number>([8192]),
    specOn: false, specs: new Set<string>(["none"]),
    nmins: new Set<number>([16]),
    offloadOn: false, offloadPositions: new Set<number>(),
  };
  let interlock: { names: string[]; message: string; request: BenchSweepRequest } | null = null;
  let runBusy = false;

  /* Results-tab state. */
  const sweepMetric = new Map<string, "gen" | "prompt" | "ttft" | "acceptance">();
  const compareSel = new Set<string>();
  const openArmDetails = new Set<string>();
  const armed = new Set<string>();

  /* Prompts-tab state. */
  let editing: PromptDraft | null = null;
  let genPurpose = "coder";
  let genDescription = "";
  let genSize: "short" | "standard" = "standard";
  let genBackendId = "";
  let generating = false;
  let genCancelArmed = false;
  let genStart = 0;
  let genTimer: number | null = null;

  let pollTimer: number | null = null;

  const enabledBackends = (): BackendRow[] => backends.filter((b) => b.enabled !== false);
  const runningSweep = (): BenchSweepRecord | undefined => sweeps.find((s) => s.status === "running");

  function defaultCorrectness(): boolean {
    if (preset === "spec" || preset === "combined") return true;
    if (preset === "custom") return [...dims.specs].some((s) => s !== "none") && dims.specOn;
    return false;
  }

  function autoPromptFor(backendId: string): string {
    if (!prompts.length) return "";
    const tag = inferredPurpose(backendId, cfgPurposes.get(backendId));
    return (prompts.find((p) => p.purpose === tag)
      ?? prompts.find((p) => p.purpose === "general")
      ?? prompts[0]).id;
  }

  function buildDims(): BenchArmVariant[][] | null {
    if (preset !== "custom") return null;
    const out: BenchArmVariant[][] = [];
    if (dims.kvOn && dims.kvPairs.size) {
      out.push([...dims.kvPairs].map((p) => {
        const [k, v] = p.split("/");
        return { cache_k: k, cache_v: v };
      }));
    }
    if (dims.flashOn && dims.flash.size) out.push([...dims.flash].map((f) => ({ flash_attn: f })));
    if (dims.depthOn && dims.depths.size) out.push([...dims.depths].map((d) => ({ depth: d })));
    if (dims.specOn && dims.specs.size) {
      const variants: BenchArmVariant[] = [];
      for (const s of dims.specs) {
        if (s === "none") variants.push({ spec: "none" });
        else if (s === "ngram-mod") {
          for (const n of (dims.nmins.size ? [...dims.nmins] : [16])) variants.push({ spec: "ngram-mod", ngram_n_min: n });
        } else if (s === "draft-simple") {
          variants.push({ spec: "draft-simple", draft_n_max: draftNMax, draft_device: draftDevice, ...(draftModelId ? { draft_model_id: draftModelId } : {}) });
        } else if (s === "combined") {
          for (const n of (dims.nmins.size ? [...dims.nmins] : [16])) {
            variants.push({ spec: "combined", ngram_n_min: n, draft_n_max: draftNMax, draft_device: draftDevice, ...(draftModelId ? { draft_model_id: draftModelId } : {}) });
          }
        }
      }
      if (variants.length) out.push(variants);
    }
    const off = selOffload();
    if (off && dims.offloadOn && dims.offloadPositions.size) {
      out.push([...dims.offloadPositions].sort((a, b) => a - b)
        .map((n) => ({ offload_expr: offloadExprFor(n, off.moe_layers) })));
    }
    return out;
  }

  /** The selected backend's MoE offload state (null for dense
   *  backends): the Run form's Offload dimension and the preview
   *  prune mirror read it. */
  function selOffload(): BackendOffload | null {
    return backends.find((b) => b.id === selBackend)?.offload ?? null;
  }

  function previewArms(): { variant: BenchArmVariant; label: string; pruned: string | null }[] {
    const variants = preset === "custom"
      ? expandDims(buildDims() ?? [])
      : presetArms(preset, draftModelId, draftNMax, draftDevice);
    return variants.map((v) => ({ variant: v, label: armLabel(v), pruned: pruneReason(v, draftModelId || null, selOffload()) }));
  }

  function buildRequest(stopIds: string[]): BenchSweepRequest {
    const arms = previewArms();
    return {
      backend_id: selBackend,
      prompt_id: selPrompt,
      temperature,
      correctness_control: correctness ?? defaultCorrectness(),
      stop_backend_ids: stopIds,
      preset: preset === "custom" ? null : preset,
      dimensions: buildDims(),
      draft_model_id: draftModelId || null,
      baseline_arm_index: Math.min(baselineIdx, Math.max(0, arms.length - 1)),
      confirm_at_06: confirm06,
    };
  }

  function parseConflicts(message: string): string[] {
    const known = backends.map((b) => b.id).filter((id) => id && message.includes(id));
    if (known.length) return [...new Set(known)];
    const bracket = message.match(/\[(.*?)\]/);
    if (bracket) {
      return bracket[1].split(",").map((s) => s.trim().replace(/^["']|["']$/g, "")).filter(Boolean);
    }
    const quoted = [...message.matchAll(/["']([^"']+)["']/g)].map((m) => m[1]);
    return [...new Set(quoted)];
  }

  /* ---------------- Run tab ---------------- */

  function checkChip(kind: string, value: string, checked: boolean, label: string): string {
    return `<label style="display:inline-flex;align-items:center;gap:5px;border:1px solid var(--border);border-radius:6px;padding:3px 8px;font-size:12px;cursor:pointer;${checked ? "background:var(--panel2)" : ""}">
      <input type="checkbox" data-bench-check="${esc(kind)}" value="${esc(value)}"${checked ? " checked" : ""}> ${esc(label)}</label>`;
  }

  function runHtml(): string {
    const backs = enabledBackends();
    const arms = previewArms();
    const runnable = arms.filter((a) => !a.pruned).length;
    const projected = arms.length * 2.25 + 2;
    const overTarget = projected > TARGET_MINUTES;
    const showDraft = preset === "spec" || preset === "combined"
      || (preset === "custom" && dims.specOn && ([...dims.specs].includes("draft-simple") || [...dims.specs].includes("combined")));
    const showNmin = preset === "custom" && dims.specOn && ([...dims.specs].includes("ngram-mod") || [...dims.specs].includes("combined"));
    const running = runningSweep();
    const backendOpts = backs.map((b) => `<option value="${esc(b.id)}"${b.id === selBackend ? " selected" : ""}>${esc(b.id)} · ${esc(b.model_file)}</option>`).join("");
    const promptOpts = prompts.map((p) => `<option value="${esc(p.id)}"${p.id === selPrompt ? " selected" : ""}>${esc(p.name)} (${esc(p.purpose)})</option>`).join("");
    const modelOpts = `<option value="">—</option>` + models.map((m) => `<option value="${esc(m.id)}"${m.id === draftModelId ? " selected" : ""}>${esc(m.name || m.id)}</option>`).join("");

    const progressHtml = running ? `<div class="opt-row" style="margin-bottom:12px">
      <div class="opt-row-main">
        <span class="opt-consulting">${icon("clock")} Running — Arm: ${(running.progress?.arm_index ?? 0) + 1} of ${running.progress?.arm_count ?? running.arms.length} · Phase: ${esc(running.progress?.phase ?? "—")} · Run: ${running.progress?.run ?? "—"} of 3 · Elapsed: <span id="benchElapsed">${fmtElapsed(running.progress?.elapsed_ms ?? (Date.now() - running.started_ms))}</span></span>
        <span style="flex:1"></span>
        <button class="btn danger" data-bench-act="cancel-run" data-sweep="${esc(running.id)}">${icon("x")} Cancel</button>
      </div>
    </div>` : "";

    const interlockHtml = interlock ? `<div class="opt-row invalid" style="margin-bottom:12px">
      <div class="opt-row-main">
        <span class="opt-invalid">${icon("warn")} Interlock — running backends conflict with this sweep</span>
      </div>
      <div style="margin-top:6px">${interlock.names.map((n) => kv("Backend", n)).join("")}</div>
      <div class="opt-consult-actions">
        <button class="btn danger" data-bench-act="stop-and-run">${icon("pause")} Stop ${interlock.names.length} backend${interlock.names.length === 1 ? "" : "s"} and run</button>
        <button class="btn" data-bench-act="dismiss-interlock">Dismiss</button>
      </div>
    </div>` : "";

    const customHtml = preset === "custom" ? `<div style="margin-top:10px">
      <div class="opt-field-label">Dimensions</div>
      <div style="display:flex;flex-direction:column;gap:8px">
        <div>
          <label style="font-size:12px"><input type="checkbox" data-bench-dim="kvOn"${dims.kvOn ? " checked" : ""}> KV pair${infoTip("Cache types for K and V. The four decision pairs are pre-selected; individual pairs cover the rest of the 3×3 grid.")}</label>
          ${dims.kvOn ? `<div style="display:flex;flex-wrap:wrap;gap:6px;margin-top:6px">${KV_PAIRS.map(([k, v]) => checkChip("kv", `${k}/${v}`, dims.kvPairs.has(`${k}/${v}`), `${k}/${v}`)).join("")}</div>` : ""}
        </div>
        <div>
          <label style="font-size:12px"><input type="checkbox" data-bench-dim="flashOn"${dims.flashOn ? " checked" : ""}> Flash attention</label>
          ${dims.flashOn ? `<div style="display:flex;flex-wrap:wrap;gap:6px;margin-top:6px">${FLASH_VALUES.map((f) => checkChip("flash", f, dims.flash.has(f), f)).join("")}</div>` : ""}
        </div>
        <div>
          <label style="font-size:12px"><input type="checkbox" data-bench-dim="depthOn"${dims.depthOn ? " checked" : ""}> Depth${infoTip("Working-context size at which generation is measured, reached by padding with real library text.")}</label>
          ${dims.depthOn ? `<div style="display:flex;flex-wrap:wrap;gap:6px;margin-top:6px">${DEPTH_VALUES.map((d) => checkChip("depth", String(d), dims.depths.has(d), fmtCount(d))).join("")}</div>` : ""}
        </div>
        <div>
          <label style="font-size:12px"><input type="checkbox" data-bench-dim="specOn"${dims.specOn ? " checked" : ""}> Speculative kind</label>
          ${dims.specOn ? `<div style="display:flex;flex-wrap:wrap;gap:6px;margin-top:6px">${SPEC_VALUES.map((s) => checkChip("spec", s, dims.specs.has(s), s)).join("")}</div>` : ""}
          ${showNmin ? `<div style="display:flex;flex-wrap:wrap;gap:6px;align-items:center;margin-top:6px"><span style="font-size:12px;color:var(--text2)">n-min:</span>${NMIN_VALUES.map((n) => checkChip("nmin", String(n), dims.nmins.has(n), String(n))).join("")}</div>` : ""}
        </div>
        ${selOffload() ? `<div>
          <label style="font-size:12px"><input type="checkbox" data-bench-dim="offloadOn"${dims.offloadOn ? " checked" : ""}> Expert offload${infoTip("MoE expert-offload positions, in MoE layers on the CPU. The axis sweeps --n-cpu-moe values between no offload and full expert offload; positions a catalog guardrail forbids are pruned in the preview and by the server.")}</label>
          ${dims.offloadOn ? `<div style="display:flex;flex-wrap:wrap;gap:6px;margin-top:6px">${offloadChoices(selOffload()!).map((n) => checkChip("offloadpos", String(n), dims.offloadPositions.has(n), offloadPosLabel(n, selOffload()!))).join("")}</div>` : ""}
        </div>` : ""}
      </div>
    </div>` : "";

    const previewRows = arms.map((a, i) => `<div class="opt-row-main" style="padding:3px 0">
      <label style="display:inline-flex;gap:6px;align-items:center;font-size:12px">
        <input type="radio" name="benchBaseline" data-bench-baseline="${i}"${i === Math.min(baselineIdx, arms.length - 1) ? " checked" : ""}>
        <span style="font-family:var(--mono);color:var(--text3)">${i + 1}.</span>
        <span style="${a.pruned ? "text-decoration:line-through;color:var(--text3)" : ""}">${esc(a.label)}</span>
      </label>
      ${i === Math.min(baselineIdx, arms.length - 1) ? `<span class="chip">Baseline</span>` : ""}
      ${a.pruned ? `<span style="font-size:12px;color:var(--err)">${esc(a.pruned)}</span>` : ""}
    </div>`).join("");

    return `${progressHtml}${interlockHtml}
    <div class="opt-consult-grid">
      <div>
        <div class="opt-field-label">Backend</div>
        ${backs.length
          ? `<select id="benchBackendSel" style="${SELECT_STYLE};width:100%">${backendOpts}</select>`
          : `<div class="opt-note">No enabled backends</div>`}
        ${selOffload() ? `<div style="margin-top:6px">${kv("Offload", `${selOffload()!.layers} of ${selOffload()!.moe_layers} MoE layers`)}</div>` : ""}
        <div class="opt-field-label" style="margin-top:10px">Prompt</div>
        ${prompts.length
          ? `<select id="benchPromptSel" style="${SELECT_STYLE};width:100%">${promptOpts}</select>`
          : `<div class="opt-note">No prompts in the library</div>`}
        <div class="opt-field-label" style="margin-top:10px">Preset</div>
        <select id="benchPresetSel" style="${SELECT_STYLE};width:100%">
          <option value="kv"${preset === "kv" ? " selected" : ""}>KV study</option>
          <option value="spec"${preset === "spec" ? " selected" : ""}>Spec study</option>
          <option value="combined"${preset === "combined" ? " selected" : ""}>Combined study</option>
          <option value="custom"${preset === "custom" ? " selected" : ""}>Custom</option>
        </select>
        ${preset !== "custom" ? `<div style="margin-top:8px">${kv("Arms", "4")}${kv("Depth", preset === "combined" ? "8,192 · 30,720" : "8,192")}</div>` : ""}
      </div>
      <div>
        <div class="opt-field-label">Sampling</div>
        <div class="opt-consult-row">
          <label style="font-size:12px;color:var(--text2)">Temperature <input id="benchTemp" type="number" min="0" max="2" step="0.1" value="${temperature}" style="${INPUT_STYLE};width:70px"></label>
          <label style="font-size:12px"><input type="checkbox" data-bench-flag="confirm06"${confirm06 ? " checked" : ""}> Confirm at 0.6${infoTip("Adds a second pass at temperature 0.6 for the winning arm, per the screening protocol.")}</label>
        </div>
        <div class="opt-consult-row" style="margin-top:8px">
          <label style="font-size:12px"><input type="checkbox" data-bench-flag="correctness"${(correctness ?? defaultCorrectness()) ? " checked" : ""}> Correctness control${infoTip("Diffs the winning arm's temperature-0 output against the baseline arm's; a large divergence stops the sweep. Default on for spec sweeps.")}</label>
        </div>
        ${showDraft ? `<div class="opt-field-label" style="margin-top:10px">Draft</div>
        <div class="opt-consult-row" style="flex-wrap:wrap">
          <select id="benchDraftModel" style="${SELECT_STYLE}" aria-label="Draft model">${modelOpts}</select>
          <label style="font-size:12px;color:var(--text2)">n-max <select id="benchDraftNMax" style="${SELECT_STYLE}">${[2, 3, 4].map((n) => `<option value="${n}"${n === draftNMax ? " selected" : ""}>${n}</option>`).join("")}</select></label>
          <label style="font-size:12px;color:var(--text2)">Device <select id="benchDraftDevice" style="${SELECT_STYLE}">
            <option value="cpu"${draftDevice === "cpu" ? " selected" : ""}>CPU</option>
            <option value="target_gpu"${draftDevice === "target_gpu" ? " selected" : ""}>Target GPU</option>
          </select>${infoTip("At large target contexts the draft model's own KV cache can exceed 14 GiB on the GPU; the draft goes to CPU in that case.")}</label>
        </div>` : ""}
        ${customHtml}
      </div>
    </div>
    <div style="margin-top:14px;border-top:1px solid var(--border);padding-top:12px">
      <div class="opt-field-label">Sweep preview${infoTip("Client-side expansion for display; the server's expansion at run time is authoritative. The baseline radio marks the arm every delta is computed against.")}</div>
      ${previewRows || `<div class="opt-note">No arms — enable a dimension with at least one value.</div>`}
      <div class="opt-counts">Arms: ${arms.length} · Runnable: ${runnable} · Est: ~${trim1(projected)} min (estimate)</div>
      ${overTarget ? `<div style="display:flex;gap:6px;align-items:center;color:var(--warn);font-size:12px;margin-top:4px">${icon("warn")} Over the ${TARGET_MINUTES}-minute target — split by depth or by axis row into separate sweeps, or run anyway.</div>` : ""}
      <div class="opt-consult-actions">
        <button class="btn primary" data-bench-act="run" ${runBusy || running || !backs.length || !prompts.length || !arms.length ? "disabled" : ""}>${icon("play")} Run sweep</button>
        ${running ? `<span class="opt-note">A sweep is running — cancel it before starting another.</span>` : ""}
      </div>
    </div>`;
  }

  /* ---------------- Results tab ---------------- */

  type Metric = "gen" | "prompt" | "ttft" | "acceptance";

  function metricValue(a: BenchArmRecord, m: Metric): number | null {
    if (!a.summary) return null;
    if (m === "gen") return a.summary.median_gen_tps;
    if (m === "prompt") return a.summary.median_prompt_tps;
    if (m === "ttft") return a.summary.median_ttft_ms;
    return a.summary.mean_accepted_len;
  }

  function metricUnit(m: Metric): string {
    return m === "ttft" ? "ms" : m === "acceptance" ? "tok" : "tok/s";
  }

  /** Winning arm index per depth condition on the selected
   *  metric (§5.2): the highest value wins, except TTFT, where
   *  the lowest time wins. */
  function winnersByMetric(s: BenchSweepRecord, m: Metric): Map<string, number> {
    const best = new Map<string, { idx: number; val: number }>();
    for (const a of s.arms) {
      if (a.status !== "measured") continue;
      const val = metricValue(a, m);
      if (val == null) continue;
      const k = depthKey(a);
      const cur = best.get(k);
      if (!cur || (m === "ttft" ? val < cur.val : val > cur.val)) best.set(k, { idx: a.index, val });
    }
    return new Map([...best.entries()].map(([k, v]) => [k, v.idx]));
  }

  /** Accepted/generated token totals per drafter kind across an
   *  arm's runs — the per-drafter attribution the run records
   *  carry, aggregated for display. */
  function drafterTotalsOf(a: BenchArmRecord): Map<string, { gen: number; acc: number }> {
    const totals = new Map<string, { gen: number; acc: number }>();
    for (const r of a.runs ?? []) {
      for (const d of r.drafters ?? []) {
        const cur = totals.get(d.kind) ?? { gen: 0, acc: 0 };
        cur.gen += d.gen_tokens; cur.acc += d.acc_tokens;
        totals.set(d.kind, cur);
      }
    }
    return totals;
  }

  function comparabilityDiffs(a: BenchSweepRecord, b: BenchSweepRecord): string[] {
    const diffs: string[] = [];
    if (a.model.sha256 !== b.model.sha256) diffs.push("Model");
    if (a.prompt_sha256 !== b.prompt_sha256) diffs.push("Prompt");
    if (a.temperature !== b.temperature) diffs.push("Temperature");
    if (a.llama_build !== b.llama_build) diffs.push("llama build");
    return diffs;
  }

  function armDetailHtml(s: BenchSweepRecord, a: BenchArmRecord): string {
    const key = `${s.id}:${a.index}`;
    const open = openArmDetails.has(key);
    const specRows = Object.entries(a.spec ?? {})
      .filter(([, v]) => v !== null && v !== undefined && v !== "")
      .map(([k, v]) => kv(k, String(v))).join("");
    const runRows = (a.runs ?? []).map((r, i) => `<tr>
      <td>${i + 1}${r.warmup ? " (warm-up)" : ""}</td><td>${fmtNum(r.prompt_tps)}</td><td>${fmtNum(r.gen_tps)}</td>
      <td>${fmtNum(r.ttft_ms)}</td><td>${r.prompt_tokens == null ? "—" : fmtCount(r.prompt_tokens)}</td>
      <td>${r.generated_tokens == null ? "—" : fmtCount(r.generated_tokens)}</td>
      <td>${r.acceptance_rate == null ? "—" : trim1(r.acceptance_rate * 100) + " %"}</td>
      <td>${fmtNum(r.mean_accepted_len)}</td></tr>`).join("");
    const drafterTotals = drafterTotalsOf(a);
    const totalAcc = [...drafterTotals.values()].reduce((sum, d) => sum + d.acc, 0);
    const palette = ["var(--accent)", "var(--purple)", "var(--teal)", "var(--warn)"];
    const drafterBar = drafterTotals.size && totalAcc > 0
      ? `<div style="margin-top:8px"><div class="opt-field-label">Accepted tokens by drafter</div>
           ${stackedBar([...drafterTotals.entries()].map(([kind, d], i) => ({ pct: (d.acc / totalAcc) * 100, color: palette[i % palette.length], label: `${kind}: ${fmtCount(d.acc)}` })), 300, 10)}
           <div class="legend">${[...drafterTotals.entries()].map(([kind, d], i) => `<span><span class="sw" style="background:${palette[i % palette.length]}"></span>${esc(kind)}: ${fmtCount(d.acc)}</span>`).join("")}</div>
         </div>`
      : "";
    return `<details class="opt-why" data-bench-details="${esc(key)}"${open ? " open" : ""}>
      <summary>Arm ${a.index + 1} detail — ${esc(a.label)}</summary>
      <div style="margin-top:6px">
        ${specRows}
        ${a.raw_flags ? `<div class="opt-kv"><span class="opt-kv-label">Raw flags:</span> <span class="opt-kv-value">${esc(a.raw_flags)}</span> <button class="btn icon-only" data-bench-copy="${esc(a.raw_flags)}" title="Copy raw flags" aria-label="Copy raw flags">${icon("copy")}</button></div>` : ""}
        ${a.comparability_key ? kv("Comparability key", a.comparability_key) : ""}
        ${a.detail ? `<div class="opt-note">${esc(a.detail)}</div>` : ""}
        ${runRows ? `<table class="tbl" style="margin-top:8px"><thead><tr><th>Run</th><th>Prompt tok/s</th><th>Gen tok/s</th><th>TTFT ms</th><th>Prompt tok</th><th>Gen tok</th><th>Acceptance</th><th>Mean len</th></tr></thead><tbody>${runRows}</tbody></table>` : ""}
        ${drafterBar}
        <div class="card-actions">
          <a class="btn" href="#/backends" data-bench-act="open-editor" data-backend="${esc(s.backend_id)}">${icon("pencil")} Open in backend editor</a>
        </div>
      </div>
    </details>`;
  }

  function sweepCardHtml(s: BenchSweepRecord): string {
    const metric: Metric = sweepMetric.get(s.id) ?? "gen";
    const winners = winnersByMetric(s, metric);
    const baseline = s.arms[s.baseline_arm_index];
    const baselineVal = baseline ? metricValue(baseline, metric) : null;
    const hasSpec = s.arms.some((a) => a.spec?.spec && a.spec.spec !== "none");
    const delKey = `sweep:${s.id}`;
    const isArmed = armed.has(delKey);
    // The metric tabs drive the whole card — chart, winner lines,
    // and the arm table's metric columns — not the chart alone.
    const deltaPct = (v: number | null): string =>
      v == null || baselineVal == null || baselineVal === 0 ? "—"
        : `${v >= baselineVal ? "+" : ""}${trim1(((v / baselineVal) - 1) * 100)} %`;
    const chartItems: BarItem[] = s.arms.map((a) => ({
      label: a.label,
      value: metricValue(a, metric),
      baseline: a.index === s.baseline_arm_index,
      winner: winners.get(depthKey(a)) === a.index,
      dim: a.status !== "measured",
    }));
    const winnerLines = [...winners.entries()].map(([depth, idx]) => {
      const a = s.arms.find((x) => x.index === idx);
      if (!a) return "";
      const v = metricValue(a, metric);
      const delta = a.index === s.baseline_arm_index ? "the baseline arm" : `Δ vs baseline: ${deltaPct(v)}`;
      return kv(`Winner${depth !== "—" ? ` (depth ${fmtCount(Number(depth))})` : ""}`, `${a.label} · ${fmtNum(v)} ${metricUnit(metric)} · ${delta}`);
    }).join("");
    const armRows = s.arms.map((a) => {
      const delta = a.index === s.baseline_arm_index ? "Baseline" : deltaPct(metricValue(a, metric));
      const head = `<td>${esc(a.label)}${a.index === s.baseline_arm_index ? ` <span class="chip">Baseline</span>` : ""}${winners.get(depthKey(a)) === a.index ? ` <span class="chip" style="color:var(--ok)">Winner</span>` : ""}</td>
        <td>${armStatusChip(a)}</td>`;
      if (metric === "gen") {
        return `<tr>${head}
          <td>${fmtNum(a.summary?.median_gen_tps)}</td>
          <td>${fmtVram(a.peak_vram_mib)}</td>
          <td>${delta}</td></tr>`;
      }
      if (metric === "prompt") {
        return `<tr>${head}
          <td>${fmtNum(a.summary?.median_prompt_tps)}</td>
          <td>${delta}</td></tr>`;
      }
      if (metric === "ttft") {
        return `<tr>${head}
          <td>${fmtNum(a.summary?.median_ttft_ms)}</td>
          <td>${delta}</td></tr>`;
      }
      const drafters = [...drafterTotalsOf(a).entries()]
        .map(([kind, d]) => `${esc(kind)}: ${fmtCount(d.acc)}`).join(" · ");
      return `<tr>${head}
        <td>${a.summary?.mean_acceptance_rate == null ? "—" : trim1(a.summary.mean_acceptance_rate * 100) + " %"}</td>
        <td>${fmtNum(a.summary?.mean_accepted_len)}</td>
        <td>${drafters || "—"}</td></tr>`;
    }).join("");
    const METRIC_HEADS: Record<Metric, string[]> = {
      gen: ["Gen tok/s", "Peak VRAM", "Δ vs baseline"],
      prompt: ["Prompt tok/s", "Δ vs baseline"],
      ttft: ["TTFT ms", "Δ vs baseline"],
      acceptance: ["Acceptance", "Mean len", "Accepted by drafter"],
    };
    const headRow = `<tr><th>Arm</th><th>Status</th>${METRIC_HEADS[metric].map((h) => `<th>${h}</th>`).join("")}</tr>`;
    // The prompt the sweep used: the record carries its identity
    // (id, recorded SHA-256); the text comes from the library entry
    // while that entry still matches the recorded hash.
    const promptEntry = prompts.find((p) => p.id === s.prompt_id);
    const promptKey = `prompt:${s.id}`;
    const promptBodyHtml = !promptEntry
      ? `<div class="opt-note">This prompt is no longer in the library — the sweep recorded its name, id, and SHA-256 only.</div>`
      : promptEntry.sha256 !== s.prompt_sha256
        ? `<div class="opt-note">The library prompt has been edited since this sweep; the exact text used is not retained in the record (recorded SHA-256 ${esc(s.prompt_sha256.slice(0, 12))}…, current ${esc(promptEntry.sha256.slice(0, 12))}…).</div>`
        : `<div style="white-space:pre-wrap;font-family:var(--mono);font-size:11px;max-height:260px;overflow:auto">${esc(promptEntry.body)}</div>`;
    const stopped = s.interlock?.stopped_confirmed ?? [];
    const durationMin = s.ended_ms != null ? trim1((s.ended_ms - s.started_ms) / 60000) : null;
    return `<div class="opt-row" style="margin-bottom:12px">
      <div class="opt-row-main">
        <label style="font-size:12px"><input type="checkbox" data-bench-compare="${esc(s.id)}"${compareSel.has(s.id) ? " checked" : ""}> Compare</label>
        <span class="opt-target">${esc(s.backend_name)} · ${esc(s.prompt_name)} · Temp: ${trim1(s.temperature)}</span>
        ${sweepStatusChip(s)}
        <span class="opt-plan-meta">${fmtDateMs(s.started_ms)}</span>
        <span style="flex:1"></span>
        <button class="btn icon-only${isArmed ? " danger armed" : ""}" data-bench-act="delete-sweep" data-sweep="${esc(s.id)}" title="${isArmed ? "Click again to delete this sweep" : "Delete sweep"}" aria-label="Delete sweep">${icon("trash")}</button>
      </div>
      <div class="opt-counts">Arms: ${s.arms.length}${s.projected_minutes != null ? ` · Projected: ~${trim1(s.projected_minutes)} min` : ""}${durationMin != null ? ` · Actual: ${durationMin} min` : ""} · llama build: ${esc(s.llama_build)}</div>
      <div class="opt-counts">Prompt: ${esc(s.prompt_name)} · ID: ${esc(s.prompt_id)} · SHA-256: ${esc(s.prompt_sha256.slice(0, 12))}… · Tokens: ${s.prompt_tokens == null ? "—" : fmtCount(s.prompt_tokens)}</div>
      <details class="opt-why" data-bench-details="${esc(promptKey)}"${openArmDetails.has(promptKey) ? " open" : ""}><summary>Prompt text</summary><div style="margin-top:6px">${promptBodyHtml}</div></details>
      ${stopped.length ? `<div class="opt-note">Stopped: ${esc(stopped.join(", "))} <a href="#/backends">Backends</a>${infoTip("Bench never restarts backends it stopped — restart them from the Backends view.")}</div>` : ""}
      ${s.status === "running" ? `<div class="opt-row-main" style="margin-top:6px">
          <span class="opt-consulting">${icon("clock")} Arm: ${(s.progress?.arm_index ?? 0) + 1} of ${s.progress?.arm_count ?? s.arms.length} · Phase: ${esc(s.progress?.phase ?? "—")} · Run: ${s.progress?.run ?? "—"} of 3 · Elapsed: ${fmtElapsed(s.progress?.elapsed_ms ?? (Date.now() - s.started_ms))}</span>
          <button class="btn danger" data-bench-act="cancel-run" data-sweep="${esc(s.id)}">${icon("x")} Cancel</button>
        </div>` : ""}
      <div class="viewsel" role="group" aria-label="Result metric" style="margin-top:8px">
        ${(["gen", "prompt", "ttft"] as Metric[]).map((m) => `<button data-bench-metric="${esc(s.id)}:${m}" class="${metric === m ? "active" : ""}">${m === "gen" ? "Generation" : m === "prompt" ? "Prompt" : "TTFT"}</button>`).join("")}
        ${hasSpec ? `<button data-bench-metric="${esc(s.id)}:acceptance" class="${metric === "acceptance" ? "active" : ""}">Acceptance</button>` : ""}
      </div>
      <div style="font-size:11px;color:var(--text3);margin-top:4px">${esc(s.prompt_name)} · Temp: ${trim1(s.temperature)}</div>
      ${barsChart(chartItems, metricUnit(metric))}
      ${winnerLines}
      <div style="overflow-x:auto"><table class="tbl" style="margin-top:8px;min-width:760px"><thead>${headRow}</thead><tbody>${armRows}</tbody></table></div>
      ${s.arms.map((a) => armDetailHtml(s, a)).join("")}
    </div>`;
  }

  function resultsHtml(): string {
    if (!sweeps.length) return `<div class="empty">No sweeps recorded</div>`;
    const sorted = [...sweeps].sort((a, b) => b.started_ms - a.started_ms);
    let compareHtml = "";
    const sel = sorted.filter((s) => compareSel.has(s.id));
    if (sel.length === 2) {
      const diffs = comparabilityDiffs(sel[0], sel[1]);
      if (diffs.length) {
        compareHtml = `<div class="opt-row invalid" style="margin-bottom:12px"><span class="opt-invalid">${icon("warn")} Cross-sweep comparison refused — differing: ${esc(diffs.join(", "))}</span></div>`;
      } else {
        const items: BarItem[] = [];
        for (const s of sel) {
          for (const a of s.arms) {
            items.push({ label: `${s.backend_name} · ${a.label}`, value: a.summary?.median_gen_tps ?? null, baseline: a.index === s.baseline_arm_index, winner: false, dim: a.status !== "measured" });
          }
        }
        compareHtml = `<div class="opt-row" style="margin-bottom:12px">
          <div class="opt-field-label">Cross-sweep — Generation tok/s</div>
          <div style="font-size:11px;color:var(--text3)">${esc(sel[0].prompt_name)} · Temp: ${trim1(sel[0].temperature)} vs ${esc(sel[1].prompt_name)} · Temp: ${trim1(sel[1].temperature)}</div>
          ${barsChart(items, "tok/s")}
        </div>`;
      }
    } else if (sel.length > 2) {
      compareHtml = `<div class="opt-note" style="margin-bottom:8px">Compare takes exactly two sweeps — ${sel.length} selected.</div>`;
    }
    return `${compareHtml}${sorted.map(sweepCardHtml).join("")}`;
  }

  /* ---------------- Prompts tab ---------------- */

  function promptsHtml(): string {
    const genElapsed = generating ? fmtElapsed(Date.now() - genStart) : "";
    const generateHtml = `<div class="opt-row" style="margin-bottom:12px">
      <div class="opt-field-label">Generate draft${infoTip("Drafts a benchmark prompt with a running backend. The result lands in the editor unsaved; long prompts are shipped defaults or pasted text.")}</div>
      <div class="opt-consult-row" style="flex-wrap:wrap">
        <input id="benchGenPurpose" type="text" value="${esc(genPurpose)}" placeholder="Purpose" style="${INPUT_STYLE};width:130px" aria-label="Generate purpose">
        <input id="benchGenDesc" type="text" value="${esc(genDescription)}" placeholder="Description — e.g. a refactoring task over a C++ parser" style="${INPUT_STYLE};flex:1;min-width:200px" aria-label="Generate description">
        <select id="benchGenSize" style="${SELECT_STYLE}" aria-label="Generate size">
          <option value="short"${genSize === "short" ? " selected" : ""}>Short</option>
          <option value="standard"${genSize === "standard" ? " selected" : ""}>Standard</option>
        </select>
        <select id="benchGenBackend" style="${SELECT_STYLE}" aria-label="Generate backend">
          <option value=""${genBackendId === "" ? " selected" : ""}>Best running</option>
          ${backends.filter((b) => b.state === "running").map((b) => `<option value="${esc(b.id)}"${genBackendId === b.id ? " selected" : ""}>${esc(b.id)}</option>`).join("")}
        </select>
        ${generating
          ? `<span class="opt-consulting">${icon("clock")} Generating <span id="benchGenElapsed">${genElapsed}</span></span>
             <button class="btn danger" data-bench-act="generate-cancel" ${genCancelArmed ? "disabled" : ""}>${icon("x")} Cancel</button>`
          : `<button class="btn" data-bench-act="generate">${icon("bolt")} Generate draft</button>`}
      </div>
    </div>`;

    let editorHtml = "";
    if (editing) {
      const e = editing;
      const isShipped = e.origin === "shipped";
      const changedFromShipped = isShipped && (e.body !== e.origBody || e.name !== e.origName);
      editorHtml = `<div class="opt-row" style="margin-bottom:12px">
        <div class="opt-field-label">${e.isNew ? "New prompt" : "Edit prompt"} ${originChip(e.origin)}</div>
        ${e.genBackend ? `<div class="opt-note">Generated by: ${esc(e.genBackend)} · Model: ${esc(e.genModel ?? "")} — unsaved</div>` : ""}
        <div style="display:flex;flex-direction:column;gap:8px;margin-top:6px">
          <label style="font-size:12px;color:var(--text2)">Name <input id="benchPromptName" type="text" value="${esc(e.name)}" style="${INPUT_STYLE};width:100%"></label>
          <label style="font-size:12px;color:var(--text2)">Purpose <input id="benchPromptPurpose" type="text" value="${esc(e.purpose)}" style="${INPUT_STYLE};width:100%"></label>
          <label style="font-size:12px;color:var(--text2)">Body <textarea id="benchPromptBody" rows="10" style="${INPUT_STYLE};width:100%;box-sizing:border-box;font-family:inherit;resize:vertical">${esc(e.body)}</textarea></label>
        </div>
        ${!e.isNew ? `<div class="opt-counts">Chars: ${fmtCount(e.body.length)} · SHA-256: ${esc(e.sha256 || "—")}</div>` : ""}
        <div class="opt-consult-actions">
          <button class="btn primary" data-bench-act="prompt-save">${icon("check")} Save prompt</button>
          <button class="btn" data-bench-act="prompt-cancel">Cancel</button>
          ${changedFromShipped ? `<button class="btn" data-bench-act="prompt-restore">${icon("restart")} Restore shipped text</button>` : ""}
          ${changedFromShipped ? `<span class="opt-note">Saving an edited shipped entry saves it as a user entry.</span>` : ""}
        </div>
      </div>`;
    }

    const rows = prompts.map((p) => {
      const delKey = `prompt:${p.id}`;
      const isArmed = armed.has(delKey);
      return `<tr>
        <td>${esc(p.name)}</td><td>${esc(p.purpose)}</td><td>${originChip(p.origin)}</td>
        <td>${fmtCount(p.body.length)}</td><td>${fmtDateMs(p.updated_ms)}</td>
        <td style="white-space:nowrap">
          <button class="btn" data-bench-act="prompt-edit" data-id="${esc(p.id)}">${icon("pencil")} Edit</button>
          ${p.origin !== "shipped" ? `<button class="btn icon-only${isArmed ? " danger armed" : ""}" data-bench-act="prompt-delete" data-id="${esc(p.id)}" title="${isArmed ? "Click again to delete this prompt" : "Delete prompt"}" aria-label="Delete prompt ${esc(p.name)}">${icon("trash")}</button>` : ""}
        </td></tr>`;
    }).join("");
    return `${generateHtml}${editorHtml}
    <div class="opt-consult-actions" style="margin:0 0 10px">
      <button class="btn" data-bench-act="prompt-new">${icon("plus")} New prompt</button>
    </div>
    ${prompts.length
      ? `<div style="overflow-x:auto"><table class="tbl" style="min-width:640px"><thead><tr><th>Name</th><th>Purpose</th><th>Origin</th><th>Chars</th><th>Updated</th><th></th></tr></thead><tbody>${rows}</tbody></table></div>`
      : `<div class="empty">No prompts in the library</div>`}`;
  }

  /* ---------------- Paint / refresh ---------------- */

  function paint(): void {
    if (disposed) return;
    const tabBtn = (t: typeof tab, label: string) =>
      `<button data-bench-tab="${t}" class="${tab === t ? "active" : ""}">${label}</button>`;
    hostEl.innerHTML = `
    ${!live ? `<div class="banner mode-live">${icon("info")} Sample mode — Bench data needs the desktop app.</div>` : ""}
    ${loadError ? `<div class="banner">${icon("err")} Backend error — ${esc(loadError)}</div>` : ""}
    <div class="card">
      <div style="display:flex;gap:10px;align-items:center;flex-wrap:wrap;margin-bottom:12px">
        <span class="viewsel" role="group" aria-label="Bench tabs">${tabBtn("run", "Run")}${tabBtn("results", "Results")}${tabBtn("prompts", "Prompts")}</span>
        <span style="flex:1"></span>
        ${statusLine ? `<span style="font-size:12px;${statusErr ? "color:var(--err)" : "color:var(--text2)"}">${esc(statusLine)}</span>` : ""}
      </div>
      ${tab === "run" ? runHtml() : tab === "results" ? resultsHtml() : promptsHtml()}
    </div>`;
  }

  async function refreshResults(): Promise<void> {
    const r = await benchResultsList();
    if (disposed) return;
    if (r.error) { loadError = r.error; return; }
    sweeps = r.data.sweeps;
    loadError = null;
  }

  async function refreshAll(): Promise<void> {
    const [pr, rr, br, mr, cfg] = await Promise.all([
      benchPromptsList(), benchResultsList(), getBackends(), getModels(), getAppConfig(),
    ]);
    if (disposed) return;
    live = pr.live && rr.live;
    const err = pr.error ?? rr.error ?? br.error ?? mr.error;
    loadError = err ?? null;
    prompts = pr.data.prompts;
    sweeps = rr.data.sweeps;
    backends = br.data;
    models = mr.data;
    const cfgBacks = (cfg.data as { backends?: unknown } | null)?.backends;
    if (Array.isArray(cfgBacks)) {
      for (const b of cfgBacks) {
        if (b && typeof b === "object") {
          const rec = b as Record<string, unknown>;
          if (typeof rec.id === "string" && typeof rec.purpose === "string") cfgPurposes.set(rec.id, rec.purpose);
        }
      }
    }
    const backs = enabledBackends();
    if (!backs.some((b) => b.id === selBackend)) {
      selBackend = backs[0]?.id ?? "";
      const off = selOffload();
      dims.offloadPositions = new Set(off ? [off.layers] : []);
    }
    if (!prompts.some((p) => p.id === selPrompt)) selPrompt = autoPromptFor(selBackend);
    if (!models.some((m) => m.id === draftModelId)) {
      draftModelId = models.find((m) => /1\.7b|draft/i.test(`${m.id} ${m.name}`))?.id ?? "";
    }
    paint();
  }

  /* ---------------- Actions ---------------- */

  function setStatus(text: string, err = false): void {
    statusLine = text;
    statusErr = err;
  }

  async function doRun(stopIds: string[]): Promise<void> {
    if (!selBackend || !selPrompt) { setStatus("Pick a backend and a prompt first.", true); paint(); return; }
    runBusy = true;
    paint();
    const req = buildRequest(stopIds);
    const r = await benchRunStart(req);
    runBusy = false;
    if (disposed) return;
    if (r.error) {
      const names = parseConflicts(r.error);
      if (names.length && !stopIds.length) {
        interlock = { names, message: r.error, request: req };
        setStatus("");
      } else {
        interlock = null;
        setStatus(`Run failed — ${r.error}`, true);
      }
      paint();
      return;
    }
    interlock = null;
    setStatus(`Sweep started: ${r.data.sweep_id}`);
    await refreshResults();
    paint();
  }

  async function doGenerate(): Promise<void> {
    generating = true;
    genCancelArmed = false;
    genStart = Date.now();
    paint();
    if (genTimer !== null) window.clearInterval(genTimer);
    genTimer = window.setInterval(() => {
      const el = hostEl.querySelector("#benchGenElapsed");
      if (el) el.textContent = fmtElapsed(Date.now() - genStart);
    }, 1000);
    const cancelled = (): boolean => genCancelArmed;
    const r = await benchPromptGenerate(genPurpose.trim() || "general", genDescription.trim(), genSize, genBackendId || undefined);
    if (genTimer !== null) { window.clearInterval(genTimer); genTimer = null; }
    generating = false;
    if (disposed) return;
    if (r.error) {
      if (!cancelled()) setStatus(`Generate failed — ${r.error}`, true);
      else setStatus("Generation cancelled.");
      paint();
      return;
    }
    // The draft lands in the editor UNSAVED (D8); origin "generated".
    editing = {
      id: null, name: "", purpose: genPurpose.trim() || "general", body: r.data.text,
      origin: "generated", version: 1, sha256: "", updatedMs: 0,
      isNew: true, origName: "", origBody: "",
      genBackend: r.data.backend_id || null, genModel: r.data.model || null,
    };
    setStatus("");
    paint();
  }

  function slugify(name: string): string {
    const s = name.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "");
    return s || `prompt-${Date.now()}`;
  }

  async function doPromptSave(): Promise<void> {
    if (!editing) return;
    const e = editing;
    if (!e.name.trim() || !e.purpose.trim()) {
      setStatus("Name and purpose are required.", true);
      paint();
      return;
    }
    // Editing a shipped entry flips its origin to "user" when the
    // name or body changed (the server keeps shipped text restorable).
    const origin = e.origin === "shipped" && (e.name !== e.origName || e.body !== e.origBody)
      ? "user" : e.origin;
    const entry: BenchPromptEntry = {
      id: e.id ?? slugify(e.name),
      name: e.name.trim(),
      purpose: e.purpose.trim(),
      body: e.body,
      sha256: e.sha256,
      origin: origin as BenchPromptEntry["origin"],
      version: e.version,
      updated_ms: e.updatedMs,
    };
    const r = await benchPromptSave(entry);
    if (disposed) return;
    if (r.error) { setStatus(`Save failed — ${r.error}`, true); paint(); return; }
    prompts = r.data.prompts;
    editing = null; // rule 17: saving a form closes it
    setStatus(`Prompt saved: ${entry.name}`);
    paint();
  }

  /* ---------------- Events ---------------- */

  hostEl.addEventListener("click", (ev) => {
    const t = ev.target as HTMLElement;

    const tabBtnEl = t.closest<HTMLElement>("[data-bench-tab]");
    if (tabBtnEl) {
      tab = (tabBtnEl.dataset.benchTab as typeof tab) ?? "run";
      paint();
      return;
    }
    const metricBtn = t.closest<HTMLElement>("[data-bench-metric]");
    if (metricBtn) {
      const [sid, m] = (metricBtn.dataset.benchMetric ?? "").split(":");
      if (sid && m) sweepMetric.set(sid, m as Metric);
      paint();
      return;
    }
    const copyBtn = t.closest<HTMLElement>("[data-bench-copy]");
    if (copyBtn) {
      const text = copyBtn.dataset.benchCopy ?? "";
      void (async () => {
        try { await navigator.clipboard.writeText(text); setStatus("Raw flags copied."); }
        catch { setStatus("Copy failed.", true); }
        paint();
      })();
      return;
    }
    const summary = t.closest<HTMLElement>("summary");
    if (summary) {
      const det = summary.closest<HTMLElement>("[data-bench-details]");
      const key = det?.dataset.benchDetails;
      if (key) {
        if (openArmDetails.has(key)) openArmDetails.delete(key); else openArmDetails.add(key);
      }
      return;
    }

    const actEl = t.closest<HTMLElement>("[data-bench-act]");
    if (!actEl) return;
    const act = actEl.dataset.benchAct;
    switch (act) {
      case "open-editor": {
        // The Backends view consumes this key on load and opens
        // the backend's flags form (Bench design §5.2 adoption
        // path); the anchor's own href does the navigation.
        try { sessionStorage.setItem("benchEditBackend", actEl.dataset.backend ?? ""); } catch { /* storage unavailable */ }
        return;
      }
      case "run":
        void doRun([]);
        return;
      case "stop-and-run": {
        if (!interlock) return;
        const names = interlock.names;
        void doRun(names);
        return;
      }
      case "dismiss-interlock":
        interlock = null;
        paint();
        return;
      case "cancel-run": {
        const sid = actEl.dataset.sweep ?? runningSweep()?.id ?? "";
        if (!sid) return;
        void (async () => {
          const r = await benchRunCancel(sid);
          if (disposed) return;
          if (r.error) { setStatus(`Cancel failed — ${r.error}`, true); paint(); return; }
          setStatus("Sweep cancellation requested.");
          await refreshResults();
          paint();
        })();
        return;
      }
      case "delete-sweep": {
        const sid = actEl.dataset.sweep ?? "";
        if (!sid) return;
        const key = `sweep:${sid}`;
        if (!armed.has(key)) {
          armed.add(key);
          paint();
          window.setTimeout(() => { armed.delete(key); if (!disposed) paint(); }, 5000);
          return;
        }
        armed.delete(key);
        void (async () => {
          const r = await benchResultDelete(sid);
          if (disposed) return;
          if (r.error) { setStatus(`Delete failed — ${r.error}`, true); paint(); return; }
          sweeps = r.data.sweeps;
          compareSel.delete(sid);
          setStatus("Sweep deleted.");
          paint();
        })();
        return;
      }
      case "generate":
        void doGenerate();
        return;
      case "generate-cancel": {
        genCancelArmed = true;
        paint();
        void benchPromptGenerateCancel();
        return;
      }
      case "prompt-new":
        editing = {
          id: null, name: "", purpose: "general", body: "", origin: "user",
          version: 1, sha256: "", updatedMs: 0, isNew: true,
          origName: "", origBody: "", genBackend: null, genModel: null,
        };
        paint();
        return;
      case "prompt-edit": {
        const p = prompts.find((x) => x.id === actEl.dataset.id);
        if (!p) return;
        editing = {
          id: p.id, name: p.name, purpose: p.purpose, body: p.body, origin: p.origin,
          version: p.version, sha256: p.sha256, updatedMs: p.updated_ms, isNew: false,
          origName: p.name, origBody: p.body, genBackend: null, genModel: null,
        };
        paint();
        return;
      }
      case "prompt-cancel":
        editing = null;
        paint();
        return;
      case "prompt-restore":
        if (editing) { editing.body = editing.origBody; editing.name = editing.origName; }
        paint();
        return;
      case "prompt-save":
        void doPromptSave();
        return;
      case "prompt-delete": {
        const id = actEl.dataset.id ?? "";
        if (!id) return;
        const key = `prompt:${id}`;
        if (!armed.has(key)) {
          armed.add(key);
          paint();
          window.setTimeout(() => { armed.delete(key); if (!disposed) paint(); }, 5000);
          return;
        }
        armed.delete(key);
        void (async () => {
          const r = await benchPromptDelete(id);
          if (disposed) return;
          if (r.error) { setStatus(`Delete failed — ${r.error}`, true); paint(); return; }
          prompts = r.data.prompts;
          if (editing?.id === id) editing = null;
          if (selPrompt === id) selPrompt = autoPromptFor(selBackend);
          setStatus("Prompt deleted.");
          paint();
        })();
        return;
      }
    }
  });

  hostEl.addEventListener("change", (ev) => {
    const t = ev.target as HTMLElement;
    if (t.id === "benchBackendSel") {
      selBackend = (t as HTMLSelectElement).value;
      selPrompt = autoPromptFor(selBackend);
      baselineIdx = 0;
      interlock = null;
      // The offload axis re-seeds at the newly selected backend's
      // current position (its position is the natural baseline of
      // the axis); a dense backend clears it.
      const off = selOffload();
      dims.offloadPositions = new Set(off ? [off.layers] : []);
      paint();
      return;
    }
    if (t.id === "benchPromptSel") { selPrompt = (t as HTMLSelectElement).value; paint(); return; }
    if (t.id === "benchPresetSel") {
      preset = (t as HTMLSelectElement).value as typeof preset;
      baselineIdx = 0;
      correctness = null;
      interlock = null;
      paint();
      return;
    }
    if (t.id === "benchDraftModel") { draftModelId = (t as HTMLSelectElement).value; paint(); return; }
    if (t.id === "benchDraftNMax") { draftNMax = Number((t as HTMLSelectElement).value) || 3; paint(); return; }
    if (t.id === "benchDraftDevice") { draftDevice = (t as HTMLSelectElement).value; paint(); return; }
    if (t.id === "benchGenSize") { genSize = (t as HTMLSelectElement).value as "short" | "standard"; return; }
    if (t.id === "benchGenBackend") { genBackendId = (t as HTMLSelectElement).value; return; }

    const baselineRadio = t.closest<HTMLElement>("[data-bench-baseline]");
    if (baselineRadio) {
      baselineIdx = Number(baselineRadio.dataset.benchBaseline) || 0;
      paint();
      return;
    }
    const compareBox = t.closest<HTMLElement>("[data-bench-compare]");
    if (compareBox) {
      const id = compareBox.dataset.benchCompare ?? "";
      if ((t as HTMLInputElement).checked) compareSel.add(id); else compareSel.delete(id);
      paint();
      return;
    }
    const dimBox = t.closest<HTMLElement>("[data-bench-dim]");
    if (dimBox) {
      const key = dimBox.dataset.benchDim as keyof typeof dims | undefined;
      if (key && typeof dims[key] === "boolean") {
        (dims as unknown as Record<string, unknown>)[key] = (t as HTMLInputElement).checked;
        baselineIdx = 0;
        paint();
      }
      return;
    }
    const flagBox = t.closest<HTMLElement>("[data-bench-flag]");
    if (flagBox) {
      const key = flagBox.dataset.benchFlag;
      const checked = (t as HTMLInputElement).checked;
      if (key === "confirm06") confirm06 = checked;
      if (key === "correctness") correctness = checked;
      paint();
      return;
    }
    const checkChipEl = t.closest<HTMLElement>("[data-bench-check]");
    if (checkChipEl) {
      const kind = checkChipEl.dataset.benchCheck;
      const value = (t as HTMLInputElement).value;
      const checked = (t as HTMLInputElement).checked;
      if (kind === "kv") { if (checked) dims.kvPairs.add(value); else dims.kvPairs.delete(value); }
      if (kind === "flash") { if (checked) dims.flash.add(value); else dims.flash.delete(value); }
      if (kind === "depth") { const n = Number(value); if (checked) dims.depths.add(n); else dims.depths.delete(n); }
      if (kind === "spec") { if (checked) dims.specs.add(value); else dims.specs.delete(value); correctness = null; }
      if (kind === "nmin") { const n = Number(value); if (checked) dims.nmins.add(n); else dims.nmins.delete(n); }
      if (kind === "offloadpos") { const n = Number(value); if (checked) dims.offloadPositions.add(n); else dims.offloadPositions.delete(n); }
      baselineIdx = 0;
      paint();
    }
  });

  hostEl.addEventListener("input", (ev) => {
    const t = ev.target as HTMLElement;
    // Drafts sync on input so poll repaints never lose operator text.
    if (t.id === "benchTemp") { const n = Number((t as HTMLInputElement).value); if (Number.isFinite(n)) temperature = n; return; }
    if (t.id === "benchGenPurpose") { genPurpose = (t as HTMLInputElement).value; return; }
    if (t.id === "benchGenDesc") { genDescription = (t as HTMLInputElement).value; return; }
    if (!editing) return;
    if (t.id === "benchPromptName") editing.name = (t as HTMLInputElement).value;
    if (t.id === "benchPromptPurpose") editing.purpose = (t as HTMLInputElement).value;
    if (t.id === "benchPromptBody") editing.body = (t as HTMLTextAreaElement).value;
  });

  await refreshAll();
  // Poll every 2 s while any sweep is running (§ contract). A tick
  // is skipped while the operator is editing a text field, so a
  // repaint cannot drop the caret mid-edit.
  pollTimer = window.setInterval(() => {
    if (disposed || !sweeps.some((s) => s.status === "running")) return;
    const active = document.activeElement;
    if (active && hostEl.contains(active)
      && (active instanceof HTMLTextAreaElement || (active instanceof HTMLInputElement && active.type === "text"))) {
      return;
    }
    void (async () => { await refreshResults(); if (!disposed) paint(); })();
  }, 2000);

  return () => {
    disposed = true;
    if (pollTimer !== null) window.clearInterval(pollTimer);
    if (genTimer !== null) window.clearInterval(genTimer);
  };
}
