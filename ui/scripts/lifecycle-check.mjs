#!/usr/bin/env node
/* Unit checks for the shared lifecycle module's pure rules
   (ui/src/lifecycle-state.ts and the pure helpers in lifecycle.ts):
   normalizers, META, toStatus, the action rule, summarize, the system
   verdict, the pending overlay and the fast-poll trigger. The module is
   bundled in memory with esbuild (vite's own dependency) and imported
   in Node; nothing touches a DOM or Tauri. Run: npm run test:lifecycle */
import { build } from "esbuild";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { strict as assert } from "node:assert";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const out = await build({
  entryPoints: [join(root, "src", "lifecycle.ts")],
  bundle: true,
  format: "esm",
  platform: "neutral",
  write: false,
  logLevel: "silent",
});
const src = out.outputFiles[0].text;
const L = await import("data:text/javascript;base64," + Buffer.from(src).toString("base64"));

let failures = 0;
function check(name, fn) {
  try {
    fn();
    console.log(`ok   ${name}`);
  } catch (e) {
    failures++;
    console.error(`FAIL ${name}\n     ${e.message.split("\n").join("\n     ")}`);
  }
}

check("fromWire accepts the unified vocabulary unchanged", () => {
  for (const s of L.LIFECYCLE_STATES) assert.equal(L.fromWire(s), s);
});
check("fromWire maps the old process and SCM vocabularies", () => {
  assert.equal(L.fromWire("start_pending"), "starting");
  assert.equal(L.fromWire("stop_pending"), "stopping");
  assert.equal(L.fromWire("paused"), "degraded");
  assert.equal(L.fromWire({ unknown: 7 }), "unknown");
  assert.equal(L.fromWire("not registered"), "not_installed");
  assert.equal(L.fromWire(null), "unknown");
  assert.equal(L.fromWire(undefined), "unknown");
  assert.equal(L.fromWire("bogus"), "unknown");
});
check("fromScm maps every SCM state", () => {
  assert.equal(L.fromScm("running"), "running");
  assert.equal(L.fromScm("continue_pending"), "starting");
  assert.equal(L.fromScm("pause_pending"), "starting");
  assert.equal(L.fromScm("stopped"), "stopped");
  assert.equal(L.fromScm("not_installed"), "not_installed");
  assert.equal(L.fromScm({ unknown: 4 }), "unknown");
});
check("toStatus keeps stopping/restarting distinct from GPU degraded", () => {
  assert.equal(L.toStatus("stopping"), "stopping");
  assert.equal(L.toStatus("restarting"), "restarting");
  assert.equal(L.toStatus("failed"), "down");
  assert.equal(L.toStatus("stopped"), "idle");
  assert.equal(L.toStatus("running"), "ok");
});
check("META: transitions are amber, stopped grey, failed red; isLive matches the Rust rule", () => {
  for (const s of ["starting", "restarting", "stopping"]) {
    assert.equal(L.META[s].isTransition, true);
    assert.equal(L.META[s].dot, "warn");
    assert.equal(L.META[s].tag, "status-transition");
  }
  assert.equal(L.META.stopped.dot, "idle");
  assert.equal(L.META.stopped.tag, "status-off");
  assert.equal(L.META.failed.tag, "status-bad");
  const live = L.LIFECYCLE_STATES.filter((s) => L.META[s].isLive);
  assert.deepEqual(live.sort(), ["degraded", "restarting", "running", "starting"]);
});
check("lifecycleLabel names origin and age of a transition", () => {
  const now = Date.parse("2026-01-01T00:00:12Z");
  const t = { kind: "restarting", origin: "activation", since: "2026-01-01T00:00:00Z" };
  assert.equal(L.lifecycleLabel("restarting", t, now), "restarting… (activation, 12 s)");
  assert.equal(L.lifecycleLabel("degraded"), "not responding");
});
check("lifecycleActions: the one Start/Stop/Restart rule", () => {
  const acts = (s, ctx = { kind: "backend" }) => L.lifecycleActions(s, ctx).map((a) => `${a.action}:${a.disabled ? "off" : "on"}`);
  assert.deepEqual(acts("running"), ["stop:on", "restart:on"]);
  assert.deepEqual(acts("degraded"), ["stop:on", "restart:on"]);
  assert.deepEqual(acts("starting"), ["stop:on", "restart:on"]);
  assert.deepEqual(acts("restarting"), ["stop:off", "restart:off"]);
  assert.deepEqual(acts("stopping"), ["stop:off", "restart:off"]);
  assert.deepEqual(acts("stopped"), ["start:on", "restart:off"]);
  assert.deepEqual(acts("failed", { kind: "backend", enabled: false }), ["start:off", "restart:off"]);
  assert.deepEqual(acts("stopped", { kind: "backend", configured: false }), ["start:off", "restart:off"]);
  assert.deepEqual(acts("not_installed"), []);
  assert.deepEqual(acts("unknown"), []);
  assert.deepEqual(acts("running", { kind: "postgres" }), ["stop:on"]);
  assert.deepEqual(acts("running", { kind: "postgres", blockedReason: "Stop the gateway first" }), ["stop:off"]);
  assert.ok(L.lifecycleActions("restarting", { kind: "gateway" })[0].reason);
  // A running disabled backend or orphan: Stop yes, Restart refused.
  assert.deepEqual(acts("running", { kind: "backend", enabled: false }), ["stop:on", "restart:off"]);
  assert.deepEqual(acts("degraded", { kind: "backend", configured: false }), ["stop:on", "restart:off"]);
  assert.ok(L.lifecycleActions("running", { kind: "backend", enabled: false })[1].reason);
  // The gateway's status call failed: its page stays operable.
  assert.deepEqual(acts("unknown", { kind: "gateway" }), ["start:on", "stop:on", "restart:on"]);
  assert.ok(L.lifecycleActions("unknown", { kind: "gateway" })[0].reason);
});
check("applyBlockedReason blocks gateway and backend actions during an apply", () => {
  assert.equal(L.applyBlockedReason({ kind: "gateway" }, true), "An activation apply is running");
  assert.equal(L.applyBlockedReason({ kind: "backend", id: "a" }, true), "An activation apply is running");
  assert.equal(L.applyBlockedReason({ kind: "log-receiver" }, true), undefined);
  assert.equal(L.applyBlockedReason({ kind: "gateway" }, false), undefined);
});
check("summarize counts enabled + configured backends only", () => {
  const s = L.summarize({
    backends: [
      { state: "running" },
      { lifecycle: "restarting", state: "restarting" },
      { state: "failed" },
      { state: "running", enabled: false },
      { state: "running", configured: false },
      { state: "stopped" },
    ],
    gateway: "running",
  });
  assert.deepEqual(s, { backendsTotal: 4, backendsRunning: 1, backendsTransitioning: 1, backendsFailed: 1, gateway: "running" });
});
check("systemVerdict: a restarting gateway is amber, never 'Gateway down'", () => {
  const sum = (running, total, gateway, extra = {}) => ({ backendsTotal: total, backendsRunning: running, backendsTransitioning: 0, backendsFailed: 0, gateway, ...extra });
  const v = (summary, stale = false, error = null) => L.systemVerdict({ summary, stale, error });
  assert.deepEqual(v(sum(2, 2, "running")), { dot: "ok", text: "All systems normal", ok: true });
  assert.deepEqual(v(sum(2, 2, null)), { dot: "ok", text: "All systems normal", ok: true });
  assert.deepEqual(v(sum(2, 2, "restarting")), { dot: "warn", text: "Gateway restarting…", ok: false });
  assert.deepEqual(v(sum(2, 2, "starting")), { dot: "warn", text: "Gateway starting…", ok: false });
  assert.deepEqual(v(sum(2, 2, "stopped")), { dot: "idle", text: "Gateway stopped", ok: false });
  assert.deepEqual(v(sum(2, 2, "failed")), { dot: "err", text: "Gateway down", ok: false });
  assert.deepEqual(v(sum(2, 2, "degraded")), { dot: "warn", text: "Gateway not responding", ok: false });
  assert.equal(v(sum(2, 2, "running"), true).text, "Telemetry stale");
  assert.equal(v(sum(2, 2, "running"), false, "x").dot, "err");
  assert.equal(v(sum(0, 0, "running")).text, "No backends configured");
  assert.deepEqual(v(sum(0, 2, "stopped")), { dot: "idle", text: "All backends stopped · gateway stopped", ok: false });
  assert.deepEqual(v(sum(0, 2, "restarting")), { dot: "warn", text: "All backends stopped · gateway restarting…", ok: false });
  assert.deepEqual(v(sum(1, 2, "running", { backendsTransitioning: 1 })), { dot: "warn", text: "1 / 2 backends running · 1 changing state", ok: false });
  assert.equal(v(sum(1, 2, "running", { backendsFailed: 1 })).dot, "err");
});
check("isBusyError recognizes the server's Busy refusals", () => {
  assert.ok(L.isBusyError("gateway restart already in progress (started by activation 4 s ago)"));
  assert.ok(L.isBusyError("an activation apply started by desktop is running (step backend)"));
  assert.ok(!L.isBusyError("spawn failed"));
  assert.ok(!L.isBusyError(undefined));
});
check("targetKey / parseTargetKey round-trip the backend registry keys", () => {
  for (const t of [{ kind: "backend", id: "coder-7b" }, { kind: "gateway" }, { kind: "postgres" }, { kind: "log-receiver" }]) {
    assert.deepEqual(L.parseTargetKey(L.targetKey(t)), t);
  }
  assert.equal(L.targetKey({ kind: "backend", id: "a" }), "backend:a");
  assert.equal(L.parseTargetKey("backend:"), null);
  assert.equal(L.parseTargetKey("nope"), null);
});
check("overlayState: pending wins until the server reports a transition", () => {
  const p = { kind: "restarting", since: Date.now() };
  assert.equal(L.overlayState("running", p), "restarting");
  assert.equal(L.overlayState("stopped", p), "restarting");
  assert.equal(L.overlayState("starting", p), "starting");
  assert.equal(L.overlayState("running", undefined), "running");
});
check("snapshotBusy triggers fast polling on any transition or apply", () => {
  const base = { backends: [{ id: "a", state: "running" }], gateway: { state: "running", transition: null, activation_applying: null, postgres: null }, services: null, logReceiver: null, pending: new Map() };
  assert.equal(L.snapshotBusy(base), false);
  assert.equal(L.snapshotBusy({ ...base, pending: new Map([["gateway", { kind: "restarting", since: 0 }]]) }), true);
  assert.equal(L.snapshotBusy({ ...base, backends: [{ id: "a", state: "stopping" }] }), true);
  assert.equal(L.snapshotBusy({ ...base, gateway: { ...base.gateway, activation_applying: { step: "plan" } } }), true);
  assert.equal(L.snapshotBusy({ ...base, logReceiver: { registered: true, state: "start_pending" } }), true);
});
check("renderers emit the shared classes and escape details", () => {
  assert.equal(L.lifecycleDot("failed"), `<span class="dot err"></span>`);
  const chip = L.lifecycleChip("restarting", { transition: { kind: "restarting", origin: "ui", since: "" }, detail: "pid <1>" });
  assert.match(chip, /class="chip status-tag status-transition"/);
  assert.match(chip, /restarting…<\/span>$/);
  assert.match(chip, /pid &lt;1&gt;/);
  const btns = L.renderLifecycleActions({ kind: "backend", id: "x\"y" }, "stopped", { enabled: true });
  assert.match(btns, /data-lc-act="start" data-lc-target="backend:x&quot;y"/);
  assert.match(btns, /data-lc-act="restart"[^>]* disabled/);
});
check("serviceRowState prefers lifecycle, falls back to SCM state", () => {
  assert.equal(L.serviceRowState({ registered: true, state: "stopped", lifecycle: "failed" }), "failed");
  assert.equal(L.serviceRowState({ registered: true, state: "stop_pending" }), "stopping");
  assert.equal(L.serviceRowState({ registered: false, state: null }), "not_installed");
});

if (failures) {
  console.error(`\n${failures} lifecycle check(s) failed`);
  process.exit(1);
}
console.log("\nall lifecycle checks passed");
