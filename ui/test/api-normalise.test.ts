import { afterEach, describe, expect, it, vi } from "vitest";
import type { GatewayConfigData, OpenRouterSpend } from "../src/api";
import {
  getDashboard, getGatewayStatus, getGpuTelemetry, getStatus, isTauri, listBackends,
  openRouterSpendBannerHtml, systemVerdict, wizardBusyMessage,
} from "../src/api";

/* The Tauri boundary: `window.__TAURI__.core.invoke` present means live
   mode; absent means sample (mock-data) mode. Tests stub `window` with
   a fake invoke that answers from a per-command table. */

type Handler = (args?: Record<string, unknown>) => unknown;

function stubTauri(handlers: Record<string, Handler>): void {
  vi.stubGlobal("window", {
    __TAURI__: {
      core: {
        invoke: async (cmd: string, args?: Record<string, unknown>) => {
          const h = handlers[cmd];
          if (!h) throw new Error(`unhandled command ${cmd}`);
          return h(args);
        },
      },
    },
  });
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("sample-mode detection", () => {
  it("is not Tauri without __TAURI__", () => {
    vi.stubGlobal("window", {});
    expect(isTauri()).toBe(false);
  });
  it("is Tauri when core.invoke is a function", () => {
    stubTauri({});
    expect(isTauri()).toBe(true);
  });
  it("is not Tauri when invoke is not a function", () => {
    vi.stubGlobal("window", { __TAURI__: { core: { invoke: "nope" } } });
    expect(isTauri()).toBe(false);
  });
  it("reports live=false and sample data outside Tauri", async () => {
    vi.stubGlobal("window", {});
    const s = await getStatus();
    expect(s.live).toBe(false);
    expect(s.error).toBeUndefined();
    expect(s.data.version).toMatch(/sample/);
    const b = await listBackends();
    expect(b.live).toBe(false);
    expect(b.data.length).toBeGreaterThan(0);
    const d = await getDashboard();
    expect(d.live).toBe(false);
  });
  it("reports live=true with an error (never sample data) when an invoke fails", async () => {
    stubTauri({ get_status: () => { throw new Error("boom"); } });
    const s = await getStatus();
    expect(s).toEqual({ data: { status: "unreachable", version: "?" }, live: true, error: "boom" });
    const b = await listBackends();
    expect(b.live).toBe(true);
    expect(b.data).toEqual([]);
    expect(b.error).toMatch(/unhandled command list_backends/);
  });
});

describe("list_backends normalisation", () => {
  it("maps supervisor state to the dashboard vocabulary and drops non-numeric stats", async () => {
    stubTauri({
      list_backends: () => ({
        backends: [
          { id: "a", port: 8081, model_file: "a.gguf", state: "running", adopted: false, enabled: true, configured: true, service_managed: false, requests_1h: 12, errors_1h: null, spark: [1, 2] },
          { id: "b", port: 8082, model_file: "b.gguf", state: "stopping", adopted: false, enabled: true, configured: true, service_managed: false },
          { id: "c", port: 8083, model_file: "c.gguf", state: "who-knows", adopted: true, enabled: false, configured: false, service_managed: false },
        ],
      }),
    });
    const r = await listBackends();
    expect(r.live).toBe(true);
    // Shared lifecycle vocabulary: a stopping backend reads "stopping".
    expect(r.data.map((b) => b.status)).toEqual(["ok", "stopping", "idle"]);
    expect(r.data[0]).toMatchObject({ id: "a", name: "a", model: "a.gguf", requests_1h: 12, spark: [1, 2] });
    expect(r.data[0].errors_1h).toBeUndefined();
    expect(r.data[0].reqSpark).toBeUndefined();
    expect(r.data[2].configured).toBe(false);
  });
  it("treats a missing backends array as empty", async () => {
    stubTauri({ list_backends: () => ({}) });
    expect((await listBackends()).data).toEqual([]);
  });
});

describe("get_gpu_telemetry normalisation", () => {
  const sample = (over: Record<string, unknown> = {}) => ({
    index: 0, name: "RTX", ts: "t", utilization_pct: 50, vram_used_mib: 8192, vram_total_mib: 24576, temp_c: 60,
    power_w: null, power_absence: "driver_not_available", stale: false, source: "nvidiasmi",
    mem_util_pct: null, fan_pct: 30, power_limit_w: null, temp_peak_c: 71, util_spark: [1], temp_spark: [1, 2],
    ...over,
  });

  it("converts MiB to GiB, nulls to undefined, and attributes processes", async () => {
    stubTauri({
      get_gpu_telemetry: () => ({
        samples: [sample(), sample({ index: 1, stale: true, source: "nvml" })],
        processes: [
          { gpu_index: 0, pid: 10, name: "llama-server", vram_mib: null, used_mib: 2048, backend_id: "a" },
          { gpu_index: 0, pid: 11, name: "", vram_mib: null, backend_id: null },
          { gpu_index: 1, pid: 12, name: "x", vram_mib: 100, backend_id: null },
        ],
        at_ms: 1234,
        source_error: null,
      }),
    });
    const r = await getGpuTelemetry();
    expect(r.live).toBe(true);
    const [g0, g1] = r.data.gpus;
    expect(g0).toMatchObject({ id: "0", vram_used_gb: 8, vram_total_gb: 24, temp_peak_c: 71, fan_pct: 30, source: "nvidia-smi", status: "ok" });
    expect(g0.power_limit_w).toBeUndefined();
    expect(g0.mem_util_pct).toBeUndefined();
    // A one-point spark is not a sparkline.
    expect(g0.util_spark).toBeUndefined();
    expect(g0.temp_spark).toEqual([1, 2]);
    expect(g0.processes).toEqual([
      { process: "llama-server", pid: 10, vram_mib: 2048, backend: "a" },
      // Unmeasured VRAM stays null, never 0.
      { process: "—", pid: 11, vram_mib: null, backend: "—" },
    ]);
    expect(g1).toMatchObject({ id: "1", status: "degraded", source: "NVML" });
    expect(r.data.stale).toBe(true);
    expect(r.data.atMs).toBe(1234);
    expect(r.data.hasPolled).toBe(true);
    expect(r.data.source).toBe("nvidia-smi");
  });
  it("flags a down GPU and distinguishes 'not polled yet' from 'no GPUs'", async () => {
    stubTauri({ get_gpu_telemetry: () => ({ samples: [sample({ down: true, stale: true })], at_ms: null }) });
    expect((await getGpuTelemetry()).data.gpus[0].status).toBe("down");
    stubTauri({ get_gpu_telemetry: () => ({ samples: [], at_ms: null, has_polled: false }) });
    const r = await getGpuTelemetry();
    expect(r.data).toMatchObject({ gpus: [], stale: false, hasPolled: false, atMs: null, source: null });
  });
});

describe("gateway_status normalisation", () => {
  it("fills defaults for a sparse payload and parses the spend block", async () => {
    stubTauri({
      gateway_status: () => ({
        port: 4000, up: 1, lan_ip: 7,
        openrouter_spend: { enabled: true, tier_active: true, spent_today_usd: 4.2, cap_usd: 5, fraction: 0.84, capped: false, warn: true },
      }),
    });
    const r = await getGatewayStatus();
    expect(r.data).toMatchObject({
      port: 4000, up: true, groups: 0, bind_host: "0.0.0.0", lan_ip: null, openrouter_cred_present: null, litellm_version: null,
    });
    expect(r.data.openrouter_spend).toEqual({ enabled: true, tier_active: true, spent_today_usd: 4.2, cap_usd: 5, fraction: 0.84, capped: false, warn: true });
  });
  it("coerces non-finite spend numbers and a missing spend block", async () => {
    stubTauri({ gateway_status: () => ({ openrouter_spend: { enabled: true, spent_today_usd: "x", fraction: NaN } }) });
    const spend = (await getGatewayStatus()).data.openrouter_spend;
    expect(spend).toMatchObject({ spent_today_usd: 0, cap_usd: 0, fraction: null, capped: false });
    stubTauri({ gateway_status: () => ({}) });
    expect((await getGatewayStatus()).data.openrouter_spend).toBeNull();
  });
});

describe("systemVerdict", () => {
  // The verdict takes the shared lifecycle summary (service-lifecycle
  // merge); a down gateway is a `failed` gateway lifecycle state.
  type Old = { total: number; running: number; stale: boolean; error: string | null; gatewayUp: boolean };
  const v = (o: Partial<Old>) => {
    const i: Old = { total: 2, running: 2, stale: false, error: null, gatewayUp: true, ...o };
    return systemVerdict({
      summary: {
        backendsTotal: i.total,
        backendsRunning: i.running,
        backendsTransitioning: 0,
        backendsFailed: 0,
        gateway: i.gatewayUp ? "running" : "failed",
      },
      stale: i.stale,
      error: i.error,
    });
  };
  it("is normal only when everything is up", () => {
    expect(v({})).toEqual({ dot: "ok", text: "All systems normal", ok: true });
  });
  it("orders the failure states", () => {
    expect(v({ error: "x", total: 0 }).text).toBe("Backend error");
    expect(v({ total: 0, running: 0 }).text).toBe("No backends configured");
    expect(v({ stale: true, gatewayUp: false }).text).toBe("Telemetry stale");
    expect(v({ gatewayUp: false }).text).toBe("Gateway down");
    expect(v({ running: 0 })).toEqual({ dot: "idle", text: "All backends stopped", ok: false });
    expect(v({ running: 1, gatewayUp: false }).text).toBe("1 / 2 backends running · gateway down");
  });
});

describe("wizardBusyMessage", () => {
  it("names the running step and humanises elapsed time", () => {
    expect(wizardBusyMessage({}, "fallback")).toBe("fallback");
    expect(wizardBusyMessage({ running_step: "Download" }, "f")).toMatch(/^Download has been running — /);
    expect(wizardBusyMessage({ running_step: "S", elapsed_secs: 45 }, "f")).toMatch(/^S has been running for 45s/);
    expect(wizardBusyMessage({ running_step: "S", elapsed_secs: 720 }, "f")).toMatch(/for 12 min/);
    expect(wizardBusyMessage({ running_step: "S", elapsed_secs: 3900 }, "f")).toMatch(/for 1 h 5 min/);
    expect(wizardBusyMessage({ running_step: "S", elapsed_secs: 7200 }, "f")).toMatch(/for 2 h —/);
  });
});

describe("openRouterSpendBannerHtml", () => {
  const spend = (o: Partial<OpenRouterSpend>): OpenRouterSpend =>
    ({ enabled: true, tier_active: true, spent_today_usd: 1, cap_usd: 5, fraction: 0.2, capped: false, warn: false, ...o });
  it("is empty when disabled, absent, or under the warning line", () => {
    expect(openRouterSpendBannerHtml(null, "!")).toBe("");
    expect(openRouterSpendBannerHtml(spend({ enabled: false, capped: true }), "!")).toBe("");
    expect(openRouterSpendBannerHtml(spend({}), "!")).toBe("");
  });
  it("warns at >= 80% with the rounded percentage", () => {
    const html = openRouterSpendBannerHtml(spend({ warn: true, spent_today_usd: 4.25, fraction: 0.85 }), "!");
    expect(html).toContain("$4.25 of the $5.00 daily cap (85%)");
  });
  it("explains a reached cap and a zero cap", () => {
    expect(openRouterSpendBannerHtml(spend({ capped: true, spent_today_usd: 5 }), "!")).toContain("reached the $5.00 daily cap");
    expect(openRouterSpendBannerHtml(spend({ capped: true, cap_usd: 0 }), "!")).toContain("the OpenRouter daily cap is $0.00");
  });
});
