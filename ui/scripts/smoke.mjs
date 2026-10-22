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
for (const cmd of ["get_status", "list_backends", "get_gpu_telemetry", "run_wizard_step", "setup_get_steps", "run_audit", "setup_mark_complete", "start_backend", "stop_backend", "restart_backend", "backend_get_launch_command", "backend_apply_config", "backend_set_offload", "backend_set_mtp", "backend_set_parallel", "backend_set_cache", "backend_cache_preview", "backend_add", "backend_add_preview", "test_backend", "list_models", "download_model", "cancel_download", "model_pause_download", "model_resume_download", "model_verify_checksum", "model_set_short_name", "model_set_offload_spec", "model_delete", "model_assign", "get_logs", "placement_propose", "placement_apply", "diagnostics_export_bundle", "get_gateway_config", "gateway_status", "gateway_stop", "gateway_preview_yaml", "update_gateway_config", "get_config", "get_mxc_policy", "mxc_set_mode", "mxc_run_self_test", "telemetry_reset_peaks", "telemetry_save_snapshot", "telemetry_set_interval", "optimize_measure_backend", "optimize_estimate_backend", "optimize_run_experiments", "optimize_estimate_experiments", "measurement_lookup", "measurement_delete", "gateway_set_backend_group", "gateway_create_group", "gateway_delete_group", "get_gpu_history", "update_config", "updates_pin", "updates_set_auto", "app_update_status", "app_update_check", "app_update_set_checking", "app_update_set_auto", "app_update_download", "app_update_restart"]) {
  check(bundle.includes(cmd), `bundle references Tauri command "${cmd}"`);
}
// Nav completeness: the sidebar is generated from the ROUTES array
// in src/main.ts, so every route's hash and label must survive into
// the built bundle. Nothing else here renders or enumerates the
// nav, so a dropped or renamed route would pass the whole harness
// while the installed app silently loses a page (the Optimize nav
// item was reported missing against v0.1.42; that instance was a
// stale running process, but the harness gap is real). Parse the
// array from the source so this tracks the source of truth instead
// of a second hand-maintained list.
const shellSrc = readFileSync(join(root, "src", "main.ts"), "utf8");
const routesBlock = shellSrc.split("const ROUTES")[1]?.split("];")[0] ?? "";
const navRoutes = [...routesBlock.matchAll(/\{\s*hash:\s*"([^"]+)",\s*label:\s*"([^"]+)"/g)]
  .map((m) => ({ hash: m[1], label: m[2] }));
check(navRoutes.length > 0, `parsed nav routes from ROUTES in src/main.ts (${navRoutes.length})`);
function escapeRegExp(s) { return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"); }
for (const r of navRoutes) {
  check(bundle.includes(r.hash), `bundle contains the nav route hash "${r.hash}"`);
  check(bundle.includes(r.label), `bundle contains the nav route label "${r.label}"`);
  // The bound entry is the part that can actually fail: the bare
  // hash also appears in the router's dispatch switch in main.ts
  // and labels appear in view titles, so either string alone
  // survives a route dropped from ROUTES. Only the compiled
  // object literal carrying hash and label together proves the
  // nav data itself survived the build.
  const entry = new RegExp(`hash\\s*:\\s*"${escapeRegExp(r.hash)}"\\s*,\\s*label\\s*:\\s*"${escapeRegExp(r.label)}"`);
  check(entry.test(bundle), `bundle contains the compiled ROUTES entry for "${r.hash}" (${r.label})`);
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
// Groups drag-and-drop fix (Oct 20, 2026): the failure was at the
// window level, not the lane markup — Tauri's OS file-drop handler
// is enabled by default and on Windows intercepts every drag in
// the webview, so no dragover ever reached the page (the OS showed
// the no-drop cursor everywhere). dragDropEnabled must be false —
// the same parsed-config pin style as withGlobalTauri below. The
// harness renders no DOM, so a synthetic dragover event cannot be
// driven here; the acceptance path is pinned by its source
// fragments instead: the drop attribute sits on the lane <section>
// itself (so closest() resolves a hit anywhere in the lane), the
// dragover handler resolves the lane through that attribute and
// calls preventDefault, a repaint never replaces the lanes DOM
// mid-drag (that destroys the drag source), and the poll does not
// run while a drag is in flight.
let dragDropDisabled = false;
try {
  dragDropDisabled = JSON.parse(readFileSync(join(root, "..", "src-tauri", "tauri.conf.json"), "utf8"))
    ?.app?.windows?.[0]?.dragDropEnabled === false;
} catch { /* unreadable/missing config fails the check below */ }
check(dragDropDisabled, "tauri.conf.json sets dragDropEnabled=false on the main window (Tauri's default OS file-drop handler intercepts HTML5 drag-and-drop on Windows)");
check(groupsSrc.includes('<section class="group-lane" data-group-lane-drop='), "Groups lanes carry the drop-target attribute on the lane element itself");
check(groupsSrc.includes("function laneFromEvent"), "Groups view resolves drop lanes through a shared laneFromEvent helper");
check(groupsSrc.includes('t.closest("[data-group-lane-drop]")'), "Groups lane resolution keys off the lane drop attribute via closest()");
const dragOverSrc = groupsSrc.slice(groupsSrc.indexOf("function onDragOver"), groupsSrc.indexOf("function onDragLeave"));
check(dragOverSrc.includes("laneFromEvent(ev)") && dragOverSrc.includes("ev.preventDefault()"),
  "Groups dragover handler accepts a drag over a lane (preventDefault after lane resolution)");
check(groupsSrc.includes('root.addEventListener("dragleave", onDragLeave)'), "Groups view clears the lane highlight on dragleave");
check(groupsSrc.includes("paintDeferredDuringDrag"), "Groups view defers repaints requested while a drag is in flight");
check(groupsSrc.includes("if (!disposed && !pending && !dragId) void load();"), "Groups poll repaint is suppressed while a drag is in flight");
// Groups lanes flow (Oct 20, 2026, design-principles rule 18):
// the lanes container wraps onto additional rows and never
// scrolls horizontally.
const stylesSrc = readFileSync(join(root, "src", "styles.css"), "utf8");
const lanesRule = stylesSrc.split(".groups-lanes {")[1]?.split("}")[0] ?? "";
check(lanesRule.includes("flex-wrap: wrap"), "Groups lanes container wraps lanes onto additional rows (flex-wrap: wrap)");
check(!lanesRule.includes("overflow-x: auto"), "Groups lanes container has no horizontal scroll (no overflow-x: auto)");
check(bundle.includes("#/groups"), "bundle contains the Groups route (#/groups)");
check(readFileSync(join(root, "src", "views", "static.ts"), "utf8").includes("Edit membership"),
  "Gateway Routing card links to the Groups editor (Edit membership)");

// Backend card declutter (Oct 20, 2026): the card face carries no
// standalone Test tool calls button and no Logs link — both are
// items in the card's Other-actions menu; the edit entry point is
// the single "Edit" (Placement is a section of the Edit session,
// not its own menu item); the card face has no model-mutating
// dropdown — the Model tile is a value and the choice is a draft
// in the Edit panel, applied through model_assign only when the
// panel saves; the Details expansion renders below the actions
// bar and carries the tool-call state, an Offload Layers field,
// and a latest-log-lines block fed by the open-Details tail
// fetch. The harness renders no DOM, so these pin the source
// fragments of that contract, in the established style.
const backendSrc = readFileSync(join(root, "src", "views", "static.ts"), "utf8");
const cardFnSrc = backendSrc.slice(backendSrc.indexOf("function backendCard("), backendSrc.indexOf("export function renderBackends"));
check(cardFnSrc.includes('data-back-menu-item data-back-act="toolcall"'), "backend card menu carries the Test tool calls item");
check(cardFnSrc.includes("View all logs"), "backend card menu carries the View all logs item");
check(!cardFnSrc.includes("const toolCallBtn"), "backend card face has no standalone Test tool calls button");
check(!cardFnSrc.includes("const logsBtn"), "backend card face has no standalone Logs button");
check(cardFnSrc.includes('data-back-menu-item data-back-act="flags"'), "backend card menu carries the Edit entry point");
check(cardFnSrc.includes("} Edit</button>"), "the backend card's edit entry point is labeled 'Edit'");
check(!cardFnSrc.includes("Edit flags</button>"), "no 'Edit flags' entry point remains on the backend card");
check(!cardFnSrc.includes('data-back-act="placement"'), "backend card menu has no separate Placement item (Placement is a section of Edit)");
check(!cardFnSrc.includes("data-back-model"), "backend card face has no model-mutating dropdown");
check(cardFnSrc.includes("data-edit-model"), "Edit panel carries the model choice as a draft select (data-edit-model)");
check(backendSrc.includes("const saveEditModel"), "the Edit save applies the model draft through model_assign (saveEditModel)");
check(!backendSrc.includes("onModelChange"), "no assign-on-selection model change handler remains");
check(cardFnSrc.indexOf('class="card-actions backend-actions"') >= 0
  && cardFnSrc.indexOf('class="card-actions backend-actions"') < cardFnSrc.indexOf("${detailsHtml}"),
  "the Details expansion renders below the actions bar");
check(cardFnSrc.includes('["Tool calls"'), "Details carries the tool-call state");
check(cardFnSrc.includes('["Offload Layers"'), "Details carries the Offload Layers field");
check(cardFnSrc.includes("Latest log lines"), "Details carries the latest-log-lines block");
check(backendSrc.includes("openDetails.has(row.id)"), "log tails fetch for backends whose Details are open");
check(backendSrc.includes("const closeEditSession"), "a successful Edit save closes the whole Edit surface (closeEditSession)");
check((backendSrc.split("closeEditSession();").length - 1) >= 3, "both Edit saves and the Edit toggle close through closeEditSession");
check(backendSrc.includes('if (mres === "failed") { paint(); return; }'), "a failed model assignment keeps the Edit surface open with its input intact");
check(bundle.includes("View all logs"), "bundle contains the View all logs menu label");
check(bundle.includes("Latest log lines"), "bundle contains the Latest log lines caption");


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

// Add Model form UX (Oct 20, 2026): the control that made the
// form appear and the form's submit control carried the same
// label, and a successful submit left the filled form on screen.
// The form now follows the Groups/Add-Backend conventions: the
// topbar's "Add model" button (plus icon, as "New group") opens
// it, the submit is "Create model" (check icon, as "Create
// group"), and the form lives in a #modelAddForm host (the Add
// Backend pattern) — a successful create empties the host, which
// clears and hides the form, while a failed create returns ahead
// of the close with the operator's input untouched. The harness
// is static and cannot drive a DOM submit, so the outcome
// behavior is pinned by the handler's source fragments: the
// close helper empties the host and is invoked only from the
// open toggle and the success branch (Cancel passes the function
// reference), with the failure branch's error write ahead of it.
check(bundle.includes('id="modelAdd"'), "bundle contains the Models topbar Add model open control");
check(bundle.includes("Create model"), "bundle contains the Add Model submit label (Create model)");
check(staticSrc.includes('id="modelAdd" title="Add model">${icon("plus")} Add model'), "Models topbar opens the form via the Add model button");
check(staticSrc.includes('id="addModelBtn">${icon("check")} Create model'), "Add Model submit is labeled Create model, distinct from the open control");
check(!staticSrc.includes('id="addModelBtn">${icon("plus")} Add model'), "Add Model submit no longer carries the open control's label");
check(staticSrc.includes('id="modelAddForm"'), "Models view renders the add-form host (#modelAddForm)");
check(staticSrc.includes("function modelAddFormHtml"), "Add Model form renders from the shared form template");
check(staticSrc.includes('addModelHost.innerHTML = ""'), "closing the Add Model form empties its host (every field clears with it)");
check((staticSrc.match(/closeAddModelForm\(\)/g) || []).length === 2, "Add Model form close is invoked only from the open toggle and the success branch");
// Multi-line fragments are matched on an LF-normalized copy: the
// Windows runner checks the sources out with CRLF endings.
const staticSrcLf = staticSrc.replace(/\r\n/g, "\n");
check(staticSrcLf.includes("closeAddModelForm();\n      await load();"), "Add Model success branch closes the form before repainting the list");
const addFailIdx = staticSrcLf.indexOf("`Add failed: ${r.error}`");
const addCloseIdx = staticSrcLf.indexOf("closeAddModelForm();\n      await load();");
check(addFailIdx >= 0 && addCloseIdx > addFailIdx, "Add Model failure branch writes the error ahead of the success-only close (input preserved)");
const renderModelsSrc = staticSrc.split("export function renderModels")[1]?.split("export async function wireModels")[0] ?? "";
check(renderModelsSrc.includes('id="modelAddForm"') && !renderModelsSrc.includes("addModelId"),
  "Models shell renders only the add-form host (the form itself appears on open)");

// Form close on save (design rule 17, Pete 2026-10-20): saving
// or applying a form closes it and shows the object in its new
// state; a failed save keeps the form open with the operator's
// input intact and the error shown. The rule is codified in the
// design doc, and the surfaces this build fixed are pinned
// below. The harness is static and cannot drive a DOM save to
// success or failure, so — like the Add Model pins above — the
// outcome behavior is pinned by handler source fragments: a
// close/clear that appears only in (or only after) the success
// branch, and a failure branch that returns ahead of any close
// or repaint that would discard input.
const designDoc = readFileSync(join(root, "..", "docs", "design-principles.md"), "utf8");
check(designDoc.includes("17. **Saving a form closes it**"), "design doc codifies rule 17 (saving a form closes it)");
check(designDoc.includes("17. **Form close on save**"), "design doc audit checklist carries the rule 17 item");
check(designDoc.includes("Multi-step flows (the setup wizard) advance through steps rather than closing"),
  "rule 17 states the wizard exclusion (no over-application to multi-step flows)");
const groupsSrcLf = groupsSrc.replace(/\r\n/g, "\n");
check(groupsSrcLf.includes('value="${esc(formDraft)}"'), "Groups create form renders its name input from the wire draft");
check(groupsSrcLf.includes('if (t.id === "groupsNewName") formDraft = (t as HTMLInputElement).value'),
  "Groups create form syncs typed input into the wire draft (repaints cannot lose it)");
check(groupsSrcLf.includes("if (r.error) { formError = r.error; paintForm(); return; }"),
  "Groups create failure keeps the form open with the error (draft re-renders intact)");
check(groupsSrcLf.includes('formDraft = "";\n    setStatus(`Group ${name} created.`);'),
  "Groups create success clears the draft and closes the form before the list repaint");
check(staticSrcLf.includes("const patchRules = async (rules: GatewayRule[], okMsg: string): Promise<boolean>"),
  "Gateway patchRules reports whether the save persisted");
check(staticSrcLf.includes("const saved = await patchRules(rules,"),
  "Gateway rule save awaits the persisted outcome before deciding to close");
check(staticSrcLf.includes("if (saved && !disposed) {\n        ruleForm.open = false;"),
  "Gateway rule form closes only on a persisted save (rule 17)");
check(!staticSrcLf.includes("ruleForm.open = false;\n      await patchRules"),
  "Gateway rule form no longer closes ahead of the save outcome");
check(staticSrcLf.includes('if (r.error) { gwMsg(`Change not saved: ${r.error}`, true); return; }'),
  "Gateway gwSave failure returns without a repaint (permanent-field input preserved)");
check(staticSrcLf.includes("gwMsg(okText);\n      await load();"),
  "Gateway gwSave success repaints the field in its new saved state");
check(staticSrcLf.includes('if (r.error) { setMsg(`Create user failed: ${r.error}`, true); return; }'),
  "Settings Create user failure returns without a repaint (input preserved)");
check(staticSrcLf.includes('if (r.error) { setMsg(`Role change failed: ${r.error}`, true); return; }'),
  "Settings role Apply failure returns without a repaint (chosen role preserved)");
// Already-compliant baselines this build did not redo, pinned so
// a later change cannot silently regress them: the MCP server
// form (closes on success, draft intact on failure) and the
// Optimize purpose editor (closes on success, draft intact on
// failure).
const mcpSrcFc = readFileSync(join(root, "src", "views", "mcp.ts"), "utf8").replace(/\r\n/g, "\n");
check(mcpSrcFc.includes("closeForm();\n    await load();"), "MCP server form closes on successful save before repainting the list (baseline)");
check(mcpSrcFc.includes("formError = r.error;\n      paintForm();"), "MCP server form failure keeps the form open with the draft intact (baseline)");
check(optimizeSrc.replace(/\r\n/g, "\n").includes("if (r.error) { statusLine = r.error; paint(); return; }"),
  "Optimize purpose save failure keeps the editor open with the error (baseline)");
check(staticSrcLf.includes("closeEditSession();\n          await load();"),
  "Backend Edit success closes the Edit session before repainting the card (baseline)");
// KV cache / flash-attention controls (v0.1.46, audited in the
// rule 17 completion pass, Oct 21, 2026): in the Edit form they
// are commit-on-change sub-controls in the MTP/parallel idiom —
// a rejected commit reverts the draft to the saved selection
// with the error on the card and the Edit session stays open;
// a landed commit shows the saved tokens in the card message
// and reloads. In the Add form they are draft fields: the draft
// lives in wire state (pCache), and the refused pairing is
// rejected at submit with the error in the form.
check(staticSrcLf.includes("KV cache not saved: ${r.error}"),
  "KV cache commit failure surfaces the error on the card (rule 17)");
check(staticSrcLf.includes("if (!ok && !disposed) {\n      flagsCacheSel = saved;"),
  "KV cache rejected commit reverts the draft to the saved selection (rule 17)");
check(staticSrcLf.includes("KV cache saved (${cacheTokenText(sel)})"),
  "KV cache successful commit reports the saved selection on the card (rule 17)");
check(staticSrcLf.includes("pCache = sel; paintParams();"),
  "Add-form KV selects sync into the wire draft (repaints cannot lose it, rule 17)");
check((staticSrcLf.match(/A quantized V cache requires flash attention/g) || []).length >= 2,
  "Quantized-V + flash-attention-off is named and refused in both the Edit commit path and the Add submit validation (rule 17)");

// Predicted throughput field + refresh (Oct 20, 2026): the
// field renders on EVERY backend card (the Backends stat tile
// and the Groups mini card), with an explicit "Not measured"
// empty state when no exact measurement resolves — design
// §7's absent-tile behavior superseded for cards by Pete's
// direction that the field be present even when empty. The
// Refresh control beside it dispatches the existing Optimize
// Measure job (the Measure profile, whose entries feed the
// prediction), never the Estimate profile, and renders
// disabled with the not-running reason for a stopped backend;
// while a refresh runs, the field carries a Measuring elapsed
// state. Consultation/benchmark mutual exclusion (tracker
// #70): a benchmark that starts while a consultation runs on
// the same backend is refused, naming the running operation —
// and the reverse — pinned at the Rust source.
check(bundle.includes("predict-refresh"), "bundle contains the predicted-throughput Refresh control");
check(staticSrc.includes('data-back-act="predict-refresh"'), "Backends card renders the predicted-throughput Refresh control");
check(staticSrc.includes("Not measured</span>"), "Backends card renders the predicted-throughput empty state (Not measured)");
check(staticSrc.includes('act === "predict-refresh"'), "Backends view handles the predicted-throughput Refresh action");
check(groupsSrc.includes("data-group-predict-refresh"), "Groups card renders the predicted-throughput Refresh control");
check(groupsSrc.includes("Not measured"), "Groups card renders the predicted-throughput empty state (Not measured)");
check(groupsSrc.includes("doPredictRefresh"), "Groups view handles the predicted-throughput Refresh action");
check(staticSrc.includes("optimizeMeasureBackend(id)"), "Backends Refresh dispatches the Optimize Measure job");
check(groupsSrc.includes("optimizeMeasureBackend(id)"), "Groups Refresh dispatches the Optimize Measure job");
check(!staticSrc.includes("optimizeEstimateBackend"), "Backends view never dispatches the Estimate profile (predictions feed from Measure only)");
check(!groupsSrc.includes("optimizeEstimateBackend"), "Groups view never dispatches the Estimate profile (predictions feed from Measure only)");
check(staticSrc.includes("start it to refresh the prediction"), "Backends Refresh states the not-running reason in place");
check(groupsSrc.includes("start it to refresh the prediction"), "Groups Refresh states the not-running reason in place");
check(staticSrc.includes("data-pred-elapsed"), "Backends card renders the refresh Measuring elapsed state");
check(groupsSrc.includes("data-group-pred-elapsed"), "Groups card renders the refresh Measuring elapsed state");
const optimizeRsSrc = readFileSync(join(root, "..", "crates", "manager-app", "src", "optimize.rs"), "utf8");
check(optimizeRsSrc.includes("mutual exclusion refusal"), "benchmark/consultation starts log the mutual-exclusion refusal (tracker #70)");
check(optimizeRsSrc.includes("fn try_claim_optimize_op"), "the per-backend running-operation claim is a named predicate in the optimize paths (tracker #70)");
check(optimizeRsSrc.includes("consultations and benchmarks on the same backend are mutually exclusive"), "the mutual-exclusion refusal names the rule and the running operation (tracker #70)");

// Preflight completion (Oct 20, 2026): the backend card carries
// the requirement's recurrent-state term as a State cache tile
// beside the KV tile (hybrid SSM models), and the failed state
// surfaces the supervisor's recorded failure_reason under the
// header in "Label: Value" form. The row type declares both
// fields so the payload contract is explicit.
const apiSrc = readFileSync(join(root, "src", "api.ts"), "utf8");
check(apiSrc.includes("rs_ram_mib?: number | null"), "BackendRow declares the recurrent-state cache field");
check(apiSrc.includes("failure_reason?: string | null"), "BackendRow declares the recorded failure reason");
check(staticSrc.includes('["State cache"'), "Backends card renders the State cache tile");
check(staticSrc.includes("b.rs_ram_mib != null"), "State cache tile renders only when the requirement carries the term");
check(staticSrc.includes('b.state === "failed" && b.failure_reason'), "Failure line is gated on the failed state and a recorded reason");
check(staticSrc.includes("Failure: ${esc(b.failure_reason)}"), "Backends card renders the recorded failure reason");
check(bundle.includes("State cache"), "bundle contains the State cache tile label");

// KV cache / flash attention (Oct 21, 2026): the cache controls
// must actually render in both forms and commit through their
// ops — the helper alone surviving the build is not the feature.
check(staticSrc.includes("${cacheHtml}"), "Edit Flags form renders the cache controls block");
check(staticSrc.includes('fieldAttr: "data-cache-field"'), "Edit Flags cache controls use the cache field attribute");
check(staticSrc.includes("backendCachePreview"), "flags form prices the cache preview through backend_cache_preview");
check(staticSrc.includes("setBackendCache"), "flags form commits cache selections through backend_set_cache");
check(staticSrc.includes("data-cache-best"), "flags form renders the predicted-best action");
check(staticSrc.includes("data-add-cache-field"), "Add backend form renders the cache controls");
check(staticSrc.includes("cacheK: pCache?.k"), "Add backend form carries the cache selection into backend_add");
check(bundle.includes("K cache type"), "bundle contains the K cache type control label");
check(bundle.includes("Use predicted best"), "bundle contains the predicted-best action label");

// Settings-pages sweep (Oct 19, 2026): every stored value rendered
// on a settings surface carries a working control wired to its real
// save path. The compiled/source fragments of those controls must
// survive the build — a value with no editor is the defect class
// this sweep removed.
const settingsSrc = readFileSync(join(root, "src", "views", "static.ts"), "utf8");
check(settingsSrc.includes('id="setTelInterval"'), "Settings renders the telemetry poll interval editor (setTelInterval)");
check(settingsSrc.includes('id="setWinmlToggle"'), "Settings renders the Windows ML switch (setWinmlToggle)");
check(settingsSrc.includes('id="setWinmlPort"'), "Settings renders the Windows ML port editor (setWinmlPort)");
check(settingsSrc.includes('id="setPgPort"'), "Settings renders the PostgreSQL port editor (setPgPort)");
check(settingsSrc.includes('id="raPort"'), "Settings renders the Admin API port editor (raPort)");
check(settingsSrc.includes('id="setMxcPath"'), "Settings renders the MXC policy file editor (setMxcPath)");
check(settingsSrc.includes("data-comp-auto"), "Settings renders per-component auto-update switches (data-comp-auto)");
check(settingsSrc.includes("data-comp-pin-input"), "Settings renders per-component pin editors (data-comp-pin-input)");
check(settingsSrc.includes('id="gwPortInput"'), "Gateway view renders the gateway port editor (gwPortInput)");
check(settingsSrc.includes('id="gwOrCredInput"'), "Gateway view renders the OpenRouter credential reference editor (gwOrCredInput)");
check(settingsSrc.includes('id="gwOrCapInput"'), "Gateway view renders the OpenRouter daily cap editor (gwOrCapInput)");
check(settingsSrc.includes('id="gwOrAllowInput"'), "Gateway view renders the OpenRouter allowlist editor (gwOrAllowInput)");
check(settingsSrc.includes("data-group-fallbacks-save"), "Gateway view renders per-group fallback editors (data-group-fallbacks-save)");
check(settingsSrc.includes("updatesSetAuto("), "Settings wires component auto-update through updatesSetAuto");
check(settingsSrc.includes("updatesPin("), "Settings wires component pins through updatesPin");
check(settingsSrc.includes("updateAppConfig("), "Settings wires generic config editors through updateAppConfig");

check(!bundle.includes("live data wiring pending"), "bundle contains no wiring-pending mock banner");
check(!bundle.includes("not wired to a backend command yet"), "bundle contains no unwired-control message");
check(bundle.includes("__TAURI__"), "bundle contains the Tauri detection hook");

// Estimate/Measure job state (tracker #69): the Optimize page
// renders a running job from the server's descriptor (kind,
// backend, start) and surfaces the recorded job_error, with
// honest worst-case copy for a slow CPU-offloaded Estimate —
// the shared `measuring` boolean alone rendered Measure copy
// during Estimate runs and never surfaced failures.
check(bundle.includes("job_error"), "bundle reads the Optimize job_error payload field (tracker #69)");
check(bundle.includes("started_unix_ms"), "bundle reads the Optimize job start time (tracker #69)");
check(bundle.includes("fully CPU-offloaded backend this can take several minutes"), "bundle carries the honest Estimate worst-case copy (tracker #69)");

// Backend form memory summary (Oct 22, 2026): the cache preview's
// predicted footprint renders as structured Label: Value rows —
// Weights, KV cache, State cache (present only when the figure
// exists), an emphasized Total, then one capacity row per placed
// GPU with its required share and the verdict as a status chip.
// The old single-string line ("Weights: … · KV: … · Total: … vs
// GPU 0 … GiB · fits") must not return in either form (the shared
// cachePreviewHtml renderer serves the Edit and Add forms).
const staticSrcFit = readFileSync(join(root, "src", "views", "static.ts"), "utf8");
check(staticSrcFit.includes("function cachePreviewHtml"), "shared cachePreviewHtml renderer exists in static.ts");
check(staticSrcFit.includes('row("KV cache"'), "cache fit block renders the KV cache as its own row");
check(staticSrcFit.includes('row("State cache"'), "cache fit block renders the State cache as its own row");
check(staticSrcFit.includes("preview.rs_mib !== null"), "cache fit block renders the State cache row only when the figure exists");
check(staticSrcFit.includes("preview.kv_mib === null"), "cache fit block keeps the unknown-KV branch (no priced total, no fit claim)");
check(staticSrcFit.includes('t.fits ? "status-on" : "status-bad"'), "cache fit block renders the per-GPU verdict as a status chip");
check(staticSrcFit.includes("GiB of"), "cache fit block renders each GPU's required share of its total");
check(bundle.includes(" GiB of "), "bundle carries the per-GPU capacity phrasing (share of total)");
check(!staticSrcFit.includes("does not fit at this context"), "cache fit block no longer appends the verdict to the one-line string");
check(!staticSrcFit.includes("vs ${targets}"), "cache fit block no longer joins GPU totals into a 'vs' string");
check(!bundle.includes("does not fit at this context"), "bundle no longer carries the one-line fit verdict");
check(staticSrcFit.includes('p.fits ? "status-on" : "status-bad"'), "placement projection renders its Fit verdict as a status chip");

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

// Numeric display discipline (design-principles rule 14): every
// displayed number renders through the charts.ts utility family
// (trim1 / fmtGb / fmtCount / fmtPct). Raw float interpolation leaked
// full-precision values into the UI — the GPUs overview row printed
// power as "11.350000381469727 W" because it interpolated g.power_w
// directly instead of routing it through trim1. Two layers of pin:
// (1) behavior — the extracted formatters turn the reported power
// value into "11.4"; (2) sites — the sources carry no raw
// interpolation of the telemetry floats and no locale-dependent
// no-argument toLocaleString() on counts/contexts.
const chartsSrcNum = readFileSync(join(root, "src", "charts.ts"), "utf8");
function extractFn(src, name) {
  const start = src.indexOf(`export function ${name}(`);
  if (start < 0) return null;
  const open = src.indexOf("{", start);
  let depth = 0;
  for (let i = open; i < src.length; i++) {
    if (src[i] === "{") depth++;
    else if (src[i] === "}") { depth--; if (depth === 0) return src.slice(start, i + 1); }
  }
  return null;
}
const fmtNames = ["trim1", "fmtGb", "fmtCount", "fmtPct"];
const fmtBodies = fmtNames.map((n) => extractFn(chartsSrcNum, n));
check(fmtBodies.every(Boolean), "charts.ts exports the numeric formatter family (trim1/fmtGb/fmtCount/fmtPct)");
let fmt = null;
try {
  const plain = fmtBodies.join("\n").replace(/export /g, "").replace(/: (number|string)/g, "");
  fmt = new Function(`${plain}; return { trim1, fmtGb, fmtCount, fmtPct };`)();
} catch { /* null fails the behavior checks below */ }
check(fmt !== null, "numeric formatters evaluate for behavior checks");
if (fmt) {
  check(fmt.trim1(11.350000381469727) === "11.4", `trim1 renders the reported power value at one decimal ("${fmt.trim1(11.350000381469727)}")`);
  check(fmt.trim1(140) === "140", "trim1 drops the trailing .0 at magnitude >= 100");
  check(fmt.trim1(95.25) === "95.3", "trim1 keeps one decimal below 100");
  check(fmt.trim1(Number.NaN) === "\u2014", "trim1 renders a non-finite value as the unknown marker");
  check(fmt.fmtGb(16.376) === "16.4", "fmtGb renders through trim1");
  check(fmt.fmtCount(1234567) === "1,234,567", "fmtCount renders counts with en-US separators");
  check(fmt.fmtCount(512.6) === "513", "fmtCount rounds to an integer");
  check(fmt.fmtPct(0.125) === "12.5", "fmtPct renders a fraction as a one-decimal percent figure");
}
const staticSrcNum = readFileSync(join(root, "src", "views", "static.ts"), "utf8");
const dashSrcNum = readFileSync(join(root, "src", "views", "dashboard.ts"), "utf8");
const mapSrcNum = readFileSync(join(root, "src", "views", "serviceMap.ts"), "utf8");
const optSrcNum = readFileSync(join(root, "src", "views", "optimize.ts"), "utf8");
check(staticSrcNum.includes("trim1(g.power_w)"), "GPUs overview row renders power through trim1");
check(!staticSrcNum.includes("${g.power_w} W"), "GPUs overview row does not interpolate raw power");
check(!staticSrcNum.includes("${g.util_pct}%") && !staticSrcNum.includes("${g.temp_c}"), "GPUs views do not interpolate raw util/temp");
check(!staticSrcNum.includes("${g.mem_util_pct}%") && !staticSrcNum.includes("${g.fan_pct}%"), "GPU card does not interpolate raw memory-util/fan percentages");
check(!staticSrcNum.includes("String(g.util_pct)"), "GPU gauge center label renders through trim1, not String()");
check(!staticSrcNum.includes(".toFixed(2)"), "no two-decimal fixed rendering remains in static.ts (predicted-throughput TTFT routes through trim1)");
check(staticSrcNum.includes("trim1(n / 1024)"), "ctxShort renders the K quotient through trim1");
check(!dashSrcNum.includes("${g.util_pct}%") && !dashSrcNum.includes("${g.temp_c}"), "dashboard console/topology do not interpolate raw util/temp");
check(dashSrcNum.includes("trim1(b.health)"), "dashboard console renders backend health through trim1");
check(dashSrcNum.includes("fmtCount(b.requests_1h)"), "dashboard console renders request counts through fmtCount");
check(!mapSrcNum.includes("String(g.util_pct)") && mapSrcNum.includes("trim1(g.util_pct)"), "service map GPU load gauge renders util through trim1");
check(optSrcNum.includes("fmtCount(e.inputs.n_ctx)") && optSrcNum.includes("fmtCount(e.flags.n_ctx)"), "Optimize renders context sizes through fmtCount");
for (const [name, src] of [["static.ts", staticSrcNum], ["dashboard.ts", dashSrcNum], ["serviceMap.ts", mapSrcNum], ["optimize.ts", optSrcNum]]) {
  check(!/\.toLocaleString\(\)/.test(src.replace(/new Date\([^)]*\)\.toLocaleString\(\)/g, "")),
    `${name} has no locale-dependent no-argument toLocaleString() on a numeric value (counts/contexts route through fmtCount)`);
}

// Rule 5 (no explanatory narrative in primary UI): the setup wizard's
// launch banner sentence and the other always-on behavior explanations
// removed 2026-10-22 must not return. Fully deleted texts are pinned
// against the built bundle; texts relocated into tooltips are pinned
// against the visible markup in the sources.
check(!bundle.includes("steps execute against this machine"), "bundle does not contain the wizard launch banner sentence");
check(!bundle.includes("A failed step pauses the run"), "bundle does not contain the wizard failure-instruction sentence");
check(!bundle.includes("Steps run in order"), "bundle does not contain the wizard sidebar narrative");
check(!bundle.includes("checksums verified per file"), "bundle does not contain the wizard sidebar methodology lines");
check(!bundle.includes("the gateway starts at its step"), "bundle does not contain the wizard sidebar start-order line");
check(!bundle.includes("This view refreshes every 2 s"), "bundle does not contain the GPUs view refresh narrative");
check(!bundle.includes("runs in order"), "bundle does not contain the checklist order clause");
check(bundle.includes("Sample mode"), "bundle contains the Sample mode status value");
check(!allSrc.includes("Sample mode \\u2014 open the installed app") && !allSrc.includes("Sample mode — open the installed app"), "no visible sample-mode banner sentence remains in ui/src (status value + tooltip only)");
const optSrcNar = readFileSync(join(root, "src", "views", "optimize.ts"), "utf8");
check(!optSrcNar.includes('<span class="opt-note">Accepted experiments run'), "Optimize experiments explanation is not a visible note (button tooltips only)");
check(!optSrcNar.includes('<p class="opt-note">Every stored measurement'), "Optimize measurements description is not a visible paragraph (info-tip only)");
const staticSrcNar = readFileSync(join(root, "src", "views", "static.ts"), "utf8");
check(staticSrcNar.includes('"banner mode-live"'), "wizard live mode renders as a neutral status banner, not warning styling");
check(!staticSrcNar.includes("wiz-side"), "wizard sidebar narrative block is removed from static.ts");

if (failures > 0) { console.error(`\n${failures} smoke check(s) failed`); process.exit(1); }
console.log("\nsmoke: all checks passed");
