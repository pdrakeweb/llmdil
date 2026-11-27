import { describe, expect, it } from "vitest";
import type { ModelRow, ModelStorageConfig, ModelStorageRow } from "../src/api";
import { isColdTier } from "../src/api";
import { mockModelStorageStatus } from "../src/mock";
import {
  fmtAgo, lostTiersHtml, modelStorageActionsHtml, modelStorageChipsHtml, modelStorageTilesHtml, moveButtonsHtml, storageCardHtml,
  storageCtx, storageMoving, tierDisplayName,
} from "../src/views/modelStorage";
import { asStorageConfig, modelStoragePanelHtml, storageConfigFromDraft, storageDraftFrom } from "../src/views/settingsPanels";

const base: ModelStorageConfig = {
  cold_tiers: [{ path: "E:\\llm-models", label: "SATA" }, { path: "F:\\more", label: "" }],
  hot_budget_gib: null, hot_min_free_gib: 100, promote_after_starts: 2, promote_window_days: 14,
  demote_after_idle_days: 30, keep_hot: ["draft"], download_tier: "hot", copy_rate_mib_s: 256, auto: true,
};
const storage = (o: Partial<ModelStorageRow> = {}): ModelStorageRow => ({
  tier: "hot", path: "C:\\data\\models\\x.gguf", present_in: ["hot"], archived: false, cold_unavailable: false,
  pinned: null, keep_hot: false, moving: null, last_started: null, ...o,
});
const row = (s: ModelStorageRow | null): ModelRow => ({ id: "m1", name: "M1", storage: s } as ModelRow);
const COLD = { hasCold: true, coldAvailable: true };

describe("tiered storage: model card", () => {
  it("chips: Cold gray, hot is the default, move amber, Unavailable light red", () => {
    expect(modelStorageChipsHtml(storage())).toBe("");
    expect(modelStorageChipsHtml(storage({ tier: "cold:1" }))).toContain(">Cold<");
    expect(modelStorageChipsHtml(storage({ tier: "missing", cold_unavailable: true }))).toMatch(/badge-err[^>]*>Unavailable/);
    expect(modelStorageChipsHtml(storage({ archived: true }))).toContain(">Archived<");
    expect(modelStorageChipsHtml(storage({ tier: "cold", archived: true }))).not.toContain("Archived");
    const mv = modelStorageChipsHtml(storage({ tier: "cold", moving: { kind: "promote", fraction: 0.428, paused: true, task_id: "t" } }));
    expect(mv).toContain("badge-warn\">Promoting 42%");
    expect(mv).toContain(">Paused<");
    expect(modelStorageChipsHtml(storage({ moving: { kind: "demote", queued: true } }))).toContain(">Queued<");
  });
  it("Promote when cold, Demote when hot with a cold tier, disabled with the pin reason", () => {
    expect(modelStorageActionsHtml(row(storage({ tier: "cold" })), COLD)).toContain('data-model-act="promote"');
    expect(modelStorageActionsHtml(row(storage()), COLD)).toContain('data-model-act="demote"');
    expect(modelStorageActionsHtml(row(storage()), { hasCold: false, coldAvailable: false })).toBe("");
    const pinned = modelStorageActionsHtml(row(storage({ pinned: "used by enabled backend coder" })), COLD);
    expect(pinned).toContain("disabled");
    expect(pinned).toContain("used by enabled backend coder");
    expect(modelStorageActionsHtml(row(storage({ moving: { kind: "promote", fraction: 0.1 } })), COLD)).toContain('data-model-act="move-cancel"');
    for (const b of [pinned, modelStorageActionsHtml(row(storage({ tier: "cold" })), COLD)]) {
      expect(b).toMatch(/aria-label="[^"]+"/);
      expect(b).toMatch(/title="[^"]+"/);
    }
  });
  it("Keep hot is the state switch; a backend pin shows disabled-on", () => {
    expect(modelStorageTilesHtml(row(storage()), { hasCold: false, coldAvailable: false })).not.toContain("Keep hot");
    const on = modelStorageTilesHtml(row(storage({ keep_hot: true, pinned: "keep hot" })), COLD);
    expect(on).toContain('aria-checked="true"');
    expect(on).not.toContain(" disabled");
    const byBackend = modelStorageTilesHtml(row(storage({ pinned: "used by enabled backend coder" })), COLD);
    expect(byBackend).toContain('aria-checked="true"');
    expect(byBackend).toContain(" disabled");
    expect(byBackend).toContain("used by enabled backend coder");
    expect(modelStorageTilesHtml(row(storage()), COLD)).not.toContain("Last started");
    expect(modelStorageTilesHtml(row(storage({ last_started: "2026-11-25T00:00:00Z" })), COLD)).toContain("Last started");
  });
  it("relative times", () => {
    const now = Date.parse("2026-11-26T12:00:00Z");
    expect(fmtAgo("2026-11-26T11:59:30Z", now)).toBe("just now");
    expect(fmtAgo("2026-11-26T11:15:00Z", now)).toBe("45 min ago");
    expect(fmtAgo("2026-11-26T07:00:00Z", now)).toBe("5 h ago");
    expect(fmtAgo("2026-11-25T12:00:00Z", now)).toBe("1 day ago");
  });
});

describe("tiered storage: Storage card", () => {
  it("renders nothing without a status, an error line on failure", () => {
    expect(storageCardHtml(null, null, [], { unregOpen: false, movesOpen: false, note: null })).toBe("");
    expect(storageCardHtml(null, "boom", [], { unregOpen: false, movesOpen: false, note: null })).toContain("Storage unavailable: boom");
  });
  it("one bar per tier, reserve marker, move progress, unregistered behind a toggle", () => {
    const st = mockModelStorageStatus();
    const closed = storageCardHtml(st, null, [], { unregOpen: false, movesOpen: false, note: null });
    expect(closed.match(/<svg[^>]*viewBox="0 0 600 12"/g)).toHaveLength(2);
    expect(closed).toContain("Free-space reserve: 100 GiB");
    expect(closed).toContain('data-storage-act="move-cancel"');
    expect(closed).toContain("Unregistered files: 1");
    expect(closed).not.toContain('data-storage-target="models\\scratch-merge-Q5_K_M.gguf" title="Promote');
    const open = storageCardHtml(st, null, [], { unregOpen: true, movesOpen: false, note: null });
    expect(open).toContain('data-storage-act="promote" data-storage-target="models\\scratch-merge-Q5_K_M.gguf"');
    const gone = { ...st, tiers: st.tiers.map((t) => t.tier === "cold" ? { ...t, available: false, free_bytes: null, total_bytes: null, files: [], model_bytes: 0 } : t) };
    expect(storageCardHtml(gone, null, [], { unregOpen: false, movesOpen: false, note: null })).toMatch(/badge-err[^>]*>Unavailable/);
  });
  it("warns about a lost cold tier with a button to add it back", () => {
    const st = mockModelStorageStatus();
    const ui = { unregOpen: false, movesOpen: false, note: null };
    expect(storageCardHtml(st, null, [], ui)).not.toContain("restore-tier");
    const lost = { ...st, lost_tiers: [{ path: "E:\\llm-models", files: 8, bytes: 121.8e9 }] };
    const html = storageCardHtml(lost, null, [], ui);
    expect(html).toContain("8 model files");
    expect(html).toContain("no longer a configured cold tier");
    expect(html).toContain('data-storage-act="restore-tier" data-storage-target="E:\\llm-models"');
    expect(lostTiersHtml({ ...st, lost_tiers: [{ path: "D:\\x", files: 1, bytes: 1 }] })).toContain("1 model file (");
  });
  it("context and polling", () => {
    const st = mockModelStorageStatus();
    expect(storageCtx(st)).toEqual({ hasCold: true, coldAvailable: true });
    expect(storageCtx(null)).toEqual({ hasCold: false, coldAvailable: false });
    expect(storageMoving(st, [])).toBe(true);
    expect(storageMoving(null, [row(storage())])).toBe(false);
    expect(tierDisplayName({ tier: "cold:1", label: "" })).toBe("Cold 2");
    expect(isColdTier("cold:1")).toBe(true);
    expect(isColdTier("missing")).toBe(false);
    expect(moveButtonsHtml({ actAttr: "data-x", idAttr: "data-y", target: "t", name: "t", tier: "missing", moving: null, pinned: null, ctx: COLD })).toBe("");
  });
});

describe("tiered storage: Settings panel", () => {
  it("round-trips the config through the draft, keeping what the form does not edit", () => {
    const r = storageConfigFromDraft(storageDraftFrom(base), base);
    expect(r).toEqual({ config: base });
  });
  it("blank means none / never; blank path drops only the first tier", () => {
    const d = { ...storageDraftFrom(base), budget: "", demoteIdle: "", coldPath: "" };
    const r = storageConfigFromDraft(d, base);
    expect("config" in r && r.config).toMatchObject({ hot_budget_gib: null, demote_after_idle_days: null, cold_tiers: [base.cold_tiers[1]] });
  });
  it("refuses invalid fields with the field named", () => {
    const bad = (o: Partial<ReturnType<typeof storageDraftFrom>>): string => {
      const r = storageConfigFromDraft({ ...storageDraftFrom(base), ...o }, base);
      return "error" in r ? r.error : "";
    };
    expect(bad({ promoteStarts: "0" })).toMatch(/^Promote after/);
    expect(bad({ minFree: "1.5" })).toMatch(/^Free-space reserve/);
    expect(bad({ budget: "0" })).toMatch(/^Hot budget/);
    expect(bad({ coldPath: "relative\\dir" })).toMatch(/^Cold tier/);
    const none = { ...base, cold_tiers: [] };
    const r = storageConfigFromDraft({ ...storageDraftFrom(none), downloadTier: "cold" }, none);
    expect("error" in r && r.error).toMatch(/^Download tier/);
  });
  it("reads a section with defaults, and renders values then the form", () => {
    expect(asStorageConfig(undefined)).toBeNull();
    expect(asStorageConfig({})).toMatchObject({ hot_min_free_gib: 100, demote_after_idle_days: 30, auto: true, download_tier: "hot" });
    expect(asStorageConfig({ demote_after_idle_days: null })?.demote_after_idle_days).toBeNull();
    const view = modelStoragePanelHtml(base, null);
    expect(view).toContain('role="switch"');
    expect(view).toContain("E:\\llm-models");
    expect(view).toContain('data-ms-act="edit"');
    expect(view).not.toContain("data-ms-field");
    const form = modelStoragePanelHtml(base, storageDraftFrom(base));
    expect(form).toContain('data-ms-field="coldPath"');
    expect(form).toContain('data-ms-act="save"');
  });
});
