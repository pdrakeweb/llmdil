/* Built-in sample data used when the UI is NOT running under Tauri
   (plain browser). Mirrors the values in the approved mockups so the
   layout can be evaluated without a backend. */

import { walk } from "./charts";
import type { BackendInfo, DashboardModel, GpuInfo, WizardStepState } from "./types";

const C = { blue: "#4a9eff", purple: "#a371f7", teal: "#39c5cf", grey: "#6d7686" };

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
      { model: "qwen3.8-27b", backendId: "planner", segments: [{ gpu: "a4000", pct: 60 }, { gpu: "p100-0", pct: 40 }] },
      { model: "qwen3-coder-30b", backendId: "coder-30b", segments: [{ gpu: "a4000", pct: 45 }, { gpu: "p100-0", pct: 35 }, { gpu: "p100-1", pct: 20 }] },
      { model: "qwen2.5-coder-7b", backendId: "coder-7b", segments: [{ gpu: "p100-0", pct: 100 }] },
      { model: "qwen3-8b", backendId: "tool-runner", segments: [{ gpu: "p100-1", pct: 100 }] },
    ],
    throughputSeries: [
      { name: "planner", color: C.blue, values: walk(48, 8.4, 2.5, 201) },
      { name: "coder-30b", color: C.purple, values: walk(48, 22.6, 5, 202) },
      { name: "coder-7b", color: C.teal, values: walk(48, 41.2, 7, 203) },
      { name: "tool-runner", color: C.grey, values: walk(48, 1, 1, 204) },
    ],
    gatewayUp: true,
    gatewayStrategy: "least-busy",
    events: [
      { time: "06:13:52", level: "warn", text: "mcp-gdrive reconnect 3 · backoff 30s" },
      { time: "06:12:10", level: "ok", text: "coder-7b queue normalized" },
      { time: "06:05:44", level: "ok", text: "download resumed qwen2.5-coder-7b · 62%" },
      { time: "06:01:19", level: "ok", text: "planner · 1,204 requests done" },
      { time: "05:30:00", level: "idle", text: "health sweep · all responding" },
    ],
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
