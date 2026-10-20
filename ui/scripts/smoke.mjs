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
for (const cmd of ["get_status", "list_backends", "get_gpu_telemetry", "run_wizard_step", "setup_get_steps", "run_audit", "setup_mark_complete", "start_backend", "stop_backend", "restart_backend", "backend_get_launch_command", "backend_apply_config", "backend_set_offload", "backend_set_mtp", "backend_set_parallel", "backend_add", "backend_add_preview", "test_backend", "list_models", "download_model", "cancel_download", "model_pause_download", "model_resume_download", "model_verify_checksum", "model_set_short_name", "model_set_offload_spec", "model_delete", "model_assign", "get_logs", "placement_propose", "placement_apply", "diagnostics_export_bundle", "get_gateway_config", "gateway_status", "gateway_stop", "gateway_preview_yaml", "update_gateway_config", "get_config", "get_mxc_policy", "mxc_set_mode", "mxc_run_self_test", "telemetry_reset_peaks", "telemetry_save_snapshot", "telemetry_set_interval", "optimize_measure_backend", "optimize_estimate_backend", "optimize_run_experiments", "optimize_estimate_experiments", "measurement_lookup", "measurement_delete", "gateway_set_backend_group", "gateway_create_group", "gateway_delete_group", "get_gpu_history"]) {
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
// The Optimize page header links to its own log scope (tracker
// #52): the scope+follow fragment must survive the build.
check(bundle.includes("#/logs?scope=optimize&follow=1"), "bundle contains the Optimize logs deep link (#/logs?scope=optimize&follow=1)");
// Logs session state + recent searches (tracker #59): the view's
// controls live in module-level session state so in-app navigation
// restores them as left; applied search sets are recorded into a
// capped, localStorage-persisted recents list (the dashboard
// view-mode pattern) applied from an expandable section.
check(bundle.includes("Recent searches"), "bundle contains the Recent searches section label");
check(bundle.includes("llm.logs.recents"), "bundle contains the Logs recents storage key");
const logsSrc = readFileSync(join(root, "src", "views", "static.ts"), "utf8");
check(logsSrc.includes("let logsViewState"), "Logs session state lives at module level in static.ts");
check(logsSrc.includes("function recordLogSearch"), "Logs recents recording exists in static.ts");
check(logsSrc.includes("LOGS_RECENTS_CAP = 30"), "Logs recents cap is 30 in static.ts");
check(logsSrc.includes('id="logRecents"'), "Logs view renders the recents section container");
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
// Add-backend launch parameters (tracker #49): the add form hosts
// the shared flags fields under its own draft attribute, renders
// the offload slider through the shared component, and collects a
// placement selection; the compiled fragments of that contract
// must survive the build.
check(bundle.includes("data-add-flags-field"), "bundle contains the add form's flags fields (data-add-flags-field)");
check(bundle.includes("data-add-place-gpu"), "bundle contains the add form's placement chips (data-add-place-gpu)");
check(allSrc.includes("function flagsCtxBatchHtml"), "shared flagsCtxBatchHtml renderer exists in ui/src");
check(allSrc.includes("function offloadSliderHtml"), "shared offloadSliderHtml renderer exists in ui/src");

// View scroll contract (tracker #50): the shell never scrolls
// (body overflow:hidden, #app height:100vh); each view's content
// must render inside the .view container (flex:1; overflow-y:auto)
// or anything past the fold is clipped with no way to reach it.
// The Optimize view once rendered #optRoot bare, leaving a long
// plan unreachable — pin #optRoot inside .view in the source.
check(readFileSync(join(root, "src", "views", "optimize.ts"), "utf8").includes('class="view"><div id="optRoot"'),
  "Optimize view renders #optRoot inside the .view scroll container");

// Gateway-stats banner contract (tracker #58): the "statistics are
// not available yet" banner explains a transient that only exists
// while the gateway is up and the metrics poller has not yet landed
// two scrapes. It must stay gated on the gateway being up — shown
// while the gateway is down, it blames a wait that cannot end.
check(readFileSync(join(root, "src", "views", "dashboard.ts"), "utf8").includes("m.gateway !== null && m.gatewayUp &&"),
  "dashboard gateway-stats banner requires the gateway to be up");

// Optimize Estimate (tracker #56): the Estimate control sits
// beside Measure on each running backend's row, the experiment
// estimate action exists, and estimates render under their own
// label — the source fragments of that contract must survive.
const optimizeSrc = readFileSync(join(root, "src", "views", "optimize.ts"), "utf8");
check(optimizeSrc.includes("data-opt-estimate"), "Optimize view renders the per-backend Estimate button (data-opt-estimate)");
check(optimizeSrc.includes('data-opt-act="estimate-experiments"'), "Optimize view renders the Estimate experiments action");
check(optimizeSrc.includes("Estimate —"), "Optimize view renders estimate figures under the Estimate label");

// Optimize fix bundle (Oct 19, 2026): the consulting elapsed
// clock reads the server's recorded start (a page instance that
// did not start the run has no local start and must not render
// the epoch as an elapsed time); a recorded consultation
// failure surfaces as a banner with the stored plan headed
// "Previous plan"; measure/estimate targets render as discrete
// cards with their actions anchored in the card's action row,
// not as one flex row of running text.
check(optimizeSrc.includes("consult_started_unix_ms"), "Optimize view reads the server-recorded consultation start");
check(optimizeSrc.includes("consult_error"), "Optimize view surfaces the server-recorded consultation failure");
check(optimizeSrc.includes("Previous plan"), "Optimize view heads the retained plan 'Previous plan' after a failed consultation");
check(optimizeSrc.includes('class="opt-measure-card"'), "Optimize view renders measure targets as cards");
check(optimizeSrc.includes('class="opt-measure-actions"'), "Optimize view anchors card actions in the card action row");
check(!optimizeSrc.includes('class="opt-measure-row"'), "Optimize view no longer renders the single-row measure blob");

// Groups page (predicted-throughput design §8): the membership
// editor's lanes are drop targets built from the gateway config,
// its cards are drag sources, and both drag-and-drop and the
// "Move to" menu call the one move op; the Gateway Routing card
// cross-links to the editor.
const groupsSrc = readFileSync(join(root, "src", "views", "groups.ts"), "utf8");
check(groupsSrc.includes("data-group-lane-drop"), "Groups view renders lanes as drop targets");
check(groupsSrc.includes("data-group-card"), "Groups view renders backend cards");
check(groupsSrc.includes("draggable=\"true\""), "Groups view cards are draggable");
check(groupsSrc.includes("data-group-move"), "Groups view renders Move to menu items");
check(groupsSrc.includes("gatewaySetBackendGroup"), "Groups view moves membership through gateway_set_backend_group");
check(bundle.includes("#/groups"), "bundle contains the Groups route (#/groups)");
check(readFileSync(join(root, "src", "views", "static.ts"), "utf8").includes("Edit membership"),
  "Gateway Routing card links to the Groups editor (Edit membership)");


// GPU power absence states: the telemetry payload classifies a null
// power reading as the driver answering N/A (hardware state) or the
// field missing from the source output (bug state). "Not measured"
// is the driver state's wording only — the GPUs card and the
// dashboard render it solely under that classification — and a
// measured draw renders with its limit alongside.
check(logsSrc.includes('power_absence === "driver_not_available"'), "GPUs view gates the 'not measured' power value on the driver-N/A state");
check(logsSrc.includes("/ ${trim1(g.power_limit_w)} W"), "GPUs card renders the power draw with the limit alongside");
check(readFileSync(join(root, "src", "api.ts"), "utf8").includes("power_absence: s.power_absence ?? undefined"),
  "telemetry adapt maps power_absence from the payload");
check(readFileSync(join(root, "src", "views", "dashboard.ts"), "utf8").includes('power_absence === "driver_not_available" ? "not measured" : "—"'),
  "dashboard power value gates 'not measured' on the driver-N/A state");

// GPU history charts: the GPUs view renders a History section with
// a 1h/24h/7d range selector and one chart per metric, fed by
// get_gpu_history; the chart helper breaks series at time gaps and
// unmeasured values instead of interpolating across them.
const gpuHistSrc = readFileSync(join(root, "src", "views", "static.ts"), "utf8");
check(gpuHistSrc.includes('id="gpuHistory"'), "GPUs view renders the history section container");
check(gpuHistSrc.includes("data-hist-range"), "GPUs history renders the range selector (data-hist-range)");
check(gpuHistSrc.includes("data-hist-metric"), "GPUs history renders per-metric chart containers (data-hist-metric)");
check(gpuHistSrc.includes("getGpuHistory(histRange)"), "GPUs history loads through get_gpu_history");
check(bundle.includes("Throughput (tok/s)"), "bundle contains the history Throughput chart title");
check(bundle.includes("No GPU history recorded yet"), "bundle contains the empty-history state");
const chartsSrc = readFileSync(join(root, "src", "charts.ts"), "utf8");
check(chartsSrc.includes("export function historyChart"), "shared historyChart renderer exists in charts.ts");
check(chartsSrc.includes("3 * s.intervalMs"), "historyChart breaks series at gaps wider than 3x the nominal interval");

// MoE offload declaration for user-added models: the Add Model
// form carries the optional declaration pair, model cards render
// the effective Expert offload figure, and the card's declaration
// editor saves through model_set_offload_spec.
check(bundle.includes("addModelMoeLayers"), "bundle contains the Add Model MoE layers field");
check(bundle.includes("addModelMoeDefault"), "bundle contains the Add Model default offload field");
check(bundle.includes("MoE layers"), "bundle contains the MoE declaration field label");
const staticSrc = readFileSync(join(root, "src", "views", "static.ts"), "utf8");
check(staticSrc.includes('data-model-act="offload"'), "Models view renders the offload declaration editor button");
check(staticSrc.includes('data-model-act="offload-save"'), "Models view renders the declaration save action");
check(staticSrc.includes("Expert offload"), "Models view renders the Expert offload card figure");

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
