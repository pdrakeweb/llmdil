/* Built-in sample data used when the UI is NOT running under Tauri
   (plain browser). Mirrors the values in the approved mockups so the
   layout can be evaluated without a backend. */

import { walk } from "./charts";
import type { OptimizePlanItem, OptimizeState, OptimizeValidation } from "./api";
import type { BackendInfo, DashboardModel, GpuInfo, WizardStepState } from "./types";

export function mockBackends(): BackendInfo[] {
  return [
    { id: "gateway", name: "gateway", port: 4000, status: "ok", health: 100, model: "", quant: "",
      throughput_toks: 0, vram_held_gb: 0, requests_1h: 1284, errors_1h: 0, spark: walk(40, 32, 8, 7) },
    { id: "planner", name: "planner", port: 8081, status: "ok", health: 92, model: "Qwen3.8-27B", quant: "Q4_K_M",
      throughput_toks: 8.4, vram_held_gb: 10.2, requests_1h: 0, errors_1h: 0, spark: walk(40, 8.4, 2.5, 11) },
    { id: "coder-30b", name: "coder-30b", port: 8082, status: "ok", health: 96, model: "Qwen3-Coder-30B", quant: "Q4_K_M",
      throughput_toks: 22.6, vram_held_gb: 18.6, requests_1h: 0, errors_1h: 0, spark: walk(40, 22.6, 5, 21) },
    { id: "coder-7b", name: "coder-7b", port: 8083, status: "ok", health: 88, model: "Qwen2.5-Coder-7B", quant: "Q5_K_M",
      throughput_toks: 41.2, vram_held_gb: 5.8, requests_1h: 0, errors_1h: 0, spark: walk(40, 41.2, 7, 31) },
    { id: "tool-runner", name: "tool-runner", port: 8084, status: "starting", health: 60, model: "Qwen3-8B", quant: "Q8_0",
      throughput_toks: 0, vram_held_gb: 1.1, requests_1h: 0, errors_1h: 0, spark: walk(40, 1, 1, 41) },
  ];
}

export function mockGpus(): GpuInfo[] {
  return [
    { id: "a4000", name: "RTX A4000", pci: "01:00.0", driver: "581.42", status: "ok",
      util_pct: 62, vram_used_gb: 9.8, vram_total_gb: 16, temp_c: 71, temp_peak_c: 74,
      power_w: 118, power_limit_w: 140,
      util_spark: walk(60, 62, 10, 101), temp_spark: walk(60, 71, 3, 102), power_spark: walk(60, 118, 12, 103),
      processes: [
        { process: "llama-server", pid: 4212, vram_mib: 6144, backend: "planner" },
        { process: "llama-server", pid: 4388, vram_mib: 3898, backend: "coder-30b" },
      ],
      models: [
        { model: "qwen3.8-27b", layers: "16/28 layers" },
        { model: "qwen3-coder-30b", layers: "20/44 layers" },
      ] },
    { id: "p100-0", name: "Tesla P100-0", pci: "41:00.0", driver: "581.42", status: "ok",
      util_pct: 45, vram_used_gb: 7.2, vram_total_gb: 16, temp_c: 68, temp_peak_c: 70,
      power_w: 210, power_limit_w: 250,
      util_spark: walk(60, 45, 12, 111), temp_spark: walk(60, 68, 3, 112), power_spark: walk(60, 210, 15, 113),
      processes: [
        { process: "llama-server", pid: 4212, vram_mib: 4210, backend: "planner" },
        { process: "llama-server", pid: 4388, vram_mib: 3166, backend: "coder-30b" },
      ],
      models: [
        { model: "qwen3.8-27b", layers: "12/28 layers" },
        { model: "qwen3-coder-30b", layers: "16/44 layers" },
      ] },
    { id: "p100-1", name: "Tesla P100-1", pci: "81:00.0", driver: "581.42", status: "ok",
      util_pct: 12, vram_used_gb: 1.1, vram_total_gb: 16, temp_c: 54, temp_peak_c: 61,
      power_w: 68, power_limit_w: 250,
      util_spark: walk(60, 12, 8, 121), temp_spark: walk(60, 54, 3, 122), power_spark: walk(60, 68, 10, 123),
      processes: [
        { process: "llama-server", pid: 4550, vram_mib: 1126, backend: "tool-runner" },
      ],
      models: [
        { model: "qwen3-coder-30b", layers: "8/44 layers" },
        { model: "qwen3-8b", layers: "32/32 layers" },
      ] },
  ];
}

export function mockDashboard(): DashboardModel {
  const all = mockBackends();
  const [gateway, ...backends] = all;
  return {
    gateway,
    backends,
    gpus: mockGpus(),
    placement: [
      { model: "qwen3.8-27b", backendId: "planner", backendStatus: "ok", segments: [{ gpu: "a4000", pct: 60, frac: 0.6 }, { gpu: "p100-0", pct: 40, frac: 0.4 }] },
      { model: "qwen3-coder-30b", backendId: "coder-30b", backendStatus: "ok", segments: [{ gpu: "a4000", pct: 45, frac: 0.45 }, { gpu: "p100-0", pct: 35, frac: 0.35 }, { gpu: "p100-1", pct: 20, frac: 0.2 }] },
      { model: "qwen2.5-coder-7b", backendId: "coder-7b", backendStatus: "ok", segments: [{ gpu: "p100-0", pct: 100, frac: 1 }] },
      { model: "qwen3-8b", backendId: "tool-runner", backendStatus: "starting", segments: [{ gpu: "p100-1", pct: 100, frac: 1 }] },
    ],
    gatewayUp: true,
    gatewayStrategy: "least-busy",
    stale: false,
    updatedAt: Date.now(),
  };
}

export function mockWizardSteps(): WizardStepState[] {
  return [
    { id: "detect", name: "Detect hardware & drivers", detail: "A4000 + 2× P100 · NVIDIA 581.42", state: "done", duration: "18s" },
    { id: "llamacpp", name: "Install llama.cpp CUDA build", detail: "b7721 · CUDA 12.8 · D:\\llm\\llama.cpp\\", state: "done", duration: "2m 04s" },
    { id: "tensorsplit", name: "Configure tensor-split", detail: "27B — 60/40 · 30B — 45/35/20", state: "done", duration: "6s" },
    { id: "models", name: "Download models + verify", detail: "4 GGUF · 46.5 GB · sha256 each", state: "done", duration: "11m 32s", note: "1 retry" },
    { id: "gateway", name: "Start LiteLLM gateway", detail: ":4000 · 4 model groups", state: "done", duration: "9s" },
    { id: "vscode", name: "Install VS Code extensions", detail: "Continue.dev · Cline", state: "running", note: "installing…" },
    { id: "mcp", name: "Configure MCP servers", detail: "GitHub · Google Drive", state: "queued" },
    { id: "winml", name: "Register Windows ML backend", detail: "WinMLServer :8090 · secondary", state: "queued" },
    { id: "mxc", name: "Apply MXC policies", detail: "Learning mode · 11 rules", state: "queued" },
    { id: "smoke", name: "Smoke-test inference", detail: "planner → coder → gateway", state: "queued" },
  ];
}

export const GPU_COLORS: Record<string, string> = {
  a4000: "#4a9eff",
  "p100-0": "#a371f7",
  "p100-1": "#39c5cf",
};

/** Sample Optimize page state for browser mode: the consult card
 *  figures and a stored sample plan exercising the row states
 *  (valid / warning / invalid, a denied delete, an advice row). */
export function mockOptimizeState(): OptimizeState {
  const item = (
    id: number, kind: string, target: string | null,
    change: Record<string, unknown>, extra: Partial<OptimizePlanItem> = {},
  ): OptimizePlanItem => ({
    id, fingerprint: `sample-${id}`, kind, target, change,
    advice_text: null, rationale: "", expected_effect: "", confidence: "medium",
    decision: "undecided", previously_denied: false, apply_outcome: null, apply_error: null,
    ...extra,
  });
  const validation = (status: "valid" | "warning" | "invalid", reason: string | null = null,
    gpu: [number, number] | null = null, sys: [number, number] | null = null, note: string | null = null): OptimizeValidation => ({
    status, reason, expected: { gpu_ram_mib: gpu, sys_ram_mib: sys, note },
  });
  return {
    purpose: "",
    last_directive: "Explore better MoE capable models for the planner",
    purpose_is_seed: true,
    purpose_seed: "Local inference served to the LAN through one gateway. Groups: planner (Qwen3.8-27B Planner); coder (Qwen3-Coder-Next-80B).",
    groups: [
      { name: "planner", purpose: "" },
      { name: "coder", purpose: "" },
      { name: "coder-fast", purpose: "" },
      { name: "tool-runner", purpose: "" },
    ],
    backends: [
      { id: "planner", purpose: "", running: true, enabled: true, model_id: "qwen3-27b-planner", short_name: "Qwen3.8-27B Planner", measured: { prompt_per_second: 812, predicted_per_second: 21.8, ttft_ms: 240, age_secs: 3600 } },
      { id: "coder", purpose: "", running: true, enabled: true, model_id: "qwen3-coder-30b", short_name: "Qwen3-Coder-Next-80B", measured: null },
      { id: "coder-fast", purpose: "", running: false, enabled: false, model_id: null, short_name: null, measured: null },
      { id: "tool-runner", purpose: "", running: false, enabled: false, model_id: null, short_name: null, measured: null },
    ],
    context_summary: {
      gpus: 2, backends: 4, groups: 4, models: 3,
      measured_blocks: ["backend running state and per-process VRAM", "gateway traffic"],
      traffic_window_label: "1 h",
    },
    context: { sample: true },
    default_backend: "planner",
    better_backend: null,
    consulting: false,
    measuring: false,
    plan: {
      summary: "Sample plan — generated locally; no backend was consulted.",
      directive: "Explore better MoE capable models for the planner",
      consulted_backend: "planner",
      created_at: new Date().toISOString(),
      config_hash: "sample",
      dropped_invalid: 1,
      truncated: 0,
      dropped_fragments: ["{\"kind\":\"backend.set_offload\",\"target\":\"coder\",\"change\":{\"offload_layers\":\"most\"}}"],
      raw_response: "{\"summary\": \"Sample plan\", \"items\": [ ... ]}",
      context: { sample: true },
      config_stale: false,
      sample: true,
      items: [
        item(1, "backend.set_context", "planner", { n_ctx: 65536 }, {
          rationale: "Planner requests regularly approach the current 32K context; doubling it doubles the KV cache, which the A4000 has room for.",
          confidence: "high",
        }),
        item(2, "backend.set_offload", "coder", { offload_layers: 48 }, {
          rationale: "Moving all MoE layers to system RAM frees the P100 entirely; system RAM is plentiful on this machine.",
          confidence: "medium",
        }),
        item(3, "group.set_strategy", "coder", { strategy: "least-busy" }, {
          rationale: "Least-busy spreads requests across group members instead of shuffling.",
          confidence: "medium",
        }),
        item(4, "model.add", "sample-model", { source_url: "sample/sample-model-Q4_K_M.gguf", quant: "Q4_K_M" }, {
          rationale: "Registers a model for later evaluation; nothing is downloaded.",
          confidence: "low",
        }),
        item(5, "backend.delete", "coder-fast", {}, {
          rationale: "The coder-fast backend has never run; removing it simplifies the layout.",
          decision: "denied",
          confidence: "low",
        }),
        item(6, "advice", null, {}, {
          advice_text: "Consider enabling the tool-runner backend before routing tool calls through the coder group; the seat exists in the layout but is disabled.",
        }),
      ],
      validations: [
        validation("valid", null, [9216, 17408], null),
        validation("warning", "fits now, but not alongside every configured backend when all are running", [44032, 2048], [40960, 45056]),
        validation("valid"),
        validation("warning", "registers the model only — the source URL and any hash are model-supplied and unverified; confirm the pinned hash in the Models view before downloading"),
        validation("valid", null, null, null, "Backend will be stopped and removed"),
        validation("valid"),
      ],
    },
  };
}
