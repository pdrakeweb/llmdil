#!/usr/bin/env node
/* Smoke test for the llm-manager-ui production bundle.
   Asserts:
   1. dist/index.html exists and references the JS bundle.
   2. The emitted JS bundle references all expected Tauri command names.
   3. The bundle is a single self-contained asset (no external URLs).
*/
import { readFileSync, existsSync, readdirSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const dist = join(root, "dist");
let failures = 0;

function check(cond, msg) {
  if (cond) console.log(`ok   ${msg}`);
  else { console.error(`FAIL ${msg}`); failures++; }
}

const indexHtml = join(dist, "index.html");
check(existsSync(indexHtml), "dist/index.html exists");

const assetsDir = join(dist, "assets");
let jsFiles = [];
if (existsSync(assetsDir)) {
  jsFiles = readdirSync(assetsDir).filter((f) => f.endsWith(".js"));
}
check(jsFiles.length > 0, `dist/assets contains a JS bundle (found ${jsFiles.length})`);

const html = existsSync(indexHtml) ? readFileSync(indexHtml, "utf8") : "";
check(/assets\/.*\.js/.test(html), "index.html references the JS bundle");

const bundle = jsFiles.map((f) => readFileSync(join(assetsDir, f), "utf8")).join("\n");
for (const cmd of ["get_status", "list_backends", "get_gpu_telemetry", "run_wizard_step", "setup_get_steps", "run_audit", "setup_mark_complete", "start_backend", "stop_backend", "restart_backend", "backend_get_launch_command", "backend_apply_config", "backend_set_offload", "test_backend", "list_models", "download_model", "cancel_download", "model_pause_download", "model_resume_download", "model_verify_checksum", "model_set_short_name", "model_delete", "model_assign", "get_logs", "placement_propose", "placement_apply", "diagnostics_export_bundle", "get_gateway_config", "gateway_status", "gateway_stop", "gateway_preview_yaml", "update_gateway_config", "get_config", "get_mxc_policy", "mxc_set_mode", "mxc_run_self_test", "telemetry_reset_peaks", "telemetry_save_snapshot", "telemetry_set_interval"]) {
  check(bundle.includes(cmd), `bundle references Tauri command "${cmd}"`);
}
// Cross-check: every Tauri command name the UI invokes must be
// registered in src-tauri's generate_handler! list. A mismatch is
// silent at build time and only fails at runtime in the installed app
// (this is how the UI once invoked "backend_test_request" while the
// registered command was "test_backend"). Extraction covers the two
// dispatch idioms used in ui/src: direct invoke<...>("name", ...) and
// the backendAction cmd-union forwarding to invoke(cmd, ...).
const mainRsPath = join(root, "..", "src-tauri", "src", "main.rs");
let registered = new Set();
try {
  const mainRs = readFileSync(mainRsPath, "utf8");
  const block = mainRs.split("generate_handler![")[1].split("])")[0].replace(/\/\/.*/g, "");
  // Entries may be module paths (optimize::optimize_get_state); Tauri
  // registers the bare function name, so compare on the last segment.
  registered = new Set(block.split(",").map((s) => s.trim().split("::").pop()).filter(Boolean));
  // The shell's own admin_api_* commands are listed literally in its
  // handler macro; collect them by name wherever they appear.
  for (const m of mainRs.matchAll(/admin_api_[a-z0-9_]+/g)) {
    registered.add(m[0]);
  }
  // The shell generates its handlers from the manager-app operation
  // registry (for_each_operation!), so the registered set is the
  // registry's snake names plus the shell's own literal entries above.
  const registryRs = readFileSync(join(root, "..", "crates", "manager-app", "src", "registry.rs"), "utf8");
  for (const m of registryRs.matchAll(/\(\w+,\s*([a-z][a-z0-9_]*),\s*(?:R|C|D|L|X|S\w*),/g)) {
    registered.add(m[1]);
  }
} catch { /* empty set fails the check below */ }
check(registered.size > 0, `parsed registered Tauri commands from generate_handler! (${registered.size})`);

function walkTs(dir) {
  const out = [];
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, e.name);
    if (e.isDirectory()) out.push(...walkTs(p));
    else if (e.name.endsWith(".ts")) out.push(p);
  }
  return out;
}
const invokedAt = new Map(); // name -> first "file:line" site
for (const file of walkTs(join(root, "src"))) {
  const src = readFileSync(file, "utf8");
  const rel = file.slice(root.length + 1);
  const lineOf = (idx) => src.slice(0, idx).split("\n").length;
  for (const m of src.matchAll(/\binvoke\b/g)) {
    const mm = /\(\s*"([a-z][a-z0-9_]*)"/.exec(src.slice(m.index, m.index + 500));
    if (mm && !invokedAt.has(mm[1])) invokedAt.set(mm[1], `${rel}:${lineOf(m.index)}`);
  }
  for (const m of src.matchAll(/cmd:\s*((?:"[a-z0-9_]+"(?:\s*\|\s*)?)+)/g)) {
    for (const q of m[1].matchAll(/"([a-z0-9_]+)"/g)) {
      if (!invokedAt.has(q[1])) invokedAt.set(q[1], `${rel}:${lineOf(m.index)}`);
    }
  }
  for (const m of src.matchAll(/backendAction\("([a-z0-9_]+)"/g)) {
    if (!invokedAt.has(m[1])) invokedAt.set(m[1], `${rel}:${lineOf(m.index)}`);
  }
}
const unregistered = [...invokedAt].filter(([name]) => !registered.has(name));
check(unregistered.length === 0,
  `every invoked command name is registered (${invokedAt.size} invoked, ${registered.size} registered)` +
  (unregistered.length ? ` — unregistered: ${unregistered.map(([n, s]) => `${n} at ${s}`).join(", ")}` : ""));

// Go-to-logs deep links (tracker #47): entity cards link into the
// Logs view with the entity's scope preselected and Follow on; the
// compiled fragments of that link contract must survive the build.
check(bundle.includes("#/logs?scope="), "bundle contains the Logs deep link (#/logs?scope=)");
check(bundle.includes("&follow=1"), "bundle contains the Logs deep-link follow flag (&follow=1)");
// Enable/disable controls are the shared state switch (design rule
// 16): the bundle must carry the switch markup contract, the shared
// component must exist, both views that host toggles must render
// through it, and no source may still render the old action-icon
// toggle (slash on enabled entities, check on disabled ones).
check(bundle.includes('role="switch"'), "bundle contains the enable/disable switch (role=\"switch\")");
check(bundle.includes("aria-checked"), "bundle exposes switch state via aria-checked");
const allSrc = walkTs(join(root, "src")).map((f) => readFileSync(f, "utf8")).join("\n");
check(allSrc.includes("function enableSwitch"), "shared enableSwitch component exists in ui/src");
check(!allSrc.includes('? "ban" : "check"'), "no action-icon enable/disable toggle remains in ui/src");
for (const view of ["views/static.ts", "views/mcp.ts"]) {
  check(readFileSync(join(root, "src", view), "utf8").includes("enableSwitch("),
    `${view} renders its enable/disable controls via enableSwitch`);
}

// View scroll contract (tracker #50): the shell never scrolls
// (body overflow:hidden, #app height:100vh); each view's content
// must render inside the .view container (flex:1; overflow-y:auto)
// or anything past the fold is clipped with no way to reach it.
// The Optimize view once rendered #optRoot bare, leaving a long
// plan unreachable — pin #optRoot inside .view in the source.
check(readFileSync(join(root, "src", "views", "optimize.ts"), "utf8").includes('class="view"><div id="optRoot"'),
  "Optimize view renders #optRoot inside the .view scroll container");

check(!bundle.includes("live data wiring pending"), "bundle contains no wiring-pending mock banner");
check(!bundle.includes("not wired to a backend command yet"), "bundle contains no unwired-control message");
check(bundle.includes("__TAURI__"), "bundle contains the Tauri detection hook");

// The UI detects Tauri via window.__TAURI__, which Tauri only injects when
// app.withGlobalTauri is true. Without it the installed app silently
// degrades to sample data on every screen.
const tauriConfPath = join(root, "..", "src-tauri", "tauri.conf.json");
let withGlobalTauri = false;
try {
  withGlobalTauri = JSON.parse(readFileSync(tauriConfPath, "utf8"))?.app?.withGlobalTauri === true;
} catch { /* unreadable/missing config fails the check below */ }
check(withGlobalTauri, "tauri.conf.json sets app.withGlobalTauri=true (without it window.__TAURI__ is absent in the installed app and the UI silently degrades to sample data)");

const external = bundle.match(/https?:\/\/[a-z0-9.-]+\//gi) || [];
const allowed = external.filter((u) => !u.includes("localhost"));
check(allowed.length === 0, `no external asset URLs in bundle (found ${allowed.length})`);

check(bundle.length > 20_000, `bundle is non-trivial (${bundle.length} bytes)`);

if (failures > 0) { console.error(`\n${failures} smoke check(s) failed`); process.exit(1); }
console.log("\nsmoke: all checks passed");
