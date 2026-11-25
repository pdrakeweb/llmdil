import { describe, expect, it } from "vitest";
import type { BackendRow, CachePreview, ModelRow } from "../src/api";
import { parseAppliedStrategies } from "../src/api/gateway";
import { fitText, sparkRange } from "../src/charts";
import { plainError, portError } from "../src/errors";
import { setupNeedsWizard } from "../src/setupGate";
import { pageState } from "../src/viewState";
import { autoRestartText, fitNow, gpuColumnText, rawContext } from "../src/views/backendCard";
import {
  applyFlagsField, fitChipHtml, flagsCtxBatchHtml, flagsFieldError, flagsParallelHtml, mtpConflictText, newFlagsFields,
  rawFlagWarnings, repriceCachePreview,
} from "../src/views/backendFlags";
import { fmtDateMs, parseTemperature } from "../src/views/bench";
import { fmtBytes, modelDisplayName } from "../src/views/common";
import { logSeverity } from "../src/views/logs";
import { downloadProgressText } from "../src/views/models";
import { autoUpdateSupported } from "../src/views/settingsPanels";

describe("custom context, batch and parallel (A13)", () => {
  const html = (d: ReturnType<typeof newFlagsFields>): string =>
    flagsCtxBatchHtml({ ...d, ctxMax: null, fieldAttr: "data-f", disabled: false })
    + flagsParallelHtml({ value: d.parallel, custom: d.parallelCustom, savedCustom: false, fieldAttr: "data-f", disabled: false });
  it("Custom… stays chosen while the value equals a preset", () => {
    const d = newFlagsFields(32768, 512, null);
    expect(html(d)).not.toContain("ctx-custom");
    for (const f of ["ctx-preset", "batch-preset", "parallel-preset"]) expect(applyFlagsField(d, f, "custom", true)).toBe(true);
    expect(d).toMatchObject({ ctx: 32768, batch: 512, parallel: 4, ctxCustom: true, batchCustom: true, parallelCustom: true });
    const h = html(d);
    for (const f of ["ctx-custom", "batch-custom", "parallel-custom"]) expect(h).toContain(`data-f="${f}"`);
    expect(h.match(/value="custom" selected/g)).toHaveLength(3);
    applyFlagsField(d, "ctx-preset", "65536", true);
    expect(d).toMatchObject({ ctx: 65536, ctxCustom: false });
  });
  it("a saved non-preset value opens as custom", () => {
    expect(newFlagsFields(40000, 600, 3)).toMatchObject({ ctxCustom: true, batchCustom: true, parallelCustom: true });
  });
  it("validates the ranges, an emptied field included", () => {
    const d = newFlagsFields(1024, 1, null);
    expect(flagsFieldError(d)).toBeNull();
    for (const bad of ["", "1023", "1048577", "2000.5", "abc"]) {
      applyFlagsField(d, "ctx-custom", bad, false);
      expect(flagsFieldError(d)).toMatch(/Context length/);
    }
    applyFlagsField(d, "ctx-custom", "1048576", false);
    expect(flagsFieldError(d)).toBeNull();
    applyFlagsField(d, "batch-custom", "0", false);
    expect(flagsFieldError(d)).toMatch(/Batch size/);
    applyFlagsField(d, "batch-custom", "512", false);
    applyFlagsField(d, "parallel-custom", "1025", false);
    expect(flagsFieldError(d)).toMatch(/Parallel slots/);
  });
});

describe("honest fit", () => {
  it("Fits only within free memory; paging never reads as a fit", () => {
    expect(fitChipHtml({ state: "fits", fitsNow: true })).toContain(">Fits<");
    expect(fitChipHtml({ state: "fits", fitsNow: false, neededMib: 12288, freeMib: 4096 })).toContain("Not enough free now: needs 12 GiB, free 4 GiB");
    expect(fitChipHtml({ state: "fits", fitsNow: null })).not.toContain(">Fits<");
    const paged = fitChipHtml({ state: "wddm_paged", pagedMib: 1536, fitsNow: true });
    expect(paged).toContain("Will page 1.5 GiB");
    expect(paged).not.toMatch(/Fits/);
    expect(fitChipHtml({ state: null })).toBe("");
  });
  const row = (fit: BackendRow["fit"], fits_now?: boolean): BackendRow => ({ id: "a", fit, fits_now } as BackendRow);
  const gpu = { gpu: 0, required_mib: 10000, total_mib: 16000, driver_model: "wddm" as const, state: "fits" as const, paged_mib: 0 };
  it("credits what the backend itself holds and lets the payload decide", () => {
    const fit = { state: "fits" as const, paged_mib: 0, per_gpu: [gpu] };
    expect(fitNow(row(fit), new Map([[0, 4000]]), [])).toEqual({ fitsNow: false, neededMib: 10000, freeMib: 4000, gpu: 0 });
    expect(fitNow(row(fit), new Map([[0, 4000]]), [{ gpu: 0, name: "", mib: 7000 }]).fitsNow).toBe(true);
    expect(fitNow(row(fit), new Map([[0, 4000]]), [{ gpu: 0, name: "", mib: null }]).fitsNow).toBeNull();
    expect(fitNow(row(fit, true), new Map(), []).fitsNow).toBe(true);
    // A split: the figures are the short GPU's, not the sum over both.
    const split = { ...fit, per_gpu: [gpu, { ...gpu, gpu: 1 }] };
    expect(fitNow(row(split), new Map([[0, 24000], [1, 4000]]), [])).toEqual({ fitsNow: false, neededMib: 10000, freeMib: 4000, gpu: 1 });
  });
});

describe("footprint follows the drafts (B12)", () => {
  const base: CachePreview = {
    selection: { k: "f16", v: "f16", flash_attn: "auto" }, v_quantized_fa_off: false, weights_mib: 12000, sys_weights_mib: 0,
    kv_mib: 4000, rs_mib: null, moe_cache_mib: 0, total_mib: 16000, basis: "exact",
    targets: [{ gpu: 0, total_mib: 16000, required_mib: 16000, fits: false, driver_model: "wddm", fit_state: "fits", paged_mib: 0, free_mib: 10000, fits_free: false }],
    headroom_reserve_percent: 5, fits_current: false, ladder: [], best: null,
  };
  it("a moved offload slider changes weights, total and the GPU row", () => {
    const p = repriceCachePreview(base, { weightsMib: 5000 });
    expect(p).toMatchObject({ weights_mib: 5000, sys_weights_mib: 7000, total_mib: 9000 });
    expect(p.targets?.[0]).toMatchObject({ required_mib: 9000, fit_state: "fits", fits_free: true });
  });
  it("a changed GPU selection divides the total evenly; over total pages on WDDM only", () => {
    const g = (n: number, wddm: boolean) => ({ gpu: n, total_mib: 7000, free_mib: 7000, wddm });
    const p = repriceCachePreview(base, { gpus: [g(1, false), g(2, true)] });
    expect(p.targets?.map((t) => [t.gpu, t.required_mib, t.fit_state, t.paged_mib])).toEqual([[1, 8000, "does_not_fit", 0], [2, 8000, "wddm_paged", 1000]]);
  });
  it("nothing drafted returns the preview itself", () => {
    expect(repriceCachePreview(base, {})).toBe(base);
  });
});

describe("raw flags (B9)", () => {
  it("the app's own flags are recognized", () => {
    expect(rawFlagWarnings("--cache-type-k q8_0 --cache-type-v q8_0 --flash-attn on --moe-cache-mib 4096 --spec-type draft-mtp --spec-draft-n-max 2 -ngl all --fit on")).toEqual([]);
    expect(rawFlagWarnings("--moe-cache-mib lots")).toHaveLength(1);
    expect(rawFlagWarnings("--not-a-flag 1")[0].token).toBe("--not-a-flag");
  });
  it("finds the raw flags' own context", () => {
    expect(rawContext("--jinja -c 8192")).toBe(8192);
    expect(rawContext("--ctx-size=4096 -c 2048")).toBe(2048);
    expect(rawContext("-ctk q8_0")).toBeNull();
  });
});

describe("backend rows", () => {
  it("lists every configured GPU, unmeasured ones marked (B8)", () => {
    const b = { tensor_split: [0.5, 0.5, 0] };
    expect(gpuColumnText(b, true, [{ gpu: 0, name: "", mib: 8000 }])).toBe("GPU 0, GPU 1 (not measured)");
    expect(gpuColumnText(b, true, [{ gpu: 0, name: "", mib: null }, { gpu: 1, name: "", mib: 200 }])).toBe("GPU 0 (not measured), GPU 1 (context only)");
    expect(gpuColumnText(b, false, [])).toBe("GPU 0, GPU 1");
  });
  it("warns about MTP with any slot count but 1 (B14)", () => {
    expect(mtpConflictText(null)).toMatch(/auto \(4\)/);
    expect(mtpConflictText(2)).toMatch(/Parallel slots: 2/);
  });
  it("counts down to the next auto-restart", () => {
    const a = { attempt: 2, max: 5, next_at_ms: 20_000, gave_up: false };
    expect(autoRestartText(a, 2_000)).toBe("Auto-restart: attempt 2 of 5 in 18 s");
    expect(autoRestartText(a, 30_000)).toMatch(/now$/);
    expect(autoRestartText({ ...a, gave_up: true }, 0)).toMatch(/gave up/);
  });
});

describe("plain errors (C10)", () => {
  it("strips the error enum's prefixes", () => {
    expect(plainError("config error: validation failed: unknown fallback 'x'")).toBe("unknown fallback 'x'");
    expect(plainError("invalid input: no such backend")).toBe("no such backend");
  });
  it("explains an argument that did not decode", () => {
    expect(plainError("invalid args `port` for command `backend_add`: invalid value: integer `70000`, expected u16")).toBe("Port must be a whole number between 1 and 65,535.");
    expect(plainError("invalid args `nCtx` for command `x`: invalid type: null, expected u32")).toBe("Context length must be a whole number, 0 or more.");
  });
  it("validates ports", () => {
    for (const p of [0, 65536, 1.5, NaN]) expect(portError(p)).not.toBeNull();
    expect(portError(65535)).toBeNull();
  });
});

describe("small displays", () => {
  it("binary units are labelled as such", () => {
    expect(fmtBytes(1536)).toBe("1.5 KiB");
    expect(fmtBytes(18.6 * 1024 ** 3)).toBe("18.6 GiB");
  });
  it("one display name per model, from id, path or stem", () => {
    const models = [{ id: "qwen3-coder-30b", name: "Qwen3-Coder-30B", gguf_path: "D:\\m\\qwen-coder.Q4.gguf" }] as ModelRow[];
    for (const x of ["qwen3-coder-30b", "D:\\m\\qwen-coder.Q4.gguf", "/other/qwen-coder.Q4.gguf", "qwen-coder.Q4"]) expect(modelDisplayName(x, models)).toBe("Qwen3-Coder-30B");
    expect(modelDisplayName("/x/unknown.gguf", models)).toBe("unknown");
  });
  it("one degree of noise is not a full-height sparkline", () => {
    const [lo, hi] = sparkRange([60, 61], 10);
    expect(hi - lo).toBeCloseTo(10);
    expect(sparkRange([0, 0.5], 10)[0]).toBe(0);
    expect(sparkRange([0, 100])).toEqual([0, 100]);
  });
  it("cuts SVG text to its room", () => {
    expect(fitText("short", 200, 12)).toBe("short");
    expect(fitText("nemotron-super-120b-long-name", 60, 12, 0.6)).toMatch(/…$/);
  });
  it("shows download percent and speed only when known", () => {
    expect(downloadProgressText(50, 100, 1048576)).toBe("50% · 1 MiB/s");
    expect(downloadProgressText(50, null, null)).toBe("");
  });
  it("bench: no 1970 dates, temperature within 0..2", () => {
    expect(fmtDateMs(0)).toBe("—");
    expect(fmtDateMs(null)).toBe("—");
    expect(parseTemperature("0.6")).toEqual({ value: 0.6 });
    for (const t of ["", "-1", "2.1", "x"]) expect(parseTemperature(t)).toHaveProperty("error");
  });
  it("highlights CUDA and llama.cpp error lines", () => {
    expect(logSeverity("0.00.123 E ggml_cuda: failed")).toBe("error");
    expect(logSeverity("CUDA error: out of memory")).toBe("error");
    expect(logSeverity("0.00.123 W low memory")).toBe("warn");
    expect(logSeverity("main: server is listening")).toBeNull();
  });
  it("LiteLLM offers no auto-update unless the payload says it can", () => {
    expect(autoUpdateSupported("litellm", {}, undefined)).toBe(false);
    expect(autoUpdateSupported("llama.cpp", {}, undefined)).toBe(true);
    expect(autoUpdateSupported("litellm", { auto_update_supported: true }, undefined)).toBe(true);
  });
  it("reads per-group applied strategies in either shape", () => {
    expect(parseAppliedStrategies({ a: "least-busy" })).toEqual({ a: "least-busy" });
    expect(parseAppliedStrategies([{ name: "a", applied_strategy: "least-busy" }])).toEqual({ a: "least-busy" });
    expect(parseAppliedStrategies(null)).toBeUndefined();
  });
  it("page state survives a second visit", () => {
    pageState("t", { f: "" }).f = "x";
    expect(pageState("t", { f: "" }).f).toBe("x");
  });
  it("the wizard opens itself only when a required part is missing (B20)", () => {
    const b = [{ configured: true }];
    expect(setupNeedsWizard({ backends: b, gateway: "stopped", steps: [{ id: "smoke-test", state: "failed" }] })).toBe(false);
    expect(setupNeedsWizard({ backends: null, gateway: null, steps: [] })).toBe(false);
    expect(setupNeedsWizard({ backends: [], gateway: "running", steps: [] })).toBe(true);
    expect(setupNeedsWizard({ backends: b, gateway: "not_installed", steps: [] })).toBe(true);
    expect(setupNeedsWizard({ backends: b, gateway: "running", steps: [{ id: "install-llamacpp", state: "failed" }] })).toBe(true);
  });
});
