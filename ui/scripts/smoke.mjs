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
for (const cmd of ["get_status", "list_backends", "get_gpu_telemetry", "run_wizard_step", "setup_get_steps", "run_audit", "setup_mark_complete", "start_backend", "stop_backend", "restart_backend", "backend_get_launch_command", "backend_apply_config", "backend_set_offload", "backend_test_request", "list_models", "download_model", "cancel_download", "model_pause_download", "model_resume_download", "model_verify_checksum", "model_set_short_name", "model_delete", "model_assign", "get_logs", "placement_propose", "placement_apply", "diagnostics_export_bundle", "get_gateway_config", "gateway_status", "gateway_stop", "gateway_preview_yaml", "update_gateway_config", "get_config", "get_mxc_policy", "mxc_set_mode", "mxc_run_self_test", "telemetry_reset_peaks", "telemetry_save_snapshot", "telemetry_set_interval"]) {
  check(bundle.includes(cmd), `bundle references Tauri command "${cmd}"`);
}
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
