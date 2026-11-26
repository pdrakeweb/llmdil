import { describe, expect, it } from "vitest";
import type { BackendOffload, MeasurementInputs, OptimizePlanItem } from "../src/api";
import { optimizeChangeSummary, optimizeIsDestructive, optimizeKindLabel } from "../src/api";
import {
  armLabel, expandDims, offloadChoices, offloadExprFor, offloadLayersOf, offloadPosLabel, presetArms, pruneReason,
} from "../src/views/bench";
import { startAllHtml, startAllTargets } from "../src/startAll";
import { mcpIdError } from "../src/views/mcp";
import { topologyGroups, topologySvg } from "../src/views/topology";
import { mockDashboard } from "../src/mock";
import { diffInputFields, placementShort } from "../src/views/optimize";

function offload(over: Partial<BackendOffload> = {}): BackendOffload {
  return {
    moe_layers: 48,
    layers: 12,
    expert_bytes: 0,
    total_bytes: 0,
    green_limit_layers: 0,
    green_end: 0.5,
    yellow_end: 0.8,
    custom: false,
    guardrail: null,
    ...over,
  } as BackendOffload;
}

describe("bench offload expressions", () => {
  it("offloadExprFor spells the canonical form per position", () => {
    expect(offloadExprFor(0, 48)).toBe("");
    expect(offloadExprFor(-1, 48)).toBe("");
    expect(offloadExprFor(12, 48)).toBe("--n-cpu-moe 12");
    expect(offloadExprFor(48, 48)).toBe("-ot exps=CPU");
    expect(offloadExprFor(60, 48)).toBe("-ot exps=CPU");
  });
  it("offloadLayersOf parses every spelling it can judge", () => {
    expect(offloadLayersOf("", 48)).toBe(0);
    expect(offloadLayersOf("  ", 48)).toBe(0);
    expect(offloadLayersOf("--n-cpu-moe 12", 48)).toBe(12);
    expect(offloadLayersOf("-ncmoe=20", 48)).toBe(20);
    expect(offloadLayersOf("--n-cpu-moe 99", 48)).toBe(48);
    expect(offloadLayersOf("--cpu-moe", 48)).toBe(48);
    expect(offloadLayersOf("-cmoe", 48)).toBe(48);
    expect(offloadLayersOf("-ot exps=CPU", 48)).toBe(48);
    expect(offloadLayersOf("-ot exps = cpu, exps=CPU", 48)).toBe(48);
  });
  it("offloadLayersOf returns null for custom forms", () => {
    expect(offloadLayersOf("-ot blk\\.1\\.=CPU", 48)).toBeNull();
    expect(offloadLayersOf("--something-else", 48)).toBeNull();
  });
  it("offloadExprFor round-trips through offloadLayersOf", () => {
    for (const n of [0, 1, 24, 47, 48]) expect(offloadLayersOf(offloadExprFor(n, 48), 48)).toBe(n);
  });
  it("offloadChoices offers quarter steps plus the current position, sorted and deduplicated", () => {
    expect(offloadChoices(offload({ moe_layers: 48, layers: 12 }))).toEqual([0, 12, 24, 36, 48]);
    expect(offloadChoices(offload({ moe_layers: 48, layers: 7 }))).toEqual([0, 7, 12, 24, 36, 48]);
  });
  it("offloadPosLabel names the endpoints", () => {
    const off = offload();
    expect(offloadPosLabel(0, off)).toBe("No offload");
    expect(offloadPosLabel(48, off)).toBe("All experts");
    expect(offloadPosLabel(20, off)).toBe("-ncmoe 20");
  });
});

describe("bench arm expansion", () => {
  it("expandDims takes the cartesian product and skips empty dimensions", () => {
    const arms = expandDims([
      [{ cache_k: "f16", cache_v: "f16" }, { cache_k: "q8_0", cache_v: "q8_0" }],
      [],
      [{ depth: 0 }, { depth: 8192 }, { depth: 30720 }],
    ]);
    expect(arms).toHaveLength(6);
    expect(arms[0]).toEqual({ cache_k: "f16", cache_v: "f16", depth: 0 });
    expect(arms[5]).toEqual({ cache_k: "q8_0", cache_v: "q8_0", depth: 30720 });
  });
  it("expandDims with no dimensions is the single baseline arm", () => {
    expect(expandDims([])).toEqual([{}]);
  });
  it("presetArms only attaches a draft model when one is chosen", () => {
    const withDraft = presetArms("spec", "draft-1", 3, "CUDA0");
    expect(withDraft).toHaveLength(4);
    expect(withDraft[2].draft_model_id).toBe("draft-1");
    const noDraft = presetArms("combined", "", 3, "CUDA0");
    expect(noDraft.every((a) => !("draft_model_id" in a))).toBe(true);
    expect(presetArms("kv", "", 3, "CUDA0").map((a) => `${a.cache_k}/${a.cache_v}`)).toEqual([
      "f16/f16", "q8_0/q8_0", "f16/q8_0", "q4_0/q4_0",
    ]);
  });
  it("armLabel describes each dimension and the baseline", () => {
    expect(armLabel({})).toBe("No spec");
    expect(armLabel({ cache_k: "q8_0", cache_v: "q8_0", flash_attn: "on", depth: 8192 })).toBe("KV q8_0/q8_0 · FA on · Depth 8,192 · No spec");
    expect(armLabel({ spec: "combined" })).toBe("combined n-min 16 · draft n-max 3");
    expect(armLabel({ spec: "none", offload_expr: "" })).toBe("No spec · Offload none");
  });
});

describe("bench pruneReason", () => {
  it("prunes a quantized V cache with flash attention off", () => {
    expect(pruneReason({ cache_v: "q8_0", flash_attn: "off" }, null)).toMatch(/Quantized V cache/);
    expect(pruneReason({ cache_v: "q8_0", flash_attn: "on" }, null)).toBeNull();
    expect(pruneReason({ cache_v: "f16", flash_attn: "off" }, null)).toBeNull();
  });
  it("prunes a draft speculative kind without a draft model", () => {
    expect(pruneReason({ spec: "draft-simple" }, null)).toMatch(/without a draft model/);
    expect(pruneReason({ spec: "draft-simple" }, "draft-1")).toBeNull();
    expect(pruneReason({ spec: "combined", draft_model_id: "d" }, null)).toBeNull();
    expect(pruneReason({ spec: "ngram-mod" }, null)).toBeNull();
  });
  it("applies the endpoints-only offload guardrail on the pinned compute capability", () => {
    const guarded = offload({
      layers: 0,
      guardrail: { allowed: "endpoints_only", compute_capability: "12.0", placed_ccs: ["12.0"] } as unknown as BackendOffload["guardrail"],
    });
    expect(pruneReason({ offload_expr: "--n-cpu-moe 12" }, null, guarded)).toMatch(/Partial expert offload is forbidden/);
    expect(pruneReason({ offload_expr: "" }, null, guarded)).toBeNull();
    expect(pruneReason({ offload_expr: "-ot exps=CPU" }, null, guarded)).toBeNull();
    const otherGpu = offload({
      layers: 12,
      guardrail: { allowed: "endpoints_only", compute_capability: "12.0", placed_ccs: ["8.9"] } as unknown as BackendOffload["guardrail"],
    });
    expect(pruneReason({}, null, otherGpu)).toBeNull();
  });
});

describe("optimize helpers", () => {
  const base: MeasurementInputs = {
    n_ctx: 8192, n_batch: 512, offload: "", tensor_split: [0.5, 0.5], split_mode: "layer", raw_flags: "", gpus: ["0:abc", "1:def"],
  };
  it("diffInputFields lists changed fields in design order", () => {
    expect(diffInputFields(base, { ...base })).toEqual([]);
    expect(diffInputFields(base, { ...base, n_ctx: 4096, gpus: ["0:abc"], tensor_split: [1] })).toEqual([
      "context", "tensor split", "GPU set",
    ]);
    expect(diffInputFields(base, { ...base, raw_flags: "-fa on", split_mode: "row", n_batch: 1024, offload: "x" })).toEqual([
      "batch", "offload", "split mode", "raw flags",
    ]);
  });
  it("placementShort names GPU indices or CPU", () => {
    expect(placementShort(base)).toBe("GPU 0+1");
    expect(placementShort({ ...base, gpus: [] })).toBe("CPU");
  });
  it("kind labels and destructive kinds mirror manager-optimize", () => {
    expect(optimizeKindLabel("backend.set_context")).toBe("Context");
    expect(optimizeKindLabel("group.set_fallbacks")).toBe("Routing");
    expect(optimizeKindLabel("something.new")).toBe("Advice");
    expect(optimizeIsDestructive("backend.delete")).toBe(true);
    expect(optimizeIsDestructive("group.delete")).toBe(true);
    expect(optimizeIsDestructive("backend.set_context")).toBe(false);
  });
  it("optimizeChangeSummary renders from the change payload only", () => {
    const item = (kind: string, change: Record<string, unknown>, target: string | null = null) =>
      ({ kind, change, target } as unknown as OptimizePlanItem);
    const short = (id: string) => `short(${id})`;
    expect(optimizeChangeSummary(item("backend.set_context", { n_ctx: 32768 }), short)).toBe("Context: 32,768 tokens");
    expect(optimizeChangeSummary(item("backend.set_context", {}), short)).toBe("Context: ? tokens");
    expect(optimizeChangeSummary(item("backend.set_placement", { tensor_split: [0.6, 0.4], split_mode: "row" }), short))
      .toBe("Split: 60 / 40 % · Mode: row");
    expect(optimizeChangeSummary(item("backend.set_offload", { offload_layers: 8, moe_cache_mib: 0 }), short))
      .toBe("Offload: 8 layers to system RAM · Expert cache: off");
    expect(optimizeChangeSummary(item("backend.create", { model_id: "m", port: 8081, group: "chat", n_ctx: 4096 }), short))
      .toBe("Model: short(m) · Port: 8,081 · Group: chat · Context: 4,096");
    expect(optimizeChangeSummary(item("group.set_fallbacks", { fallbacks: [] }), short)).toBe("Fallbacks: none");
    expect(optimizeChangeSummary(item("group.delete", {}, "planner"), short)).toBe("Group: planner");
    expect(optimizeChangeSummary(item("unknown.kind", { x: 1 }), short)).toBe("");
  });
});

describe("mcpIdError", () => {
  it("accepts ids matching ^[a-z][a-z0-9_]*$", () => {
    expect(mcpIdError("filesystem")).toBe("");
    expect(mcpIdError("web_search2")).toBe("");
  });
  it("rejects empty, upper-case, leading digits, dashes and untrimmed input", () => {
    expect(mcpIdError("")).toBe("Id is required.");
    for (const bad of ["Files", "2fs", "my-server", " fs", "fs ", "_fs"]) {
      expect(mcpIdError(bad)).toMatch(/must match/);
    }
  });
});

describe("Start All Services", () => {
  const rows = [
    { id: "run", state: "running" }, { id: "stop", state: "stopped" }, { id: "fail", state: "failed" },
    { id: "start", state: "starting" }, { id: "off", state: "stopped", enabled: false },
    { id: "orphan", state: "stopped", configured: false },
  ];
  const stateOf = (b: { state: string }) => b.state as "running";
  it("starts only enabled, configured backends that are down, and a down gateway", () => {
    expect(startAllTargets(rows, stateOf, "running")).toEqual({ backends: ["stop", "fail"], gateway: false });
    expect(startAllTargets(rows, stateOf, "stopped").gateway).toBe(true);
    expect(startAllTargets(rows, stateOf, "restarting").gateway).toBe(false);
    expect(startAllTargets(rows, stateOf, null).gateway).toBe(false);
  });
  it("shows nothing when everything runs, and keeps a failure until dismissed", () => {
    const base = { running: 3, total: 3, stopped: false, busy: false, error: null };
    expect(startAllHtml(base)).toBe("");
    const html = startAllHtml({ ...base, running: 2, stopped: true });
    expect(html).toContain("Services running: 2 of 3");
    expect(html).toContain("data-start-all ");
    expect(startAllHtml({ ...base, busy: true })).toContain("disabled");
    const failed = startAllHtml({ ...base, error: "Start All failed — <x>" });
    expect(failed).toContain("&lt;x&gt;");
    expect(failed).toContain("data-start-all-dismiss");
    expect(failed).not.toContain("Services running");
  });
});

describe("Topology: groups contain their models", () => {
  const b = (id: string, lifecycle: "running" | "stopped") =>
    ({ id, name: id, port: 1, status: lifecycle === "running" ? "ok" : "idle", lifecycle, model: `${id}-model` }) as never;
  const g = (name: string, members: string[]) =>
    ({ name, members, strategy: "least-busy", fallbacks: [], purpose: "", enabled: true, enabled_members: members.length });
  it("lists each group's members, then the backends no group routes to", () => {
    const out = topologyGroups({ backends: [b("a", "running"), b("b", "stopped"), b("c", "running")], groups: [g("one", ["a", "b"]), g("ext", ["h:9"])] });
    expect(out.map((x) => x.name)).toEqual(["one", "ext", null]);
    expect(out[0].rows.map((r) => r.backend?.id)).toEqual(["a", "b"]);
    expect(out[1].rows[0].backend).toBeUndefined();
    expect(out[2].rows.map((r) => r.member)).toEqual(["c"]);
  });
  it("never invents a grouping when the groups are unknown", () => {
    const out = topologyGroups({ backends: [b("a", "running")], groups: null });
    expect(out).toHaveLength(1);
    expect(out[0].name).toBeNull();
  });
  it("draws a container per group with its models and the deployed count", () => {
    const svg = topologySvg(mockDashboard());
    expect(svg.match(/class="topo-group-box"/g)).toHaveLength(3);
    expect(svg).toContain("2 of 2 deployed");
    expect(svg).toContain("Qwen3-Coder-30B");
  });
});
