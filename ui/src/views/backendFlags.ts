/* Backend launch-flag editors: context/batch, offload, MTP, MoE cache,
   parallel slots, KV cache, raw flags and predicted throughput
   (split from static.ts). */

import type { BackendMoeCache, BackendMtp, BackendOffload, CachePreview, CacheSelectionLike, CacheTypeName, FlashAttnName, PlacementProjectionData, PredictedThroughput } from "../api";
import { fmtCount, fmtGb, trim1 } from "../charts";
import { esc } from "../html";
import { icon } from "../icons";
import { C, SELECT_STYLE } from "./common";

/** Context-length presets offered by the flags editor (tokens). */
const CTX_PRESETS = [8192, 16384, 32768, 65536, 131072, 262144, 524288, 1048576];
/** Batch-size presets offered by the flags editor. */
const BATCH_PRESETS = [128, 256, 512, 1024, 2048, 4096];
/** Parallel-slot presets offered by the flags editor (tracker #61). */
const PARALLEL_PRESETS = [1, 2, 4, 8];
/** The Parallel slots control's range — the same bounds
 *  backend_set_parallel validates (manager_models::parallel). */
export const PARALLEL_MIN = 1;
export const PARALLEL_MAX = 1024;
/** The context length's range, as backend_update_flags validates it. */
export const CTX_MIN = 1024;
export const CTX_MAX = 1048576;
/** The slot count llama.cpp's `--parallel` default (auto, -1)
 *  initializes on the pinned build, banner-verified — see
 *  manager_models::parallel. The unset field state names it so
 *  the effective count is visible rather than a bare "auto", and
 *  it seeds the Custom input when the operator leaves the
 *  default to type a count. */
const PARALLEL_LLAMA_AUTO_SLOTS = 4;

/** 131072 -> "128K"; values under 1024 stay plain. The quotient
 *  renders through trim1: a non-power-of-two context (10000)
 *  reads "9.8K", never the raw 9.765625K. */
function ctxShort(n: number): string {
  return n >= 1024 ? `${trim1(n / 1024)}K` : String(n);
}

/** Expert-offload weight figures for a slider position, priced
 *  with the same byte arithmetic as the backend memory requirement
 *  (manager_models::memory_requirement): the offloaded share of
 *  the routed experts sits in system RAM, the remaining weights
 *  on the GPU. Weights only — the KV cache is offload-independent
 *  and shown on the card. */
export function offloadFigures(o: BackendOffload, layers: number): { gpuMib: number; sysMib: number } {
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
export function offloadIsUntested(o: BackendOffload, layers: number): boolean {
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
  const base = "Routed-expert weights for the first N MoE layers run from system RAM instead of the GPU, freeing GPU memory at some speed cost. Green is this model's catalog default \u2014 the most offload that stays fast; past it, CPU expert computation increasingly dominates generation speed. Figures are weights only; the KV cache stays on the GPU. Applies on the backend's next start.";
  if (!offloadIsUntested(o, layers) || !o.guardrail) return base;
  return `${base} This position is outside the range verified for this model's GPU architecture (compute capability ${o.guardrail.compute_capability}): partial offload positions are not verified there, and saving one needs an explicit override while starts without one are refused.`;
}

/** The flag tokens a slider position writes — the canonical forms
 *  the app itself writes (backend_set_offload). */
export function offloadTokenText(o: BackendOffload, layers: number): string {
  if (layers <= 0) return "no offload tokens";
  if (layers >= o.moe_layers) return "-ot exps=CPU";
  // The position counts MoE layers; the flag value is the backend's
  // block-index bound for it (they differ past leading dense blocks).
  return `--n-cpu-moe ${o.n_cpu_moe_by_position?.[layers] ?? layers}`;
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
export const MTP_POSITIONS = [0, 1, 2, 3, 4];

/** The control value a saved MTP state implies: 0 when off;
 *  the explicit length when set; llama.cpp's default when MTP is
 *  on without one. */
export function mtpPosition(m: BackendMtp): number {
  if (!m.enabled) return 0;
  return m.n_max ?? MTP_LLAMA_N_MAX_DEFAULT;
}

/** The flag form a chosen value writes (the readout once a
 *  value is chosen, and the saved message after a commit). */
export function mtpTokenText(pos: number): string {
  return pos === 0 ? "Speculative decoding off" : `--spec-type draft-mtp --spec-draft-n-max ${pos}`;
}

/** The readout for the SAVED state, before any change: a backend
 *  that enables MTP without an explicit length runs at
 *  llama.cpp's default, and the readout says so rather than
 *  implying a saved value. */
export function mtpSavedTokenText(m: BackendMtp): string {
  if (!m.enabled) return "Speculative decoding off";
  if (m.n_max == null) return `--spec-type draft-mtp (draft length: llama.cpp default, ${MTP_LLAMA_N_MAX_DEFAULT})`;
  return mtpTokenText(m.n_max);
}

/** The expert-cache size presets the control offers (MiB) —
 *  the server module's MOE_CACHE_PRESETS_MIB. */
export const MOE_CACHE_PRESETS_MIB = [2048, 4096, 6144, 8192, 10240];

/** The size a fresh enable commits when nothing is saved or
 *  drafted — the server module's MOE_CACHE_DEFAULT_MIB. */
export const MOE_CACHE_DEFAULT_MIB = 4096;

/** The flag form a chosen state writes (the readout once a
 *  value is chosen, and the saved message after a commit). */
export function moeCacheTokenText(mib: number | null): string {
  return mib == null ? "Expert cache off" : `--moe-cache-mib ${mib}`;
}

/** The readout for the SAVED state, before any change. */
export function moeCacheSavedTokenText(m: BackendMoeCache): string {
  return moeCacheTokenText(m.mib);
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
  return `Context ${fmtCount(inp.n_ctx)} · Batch ${fmtCount(inp.n_batch)} · ${gpus}`;
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
 *  date, source, placement, and environment on one line.
 *  Exported for the Groups page's compact cards (§8.2), which
 *  carry the same figure under the same rules. */
export function predictedCardTip(p: PredictedThroughput): string {
  const parts: string[] = [];
  if (p.prompt_per_second !== null) parts.push(`Prompt: ${trim1(p.prompt_per_second)} tok/s`);
  if (p.ttft_ms !== null) parts.push(`TTFT: ${trim1(p.ttft_ms / 1000)} s`);
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
export function predictedLineHtml(exact: PredictedThroughput | null, near: PredictedThroughput | null): string {
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
 *  single slot; the MTP control surfaces any other setting — auto
 *  included, which starts several slots — and never rewrites it: the
 *  Parallel slots field in the same form is where the operator sets
 *  it (tracker #61), and the line says so. */
export function mtpConflictText(parallel: number | null): string {
  return `Parallel slots: ${parallel === null ? `auto (${PARALLEL_LLAMA_AUTO_SLOTS})` : parallel} — MTP drafting needs --parallel 1; set Parallel slots to 1 in the Edit form — the next start will not draft until then`;
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
  ["--cache-type-k", "--cache-type-k", false, V("any")], ["-ctk", "--cache-type-k", false, V("any")],
  ["--cache-type-v", "--cache-type-v", false, V("any")], ["-ctv", "--cache-type-v", false, V("any")],
  ["--flash-attn", "--flash-attn", false, V("any")], ["-fa", "--flash-attn", false, V("any")],
  ["--moe-cache-mib", "--moe-cache-mib", false, V("int")],
  ["--fit", "--fit", false, V("any")], ["-fit", "--fit", false, V("any")],
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
  // "auto" and "all" are llama.cpp's own spellings beside a count.
  ["--gpu-layers", "--gpu-layers", false, V("any")], ["--n-gpu-layers", "--gpu-layers", false, V("any")], ["-ngl", "--gpu-layers", false, V("any")],
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
export function flagsRawWarningsHtml(warnings: FlagWarningLike[]): string {
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
export function flagsCtxBatchHtml(o: {
  ctx: number;
  batch: number;
  /** The Custom… choice, explicit: it stays chosen while the number
   *  equals a preset. */
  ctxCustom: boolean;
  batchCustom: boolean;
  ctxMax: number | null;
  fieldAttr: string;
  disabled: boolean;
}): string {
  const dis = o.disabled ? " disabled" : "";
  const ctxC = o.ctxCustom || !CTX_PRESETS.includes(o.ctx);
  const batchC = o.batchCustom || !BATCH_PRESETS.includes(o.batch);
  return `<div style="display:grid;grid-template-columns:1fr 1fr;gap:10px;margin-top:8px">
        <label style="font-size:12px;color:var(--text2)">Context length (-c)
          <span style="display:flex;gap:6px;margin-top:4px">
            <select ${o.fieldAttr}="ctx-preset" style="${SELECT_STYLE};flex:1;min-width:0"${dis}>
              ${CTX_PRESETS.map((v) => `<option value="${v}"${!ctxC && v === o.ctx ? " selected" : ""}>${fmtCount(v)} (${ctxShort(v)})${v === 32768 ? " \u2014 default" : ""}${o.ctxMax && v > o.ctxMax ? " \u2014 beyond rated" : ""}</option>`).join("")}
              <option value="custom"${ctxC ? " selected" : ""}>Custom\u2026</option>
            </select>
            ${!ctxC ? "" : `<input ${o.fieldAttr}="ctx-custom" type="number" value="${Number.isFinite(o.ctx) ? o.ctx : ""}" min="${CTX_MIN}" max="${CTX_MAX}" aria-label="Custom context length" style="${SELECT_STYLE};width:110px"${dis}>`}
          </span></label>
        <label style="font-size:12px;color:var(--text2)">Batch size (-b)
          <span style="display:flex;gap:6px;margin-top:4px">
            <select ${o.fieldAttr}="batch-preset" style="${SELECT_STYLE};flex:1;min-width:0"${dis}>
              ${BATCH_PRESETS.map((v) => `<option value="${v}"${!batchC && v === o.batch ? " selected" : ""}>${fmtCount(v)}${v === 512 ? " \u2014 default" : ""}</option>`).join("")}
              <option value="custom"${batchC ? " selected" : ""}>Custom\u2026</option>
            </select>
            ${!batchC ? "" : `<input ${o.fieldAttr}="batch-custom" type="number" value="${Number.isFinite(o.batch) ? o.batch : ""}" min="1" aria-label="Custom batch size" style="${SELECT_STYLE};width:110px"${dis}>`}
          </span></label>
      </div>`;
}

/** The flag form the Parallel slots selection writes: the
 *  canonical token for an explicit count, or the default's
 *  description when unset. */
export function parallelTokenText(value: number | null): string {
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
export function flagsParallelHtml(o: {
  value: number | null;
  /** The Custom… choice, explicit (see flagsCtxBatchHtml). */
  custom: boolean;
  savedCustom: boolean;
  fieldAttr: string;
  disabled: boolean;
}): string {
  const dis = o.disabled ? " disabled" : "";
  const v = o.value;
  const isCustom = v !== null && (o.custom || !PARALLEL_PRESETS.includes(v));
  return `<div style="display:grid;grid-template-columns:1fr 1fr;gap:10px;margin-top:8px;align-items:end">
        <label style="font-size:12px;color:var(--text2)">Parallel slots (--parallel)<span class="info-tip" title="Request slots the server runs concurrently; each slot is an independent sequence with its own share of the work. An explicit count divides the context length across the slots; Auto (llama.cpp's default) keeps one unified KV cache at the full context. 1 suits a single request stream, and MTP drafting needs 1.">${icon("info")}</span>
          <span style="display:flex;gap:6px;margin-top:4px">
            <select ${o.fieldAttr}="parallel-preset" style="${SELECT_STYLE};flex:1;min-width:0"${dis}>
              <option value="default"${v === null ? " selected" : ""}>Auto \u2014 llama.cpp default</option>
              ${PARALLEL_PRESETS.map((p) => `<option value="${p}"${!isCustom && p === v ? " selected" : ""}>${p}</option>`).join("")}
              <option value="custom"${isCustom ? " selected" : ""}>Custom\u2026</option>
            </select>
            ${isCustom ? `<input ${o.fieldAttr}="parallel-custom" type="number" value="${Number.isFinite(v) ? v : ""}" aria-label="Custom parallel slots" min="${PARALLEL_MIN}" max="${PARALLEL_MAX}" style="${SELECT_STYLE};width:110px"${dis}>` : ""}
          </span></label>
        <div style="font-size:12px;color:var(--text2)"><span style="font-family:var(--mono)">${esc(parallelTokenText(v))}</span>${o.savedCustom ? `<span title="The saved flags spell parallel slots in a form the control does not write (an -np or = spelling, repeated tokens, ...). Saving from the field writes the canonical --parallel N; every other flag is preserved."> \u00b7 custom flags</span>` : ""}</div>
      </div>`;
}

/* ---------------- KV cache / flash attention ----------------
 * The cache controls: K and V cache type selects plus the
 * flash-attention tri-state, committing through
 * backend_set_cache (edit form) or carried by backend_add (add
 * form). State arrives decoded server-side (BackendCache /
 * CachePreview) — the TS side never parses raw flags for these.
 * The predicted footprint and the predicted-best ladder come
 * from the preview payload, priced server-side by the same
 * requirement model as the start gate. */
export const CACHE_TYPES: CacheTypeName[] = ["f16", "q8_0", "q4_0"];
export const FLASH_ATTN_MODES: FlashAttnName[] = ["auto", "on", "off"];

/** The flag tokens a cache selection writes — the canonical
 *  forms the app itself writes (backend_set_cache). */
export function cacheTokenText(sel: CacheSelectionLike): string {
  return `--cache-type-k ${sel.k} --cache-type-v ${sel.v} --flash-attn ${sel.flash_attn}`;
}

/** A GPU's figures for pricing a placement draft. */
export interface RepriceGpu { gpu: number; total_mib: number; free_mib: number | null; wddm: boolean }

/** The preview with the form's other drafts priced in: backend_cache_
 *  preview prices the SAVED offload position and placement, so a moved
 *  MoE slider (`weightsMib`, the GPU-resident weights at the draft
 *  position) and a changed GPU selection (`gpus`: an even split, what
 *  saving a new selection writes) are applied here — as is an expert-cache draft (`moeCacheMib`)
 *  where the server priced none — under the same
 *  rules (required against total VRAM; over total pages on WDDM). */
export function repriceCachePreview(
  p: CachePreview,
  d: { weightsMib?: number; moeCacheMib?: number; gpus?: RepriceGpu[] },
): CachePreview {
  const dWeights = d.weightsMib !== undefined ? d.weightsMib - p.weights_mib : 0;
  const delta = dWeights + (d.moeCacheMib !== undefined ? d.moeCacheMib - p.moe_cache_mib : 0);
  if (!delta && !dWeights && !d.gpus) return p;
  const total = p.total_mib + delta;
  const old = p.targets ?? [];
  const oldSum = old.reduce((a, t) => a + (t.required_mib ?? 0), 0);
  const targets = (d.gpus ?? old).map((g) => {
    const prev = old.find((t) => t.gpu === g.gpu);
    const wddm = "wddm" in g ? g.wddm : g.driver_model === "wddm";
    const free = g.free_mib ?? prev?.free_mib ?? null;
    const share = d.gpus ? 1 / d.gpus.length
      : oldSum > 0 && prev?.required_mib != null ? prev.required_mib / oldSum : null;
    const required = share === null ? null : Math.ceil(total * share);
    const over = required === null ? 0 : Math.max(0, required - g.total_mib);
    return {
      gpu: g.gpu, total_mib: g.total_mib, required_mib: required,
      fits: required === null ? null : required <= (g.total_mib * (100 - p.headroom_reserve_percent)) / 100,
      driver_model: prev?.driver_model ?? (wddm ? "wddm" as const : null),
      fit_state: required === null ? null : !over ? "fits" as const : wddm ? "wddm_paged" as const : "does_not_fit" as const,
      paged_mib: over && wddm ? over : 0,
      free_mib: free,
      fits_free: required === null || free === null ? null : required <= free,
    };
  });
  const shares = targets.map((t) => (t.required_mib ?? 0) / Math.max(1, total));
  const rungs = p.ladder.map((r) => {
    const rt = r.total_mib + delta;
    const fits = targets.length > 0 && targets.every((t, i) => rt * shares[i] <= (t.total_mib * (100 - p.headroom_reserve_percent)) / 100);
    return { ...r, total_mib: rt, fits };
  });
  return {
    ...p,
    weights_mib: p.weights_mib + dWeights,
    sys_weights_mib: Math.max(0, p.sys_weights_mib - dWeights),
    moe_cache_mib: d.moeCacheMib ?? p.moe_cache_mib,
    total_mib: total,
    targets,
    fits_current: targets.length ? targets.every((t) => t.fits === true) : null,
    fits_now: targets.length && targets.every((t) => t.fits_free !== null) ? targets.every((t) => t.fits_free === true) : null,
    ladder: rungs,
    best: rungs.find((r) => r.fits) ?? null,
  };
}

/** The one fit chip, honest about what fits NOW: "Fits" only within
 *  free memory; paging states its amount and never reads as a fit;
 *  a requirement within total but over free memory names both
 *  figures. Empty when nothing can be judged. */
export function fitChipHtml(f: {
  state: "fits" | "wddm_paged" | "does_not_fit" | null | undefined;
  pagedMib?: number | null;
  /** Null/undefined = unknown: the chip then claims total VRAM only. */
  fitsNow?: boolean | null;
  neededMib?: number | null;
  freeMib?: number | null;
  /** The GPU the figures are for. */
  gpu?: number | null;
}): string {
  const gib = (mib: number): string => fmtGb(mib / 1024);
  if (!f.state) return "";
  if (f.state === "wddm_paged") {
    return `<span class="chip status-tag status-transition" title="The requirement exceeds total VRAM on a WDDM GPU; the excess pages to system RAM and the start asks for confirmation.">Will page ${gib(f.pagedMib ?? 0)} GiB</span>`;
  }
  if (f.state === "does_not_fit") {
    return `<span class="chip status-tag status-bad" title="The requirement exceeds the placed GPUs' total VRAM.">Does not fit</span>`;
  }
  if (f.fitsNow === false) {
    const figs = f.neededMib != null && f.freeMib != null ? `${f.gpu != null ? ` on GPU ${f.gpu}` : ""}: needs ${gib(f.neededMib)} GiB, free ${gib(f.freeMib)} GiB` : "";
    return `<span class="chip status-tag status-transition" title="The requirement is within total VRAM, but other processes hold too much of it at the moment.">Not enough free now${figs}</span>`;
  }
  if (f.fitsNow === true) return `<span class="chip status-tag status-on" title="The requirement fits the memory that is free now.">Fits</span>`;
  return `<span class="chip status-tag status-off" title="Within total VRAM. Free memory is not measured.">Within total VRAM</span>`;
}

/** Whether the selection is the pairing llama.cpp refuses: a
 *  quantized V cache with flash attention off. The set op
 *  refuses it too; the form names it before committing. */
export function cacheVQuantizedFaOff(sel: CacheSelectionLike): boolean {
  return sel.v !== "f16" && sel.flash_attn === "off";
}

export function sameCacheSelection(a: CacheSelectionLike, b: CacheSelectionLike): boolean {
  return a.k === b.k && a.v === b.v && a.flash_attn === b.flash_attn;
}

/** The predicted-footprint block under the cache controls: the
 *  current selection's footprint as structured Label: Value
 *  rows — Weights, KV cache and State cache (each present only
 *  when its figure exists; an unknown figure renders no row,
 *  never a zero), the emphasized Total, then one capacity row
 *  per placed GPU carrying that GPU's required share of its
 *  total VRAM and its verdict as a status chip — and below it
 *  the predicted-best rung: as a button when it differs from
 *  the current selection (the host wires the attribute), as
 *  text when it is the current selection, and as the honest
 *  no-fit statement (with the lowest rung's numbers) when no
 *  rung fits at the chosen context. Numbers only; explanation
 *  in the tooltip. Shared by the Edit Flags form and the Add
 *  Backend form (via flagsCacheHtml): one renderer, two hosts,
 *  so the forms cannot diverge. */
export function cachePreviewHtml(preview: CachePreview | null, sel: CacheSelectionLike, bestAttr: string): string {
  if (!preview) return "";
  const gib = (mib: number): string => fmtGb(mib / 1024);
  const row = (label: string, value: string, extra = ""): string => `<div>${label}: ${value}${extra}</div>`;
  if (preview.kv_mib === null) {
    // KV geometry unknown: nothing is priced beyond the weights
    // — no KV row, no total, no capacity verdict (a total priced
    // without KV would understate the footprint, and a fit claim
    // on it would imply a measurement that does not exist).
    return `<div style="font-size:12px;color:var(--text2);margin-top:6px">${row("Weights", `${gib(preview.weights_mib)} GiB`, `<span class="info-tip" title="This model's KV geometry is not known, so no KV figure or predicted-best rung can be priced for it. The cache types still apply at launch.">${icon("info")}</span>`)}</div>`;
  }
  const rows = [
    row("Weights", `${gib(preview.weights_mib)} GiB`),
    row("KV cache", `${gib(preview.kv_mib)} GiB`),
  ];
  if (preview.rs_mib !== null && preview.rs_mib !== undefined) {
    rows.push(row("State cache", `${gib(preview.rs_mib)} GiB`));
  }
  // The expert cache is itemized whenever one is priced, so the
  // rows sum to the Total (before Oct 23, 2026 the cache rode the
  // total silently and the itemized rows fell short by exactly
  // its size).
  if (preview.moe_cache_mib) {
    rows.push(row("Expert cache", `${gib(preview.moe_cache_mib)} GiB`));
  }
  rows.push(`<div style="font-weight:600;color:var(--text)">Total: ${gib(preview.total_mib)} GiB<span class="info-tip" title="Predicted footprint of the current selection, priced by the same requirement model as the start gate: GPU weights at the current offload, KV at the selected cache types and the form's context, and the recurrent-state cache when the model carries one. A selection fits when its per-GPU total is within ${preview.headroom_reserve_percent}% headroom of the target's VRAM — the reserve covers the compute buffers the requirement model does not price. Predicted best is the highest-quality rung that fits: f16, then q8_0/q8_0, then q8_0/q4_0 (V degrades before K), then q4_0/q4_0, flash attention on at every rung.">${icon("info")}</span></div>`);
  for (const t of preview.targets ?? []) {
    const state = t.fit_state ?? (t.fits === true ? "fits" : t.fits === false ? "does_not_fit" : null);
    const chip = ` ${fitChipHtml({ state, pagedMib: t.paged_mib, fitsNow: t.fits_free ?? null, neededMib: t.required_mib, freeMib: t.free_mib })}`;
    const value = t.required_mib === null || t.required_mib === undefined
      ? `${gib(t.total_mib)} GiB`
      : `${gib(t.required_mib)} GiB of ${gib(t.total_mib)} GiB`;
    rows.push(row(`GPU ${t.gpu}`, value, chip));
  }
  let bestHtml = "";
  const best = preview.best;
  if (best && !sameCacheSelection(best, sel)) {
    bestHtml = `<div style="margin-top:6px"><button class="btn" ${bestAttr}>${icon("check")} Use predicted best: ${esc(`${best.k}/${best.v} · FA ${best.flash_attn}`)} (${gib(best.total_mib)} GiB)</button></div>`;
  } else if (best) {
    bestHtml = `<div style="font-size:12px;color:var(--text2);margin-top:6px">Predicted best: this selection</div>`;
  } else if (preview.ladder.length) {
    const lowest = preview.ladder[preview.ladder.length - 1];
    bestHtml = `<div style="font-size:12px;color:#e3b341;margin-top:6px">No cache selection fits at this context — lowest: ${esc(`${lowest.k}/${lowest.v}`)} at ${gib(lowest.total_mib)} GiB</div>`;
  }
  return `<div style="font-size:12px;color:var(--text2);margin-top:6px">${rows.join("")}</div>${bestHtml}`;
}

/** The cache controls block — three selects in the ctx/batch
 *  idiom plus the token readout and the predicted-footprint
 *  slot — shared between the Edit Flags form and the Add backend
 *  form: one implementation, two hosts. `fieldAttr` is the data
 *  attribute the host's draft sync reads; the preview slot's
 *  content is rendered from `preview` for `sel` and re-rendered
 *  by the host as previews land. */
export function flagsCacheHtml(o: {
  sel: CacheSelectionLike;
  savedCustom: boolean;
  /** Whether the shown selection IS the saved state and no cache token
   *  is saved for it (the edit form only). */
  savedAbsent: boolean;
  preview: CachePreview | null;
  fieldAttr: string;
  bestAttr: string;
  disabled: boolean;
}): string {
  const dis = o.disabled ? " disabled" : "";
  const selHtml = (field: string, label: string, values: string[], cur: string, tip: string): string => `
        <label style="font-size:12px;color:var(--text2)">${label}<span class="info-tip" title="${esc(tip)}">${icon("info")}</span>
          <span style="display:flex;gap:6px;margin-top:4px">
            <select ${o.fieldAttr}="${field}" style="${SELECT_STYLE};flex:1;min-width:0"${dis}>
              ${values.map((v) => `<option value="${v}"${v === cur ? " selected" : ""}>${v}</option>`).join("")}
            </select>
          </span></label>`;
  return `<div style="margin-top:12px">
      <div style="display:grid;grid-template-columns:1fr 1fr 1fr;gap:10px">
        ${selHtml("cache-k", "K cache type", CACHE_TYPES, o.sel.k, "The element type the key cache is stored in. f16 is full quality; q8_0 stores each element in about half the space with minimal quality loss; q4_0 in about a quarter, with more. Applies on the backend's next start.")}
        ${selHtml("cache-v", "V cache type", CACHE_TYPES, o.sel.v, "The element type the value cache is stored in. A quantized V cache requires flash attention (auto promotes to on; off is refused by llama.cpp and by this form).")}
        ${selHtml("flash-attn", "Flash attention", FLASH_ATTN_MODES, o.sel.flash_attn, "Flash attention computes attention in tiles, cutting memory traffic. Auto lets llama.cpp decide (and promotes to on when a quantized V cache or a tensor split requires it); on forces it; off disables it. On the pinned build it is supported on every GPU the app targets, Pascal included (generic tile kernel).")}
      </div>
      <div style="font-size:12px;color:var(--text2);margin-top:4px"><span style="font-family:var(--mono)">${esc(cacheTokenText(o.sel))}</span>${o.savedCustom ? `<span title="The saved flags spell the cache settings in a form the controls do not write (a -ctk or = spelling, a partial selection, ...). Saving from these controls writes the canonical tokens; every other flag is preserved."> · custom flags</span>` : ""}${o.savedAbsent ? `<span title="No cache tokens are saved for this backend: it runs at llama.cpp's defaults (f16/f16, flash attention auto). Changing a control saves the tokens."> · llama.cpp defaults (no tokens saved)</span>` : ""}</div>
      <div data-cache-slot>${cachePreviewHtml(o.preview, o.sel, o.bestAttr)}</div>
    </div>`;
}

/** The rated-context line under the flags fields, shared by the
 *  Edit Flags form and the Add backend form. `savedText` is the
 *  host's sentence for when the values take effect (the edit form
 *  saves to the configuration immediately; the add form saves at
 *  creation). */
export function flagsRatedLineHtml(o: {
  ctxMax: number | null;
  modelId: string | null;
  savedText: string;
}): string {
  return `<div style="font-size:12px;color:var(--text2);margin-top:6px">${o.modelId && o.ctxMax
    ? `Rated context: ${fmtCount(o.ctxMax)}<span class="info-tip" title="${esc(`Model ${o.modelId} is rated to ${ctxShort(o.ctxMax)} context (${fmtCount(o.ctxMax)} tokens); larger values are accepted but exceed the rating. ${o.savedText}`)}">${icon("info")}</span>`
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
export function offloadSliderHtml(o: {
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
export function paintOffloadReadout(card: Element, o: BackendOffload, n: number): void {
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
export interface FlagsFieldDraft {
  ctx: number;
  batch: number;
  parallel: number | null;
  /** The Custom… choices, explicit state: a custom value equal to a
   *  preset keeps the numeric input. */
  ctxCustom: boolean;
  batchCustom: boolean;
  parallelCustom: boolean;
}

/** A draft for saved values (a non-preset value is custom already). */
export function newFlagsFields(ctx: number, batch: number, parallel: number | null): FlagsFieldDraft {
  return {
    ctx, batch, parallel,
    ctxCustom: !CTX_PRESETS.includes(ctx),
    batchCustom: !BATCH_PRESETS.includes(batch),
    parallelCustom: parallel !== null && !PARALLEL_PRESETS.includes(parallel),
  };
}

/** What is wrong with the context/batch/parallel draft, or null — the
 *  bounds the save ops apply, in the form's own terms. An explicit
 *  saved `--parallel -1` (auto) is accepted unchanged. */
export function flagsFieldError(d: FlagsFieldDraft): string | null {
  if (!Number.isInteger(d.ctx) || d.ctx < CTX_MIN || d.ctx > CTX_MAX) return `Context length must be a whole number between ${fmtCount(CTX_MIN)} and ${fmtCount(CTX_MAX)}.`;
  if (!Number.isInteger(d.batch) || d.batch < 1 || d.batch > d.ctx) return `Batch size must be a whole number between 1 and the context length (${fmtCount(d.ctx)}).`;
  if (d.parallel !== null && d.parallel !== -1 && (!Number.isInteger(d.parallel) || d.parallel < PARALLEL_MIN || d.parallel > PARALLEL_MAX)) return `Parallel slots must be a whole number between ${PARALLEL_MIN} and ${fmtCount(PARALLEL_MAX)}.`;
  return null;
}

export function applyFlagsField(
  draft: FlagsFieldDraft,
  field: string,
  value: string,
  isChange: boolean,
): boolean {
  // An emptied custom input is NaN (invalid), not 0.
  const num = value.trim() === "" ? NaN : Number(value);
  if (field === "ctx-custom") { draft.ctx = num; return false; }
  if (field === "batch-custom") { draft.batch = num; return false; }
  if (field === "parallel-custom") { draft.parallel = num; return false; }
  if (isChange && field === "ctx-preset") {
    draft.ctxCustom = value === "custom";
    if (value !== "custom") draft.ctx = Number(value);
    return true;
  }
  if (isChange && field === "batch-preset") {
    draft.batchCustom = value === "custom";
    if (value !== "custom") draft.batch = Number(value);
    return true;
  }
  if (isChange && field === "parallel-preset") {
    draft.parallelCustom = value === "custom";
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
 *  backend's model is not offload-capable. offloadPending is
 *  a released draft position the guardrail blocks, awaiting the
 *  operator's override/snap decision in the form (override = the
 *  decision to save it anyway), else null. modelChanged: the Model
 *  draft differs from the saved model — the model-specific controls
 *  (offload, expert cache, MTP) wait for that save.
 *  mtpPosition is the MTP draft control's chosen value (0 = Off,
 *  1..4 = draft tokens), null when the backend's model is not
 *  MTP-capable. moeCacheOn / moeCacheMib are the expert-cache
 *  control's draft: the switch state and the chosen size (the
 *  size draft is retained while the switch is off, so toggling
 *  back on restores it); the size is null when the backend has
 *  never carried a cache and none has been chosen. fields carries the context, batch
 *  and parallel drafts. advanced/rawText/rawWarnings
 *  carry the advanced flags editor's draft (tracker #60): while
 *  advanced is on, the raw text is the single source of truth and
 *  the structured fields are inert. editModelId is the Edit
 *  panel's model draft: the registered model chosen in the
 *  panel's Model select ("" when the backend's file matches no
 *  registered model), applied through model_assign only when the
 *  panel saves — the card face carries no model control. */
export interface FlagsDraft { fields: FlagsFieldDraft; moeCacheAuto: boolean; modelChanged: boolean; ctxMax: number | null; modelId: string | null; offloadLayers: number | null; offloadPending: { layers: number; override: boolean } | null; mtpPosition: number | null; moeCacheOn: boolean; moeCacheMib: number | null; cacheSel: CacheSelectionLike | null; cachePreview: CachePreview | null; advanced: boolean; rawText: string; rawWarnings: FlagWarningLike[]; editModelId: string }

/** State for the in-card Placement form (tensor split editing,
 *  moved here from the GPUs page — Pete's direction, 2026-10-16),
 *  non-null only for the backend whose form is open. gpus mirrors
 *  the latest telemetry GPU list for the include/exclude
 *  checkboxes; excluded holds the unchecked GPU indices (a draft the
 *  Edit panel's Save writes); projection is the last successful
 *  propose result for this backend; busy disables the section's
 *  controls while a propose invoke is in flight. */
export interface PlaceCardState {
  gpus: { index: number; name: string }[];
  excluded: Set<number>;
  modelId: string | null;
  projection: PlacementProjectionData | null;
  busy: boolean;
}
