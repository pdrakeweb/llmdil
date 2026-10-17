# Design B: The Appliance Console
Local LLM Service Manager — UI design brainstorm, direction B.
Author: Designer B (subagent). Date: 2026-10-08. Design only, no code.

Direction name: **The Appliance Console** — LM Studio-inspired, appliance-style, guided.

Philosophy (3 lines):
1. The app feels like a well-built appliance: first run lands on setup, daily use is health-at-a-glance, and every automation explains what it did and yields to manual override.
2. Guided by default, expert one click away: plain-language stages and curated defaults with visible, editable overrides — progressive disclosure, never dumbed down for Pete's level.
3. One object, one place: each backend, GPU, model, and policy has a single canonical surface; state is never stale, silent, or collapsed behind one toggle.

Related documents:
- Research brief: `research-brief.md` (10 tools surveyed; patterns and anti-patterns referenced as [P1]–[P8], [A1]–[A6]).
- MXC mechanics: `../hidden_files/mxc-research-brief-2026-10-08.md`.
- Windows ML llama.cpp findings: `../hidden_files/winml-llamacpp-research-2026-10-08.md`.

---

## 1. Mockups (structured text, 8 windows)

Conventions used across all mockups: window title is declarative. Status dots: green = healthy, amber = degraded/action needed, red = failed/stopped-unexpectedly, grey = intentionally stopped/disabled. No decorative icons carry meaning alone; every dot has a text label. All numbers show units. "Last updated Xs ago" appears on every live view; stale data (>10s without a successful poll) is labeled STALE, never silently shown.

---

### M1. First-Run Setup Wizard — "Set up Local LLM Service"

Purpose: ordered, staged installation of the full stack on first launch. First run lands here, not on an empty dashboard [P8].

Layout (modal-width window, 1100x760, non-resizable during run):
- Top region: title bar "Set up Local LLM Service" + step progress rail (7 stages, left to right): 1 Prerequisites, 2 GPU drivers, 3 llama.cpp, 4 Models, 5 Gateway, 6 Editor & MCP, 7 Sandbox. Current stage highlighted; completed stages show check; future stages grey. Clicking a completed stage revisits it (read-only summary, "Re-run" button per stage).
- Left region (30%): stage explanation panel. Plain-language heading (e.g. "Download the models"), 2–4 sentence description of what this stage does and why the order matters, and a "What will change on your machine" list (paths, ports, services). No marketing prose.
- Center region (55%): stage work area — a UniGetUI-style component table [P1]: columns Component | Required | Detected | Status | Action. Rows encode the dependency chain; a row cannot be actioned until its prerequisites are green (blocked rows show "waiting on: GPU drivers").
- Right/bottom region: action bar. Primary button per stage ("Install all missing", "Download selected"), secondary "Skip for now" (records the skip as an explicit state, re-surfaces in Updates & Diagnostics), tertiary "Show commands" (see Interactions).
- Bottom strip: live operation log, one line per action (queued → running → done/failed), each with timestamp. Expandable to the full log drawer (M6).

Stage contents:
1. Prerequisites — checks (not installs): Windows 11 24H2 build ≥ 26100.9278 (MXC minimum), NVIDIA driver version vs minimum for the llama.cpp CUDA build, CUDA toolkit presence (or CUDA-enabled build selected), VS Code installed, disk space for 4 GGUFs, WebView2 runtime. Each check row: pass/fail with the exact detected value and, on fail, a remediation link or inline fix action ("Open NVIDIA driver download", "Install CUDA 12.x via winget").
2. GPU drivers — driver version row with Update action; shows current vs latest-known (from local cache; "checked 2h ago" label — never claims live without a check).
3. llama.cpp — CUDA build selection: curated default build (version pinned, e.g. b7xxx CUDA 12.x) with "why this build" note (tensor-split, CUDA graphs support). Action: Download + verify checksum. Advanced disclosure: build variant picker (CUDA / Vulkan fallback / CPU-only), install path override.
4. Models — the LM Studio-style catalog [P4]: the 4 curated GGUFs as rows (Qwen3.8-27B planner, Qwen3-Coder-30B, Qwen2.5-Coder-7B coder pool, Qwen3-8B tool-runner), each with size on disk, quant, parameter count, and a fit-check column computed against live free VRAM across the 3 GPUs ("fits: 27B Q4 needs ~17GB → A4000+P100 split, OK"). Multi-select with total download size; per-model download progress bars; pause/resume/cancel. Skipped models remain listed as "not downloaded" post-setup.
5. Gateway — LiteLLM install (uv/pip into the app's managed Python env; path shown), config file generation preview (the exact YAML it will write, editable before apply), port selection (default 4000, conflict check), "Start gateway after setup" checkbox.
6. Editor & MCP — VS Code extension rows (Continue.dev, Cline): Install/Update via `code --install-extension` with version pins; MCP server rows (GitHub, Google Drive): install + credential status ("GitHub token: present in Windows Credential Manager / missing → Store" — the app never shows the secret value, only presence).
7. Sandbox — MXC runtime check (PSEC 1.1 + ingress-support probe result shown verbatim), default policy preview (the JSON for the tool-execution sandbox, plain-language summary beside each section), mode selector: Learning (recommended first) / Permissive / Enforcement. "Run policy self-test" button executes a harmless sandboxed command and shows the activity report inline.

Finish screen: summary table of everything installed/changed (component, version, location), "Open dashboard" primary, "Open setup log" secondary, "Re-run any stage" list.

Primary interactions:
- "Show commands": every automated step expands to the exact command(s) it will run (winget line, curl URL, checksum, file paths) with Copy buttons. Pete can run them himself or let the app run them — automation is transparent, never magic.
- Manual override: every row has an overflow menu "Mark as manually installed" (with path/version fields) and "Use custom…" (custom build URL, custom model file path). Overrides are first-class state, labeled "manual" in later views.
- Failure: a failed row shows the failing log lines inline (last 20), "Retry", "Open full log", and "Skip with note". Nothing fails silently [A6].

---

### M2. Main Dashboard — "Dashboard"

Purpose: health-at-a-glance for daily use. Answers in under 5 seconds: is everything up, where are the models, is anything saturated.

Layout (main window, sidebar + content):
- Left region: sidebar nav (icon + label): Dashboard, Models, Backends, Logs, Gateway, Settings. Bottom of sidebar: app status strip — gateway dot + "LiteLLM :4000", "4/4 backends up", tray/minimize controls.
- Top region: header bar. Left: window title "Dashboard". Center: global status line — "All systems normal" or the single most severe active issue ("backend coder-30b restarted 2m ago — see Logs"). Right: "Last updated 3s ago", pause-live-updates toggle, Settings gear.
- Content region, top-to-bottom:
  - Row 1 — Service cards (5 cards: LiteLLM gateway + 4 llama-server backends) [P5]. Each card: status dot + name, port, loaded model (or "no model loaded"), uptime, VRAM held (GB), req/s + tokens/s (1m avg), and inline icon-buttons: restart, stop/start, "logs" (jumps to M6 filtered). Cards are NOT a single toggle [A2] — each backend is independently controllable and its state independently visible.
  - Row 2 — GPU strip: 3 compact GPU cards (A4000, P100-0, P100-1) [P3]: name, util %, VRAM bar (used/total GB), temp °C, power W. Clicking a card opens M3. The strip shows the busiest engine metric per GPU (Task Manager rule) so an idle-looking card can't hide load.
  - Row 3 — two columns: left "Model placement" mini-map (which model lives on which GPU(s), tensor-split ratios as small stacked bars — e.g. Qwen3.8-27B: A4000 60% / P100-0 40%); right "Recent activity" (last 8 events: backend restarts, downloads completed, config changes, policy denials — each linking to the relevant surface).
- Bottom region: collapsible task drawer (shared with M6): currently running operations with progress and abort.

Primary interactions:
- Click any service card (not the action buttons) → Backend detail (M4).
- Hover a GPU card → tooltip with per-engine util, clocks, fan, driver version.
- The header status line is clickable → opens the issues list (active alerts with severity, source, and "acknowledge"/"open logs" actions).

---

### M3. GPUs & Placement — "GPUs"

Purpose: answer "where is each model, how much VRAM does it hold, is any GPU saturated" [P3].

Layout:
- Top region: header "GPUs" + controls: poll interval selector (1s/5s/off), "Reset peak marks" button, snapshot button ("Save snapshot" writes timestamped JSON to the diagnostics folder).
- Content region: 3 full GPU cards, left to right (A4000, P100-0, P100-1). Each card, top-to-bottom:
  - Header: GPU name, PCI bus/device/function, driver version, status dot.
  - Metric grid (2 cols): Utilization % (with 60s sparkline), VRAM used/total + bar, Memory util %, Temperature °C (with rising/falling arrow + peak mark), Power draw W / limit W, Clocks (graphics/memory MHz), Fan %.
  - "Processes" sub-table: process name / PID → VRAM MiB → backend association ("llama-server · coder-30b" or "unassociated"). Unassociated consumers are shown, not hidden — a stray process eating VRAM is visible here.
  - "Models on this GPU" list: model name, layers on this GPU (n/total), tensor-split share %, KV cache MB.
- Below the cards: placement map — a horizontal stacked-bar per model showing its split across the 3 GPUs + system RAM offload (if any), with the configured `--tensor-split` values beside the observed values. Mismatch (configured vs observed) is flagged amber: "configured 60/40, observed 55/45 — backend restarted with edited flags?"

Primary interactions:
- "Rebalance" (expert disclosure): opens the placement editor — per-model tensor-split sliders/number fields with live "projected VRAM per GPU" readout and a fit validation ("A4000 would hold 15.9/16 GB — tight"). Apply triggers a guided backend restart sequence (drains, restarts one backend at a time, verifies health after each). The current running values remain visible alongside the edited values until applied — config is never edited blind [P7].
- Per-process "details" link → jumps to Logs filtered to that PID's backend.

---

### M4. Backend Detail — "Backends / coder-30b"

Purpose: one canonical surface per llama-server process: status, config, logs, and a test harness — the Proxmox context-tabs idea without the tree [P2 adapted].

Layout:
- Top region: header with backend name, status dot + state text ("Running · healthy · uptime 3d 4h"), port, PID, and action buttons: Restart, Stop, Start (contextual), "Open in browser" (llama-server web UI), overflow (Edit config, Duplicate backend, Delete).
- Tab bar: Summary | Configuration | Logs | Test request.
  - Summary tab: resource bars (VRAM held on each GPU, RAM, context usage: n_ctx used/allocated), request stats (total requests, tokens in/out, avg TTFT, tokens/s, error rate 1h/24h), health-check history (last 20 probes: timestamp, latency, result), and the exact launch command (read-only, copyable) — the "show what it did" rule applied to running state.
  - Configuration tab: structured form of the backend's server flags [P7] — model file path (with "change" opening the model picker), n_ctx, n_batch, tensor-split, split-mode, parallel slots, KV quant, flash attention, port, plus "raw flags" read-only preview of the assembled command line. Fields carry validation (n_ctx ≤ model's trained max; tensor-split sums to 1.0) and "requires restart" badges. Save → "Apply & restart" / "Save for next restart" choice. Global defaults shown greyed behind per-backend overrides [P6].
  - Logs tab: the backend's log stream (embedded M6 viewer, pre-filtered), with level filter and "jump to errors".
  - Test request tab: minimal request composer (the anti-chat affordance [A1]): model fixed to this backend, prompt textarea, parameter overrides (temperature, max tokens), Send → raw JSON response + timing breakdown (TTFT, tokens/s). Explicitly a diagnostic tool, not a chat surface — no history, no personas.
- Bottom region: the shared task drawer.

Primary interactions:
- Restart offers "graceful (drain 30s)" vs "immediate".
- Any config change shows a diff summary before apply ("3 flags change: --n-ctx 8192→32768 …").
- "Copy launch command" for manual reproduction outside the app.

---

### M5. Model Library — "Models"

Purpose: download, verify, store, and assign the GGUFs — LM Studio's catalog + fit-check [P4], plus placement assignment.

Layout:
- Top region: header "Models" + storage summary ("Models: 4 installed · 48.2 GB in D:\llm\models") + "Add model" (URL or local file) button.
- Tab bar: Catalog | Installed | Downloads.
  - Catalog tab: the 4 curated Qwen rows + "compatible extras" section (clearly labeled as not part of Pete's plan). Columns: Model | Params | Quant | Size | Fits (live fit-check vs free VRAM: green "fits A4000+P100 split", amber "fits with offload", red "does not fit") | action (Download / queued state). Row click → detail pane: description, why it's in the plan (planner / coder pool / tool-runner), recommended placement, source URL + checksum.
  - Installed tab: rows per GGUF: name, size, quant, checksum verified date, assigned backend(s) + placement summary, actions: Load into backend (picker), Verify checksum, Reveal in Explorer, Delete (with "backend X currently uses this" guard).
  - Downloads tab: queue with per-download progress bar, speed, ETA, pause/resume/cancel, completed history with checksums. Failed downloads show the error and resume from byte offset where the server allows.
- Right region (detail pane, 30%): selected model detail — metadata, fit-check breakdown per GPU ("needs ~16.8 GB: A4000 free 9.1 + P100-0 free 8.4 → fits with split 55/45"), recommended server flags for this model, and the override controls (custom split, context size) that feed M4's configuration.

Primary interactions:
- Fit-check is recomputed live as VRAM changes; a model that fit yesterday but doesn't today says why ("P100-1 now holds coder-7b, 6.2 GB less free").
- "Download all missing" respects the curated order and skips already-installed.

---

### M6. Logs & Activity — "Logs"

Purpose: logs as a first-class surface [P2]; the always-available task stream without modal dialogs.

Layout:
- Top region: header "Logs" + scope selector (All / Gateway / each backend / Installer / App) + level filter (info/warn/error) + text search + "Follow" toggle + "Export" (writes filtered view to file).
- Content region: virtualized log table — timestamp, scope tag, level, message. Error rows expandable to show surrounding context lines. Color is redundant with the level label (no color-only meaning).
- Bottom region (persistent across the whole app, collapsible): task drawer — every long operation (installs, downloads, backend restarts, config applies) as a row: operation, target, progress, elapsed, status, Abort button for running tasks, "view log" jumping the main pane to that operation's lines. Completed tasks collapse to one line each; failures stay expanded until acknowledged.

Primary interactions:
- Clicking a log line's scope tag filters to that scope.
- "Copy as diagnostic bundle" zips recent logs + config snapshot + GPU snapshot for troubleshooting.
- The drawer auto-expands when a new task starts and auto-collapses (to one line) when it completes — never steals focus, never hides [A6].

---

### M7. Gateway — "Gateway"

Purpose: LiteLLM as the unified front door — model groups, routing policy, and request visibility.

Layout:
- Top region: header "Gateway" + status (LiteLLM version, config file path, port 4000, uptime) + actions: Restart, Edit config, "Open LiteLLM UI" (if enabled).
- Content region, top-to-bottom:
  - "Model groups" table: group name (e.g. `planner`, `coder-fast`, `coder`, `tool-runner`) → member backends → routing strategy (simple-shuffle / latency-based / least-busy) → fallback chain. This is where Pete's routing policy lives: coder-fast = defined code tasks only, tool-needing work → planner/coder.
  - "Routing rules" list: plain-language rules with their LiteLLM config equivalents ("Requests tagged task=coding without tools → coder-fast"). Each rule shows its source: curated default vs Pete override.
  - "Recent requests" table (last 100, ring buffer): timestamp, group, backend chosen, tokens in/out, latency, status. Failed requests expandable to the error returned.
  - "Health" strip: per-backend latency sparkline as seen by the gateway (catches "backend up but slow" that process status misses).
- Bottom: task drawer.

Primary interactions:
- Edit model group → structured editor (member backends checkboxes, strategy dropdown, fallback ordering drag) with config-diff preview before apply; apply hot-reloads LiteLLM where supported, else offers restart.
- "Test routing": send a tagged test request, see which backend the gateway picked and why (strategy trace line).

---

### M8. Settings — "Settings"

Purpose: the admin surface, page-based and separate from monitoring [P6]; global defaults with per-object overrides live here and on the objects they affect.

Layout: left sub-nav (within the Settings page): General | Backends | Models & placement | Gateway | Sandbox (MXC) | Editor & MCP | Windows ML | Updates | Diagnostics.
- General: app behavior — launch on startup, start minimized to tray, poll intervals, log retention, data directory paths, theme (light/dark/system), notifications toggles.
- Backends: global default server flags form [P7] (the template every backend inherits; per-backend overrides shown as "3 overrides" links jumping to M4). Auto-restart policy (on crash: restart up to N times with backoff; on config change: manual/guided/auto).
- Models & placement: default placement strategy per model, KV-cache defaults, download directory, checksum policy.
- Sandbox (MXC): policy editor — the JSON policy as a structured form (filesystem paths read/write/denied, network egress rules, loopback toggles, UI), mode selector (Learning/Permissive/Enforcement) with the current mode bannered on the Dashboard, "Run in Learning mode for 7 days then review" guided flow that opens the activity report and proposes least-privilege tightening with accept/reject per rule. PSEC/ingress probe results shown verbatim. Credential status: which secrets the launcher will inject from Windows Credential Manager (presence only, never values).
- Editor & MCP: VS Code extension inventory (installed version, pinned version, update), MCP server rows (command, env, enabled; credential presence), "test MCP connection" per server.
- Windows ML: secondary-target panel — WinMLServer registration status, which model (if any) is registered, its /v1 endpoint, LiteLLM backend entry preview; banner stating the researched limits (no tensor-split exposed, coarse GPU selection — experimental) with "re-check capabilities" button that re-probes and updates the banner. Register/unregister actions; never presented as primary.
- Updates: UniGetUI-style inventory [P1] — every managed component (llama.cpp build, LiteLLM, each model file, each extension, MXC runtime, GPU driver): installed version, latest known, status, per-row Update, "Update all", per-component "pin version" and auto-update toggles. Last-checked timestamps; "Check now".
- Diagnostics: "Run full diagnostic" (prereqs re-check + backend health + port conflicts + disk space + config validation) producing a pass/fail report with remediation actions; "Open data folder"; "Export diagnostic bundle"; app log level.

Primary interactions: every settings page has the same footer pattern — "unsaved changes" indicator, Discard / Apply, and for changes needing restarts, an explicit "Apply & restart affected services" with the affected list named. No modal wizards for routine config [A5].

---

## 2. Design doc

### Goals
1. Get Pete from zero to a working 3-GPU inference stack without CLI archaeology: ordered setup with prerequisite checks, verified downloads, and transparent automation.
2. Make the running system legible at a glance: per-backend and per-GPU state, model placement, and request flow — no single toggle hiding multi-backend reality.
3. Keep full expert control: every default is visible, every automated step shows its commands, every config change shows a diff; manual overrides are first-class.
4. Contain agent tool execution under MXC policy with a guided path from Learning mode to enforced least-privilege.
5. Never show stale or silent state: every live number carries its age; every action produces visible feedback with log lines attached.

### Layout system
- Window: fixed sidebar (200px, icon + label) + header bar + content + collapsible bottom task drawer. One window; no floating palettes.
- Content grid: 12-column, 16px gutters; cards have 1px borders, 8px radius, no shadows except the drawer. Density: Task-Manager-like tables, 13px base font, tabular numerals for all metrics.
- The bottom task drawer is the single global "work happening" surface: 40px collapsed (current operation + progress), expandable to 240px, full-page under Logs. It never appears as a modal.
- Detail views (backend, model) use the same tab pattern: Summary | Configuration | Logs | (context action). Tabs, not trees — the inventory is small and fixed (5 services, 3 GPUs, 4+ models), so a tree adds depth without gain.

### Navigation model
Flat sidebar, page-per-concern: Dashboard · Models · Backends · Logs · Gateway · Settings. Rationale: the object count is small and stable; Proxmox's tree earns its keep at hundreds of nodes, here it would bury the 5 services Pete touches daily. Cross-links are explicit ("view in Logs", "edit in Settings → Backends") rather than nested. First-run replaces the sidebar entirely with the wizard; post-setup, the wizard stages remain reachable as Updates/Diagnostics pages — the wizard is a path, not a place.

### Information hierarchy
1. Status first: is it up, is it healthy — dots + words, always paired.
2. Resources second: VRAM/utilization/placement — the numbers Pete tunes against.
3. Configuration third: flags, policies, versions — one click from the status that they explain.
4. History fourth: logs, events, request tables — for diagnosis, not for scanning.
Global rule: any number on screen answers "as of when" (relative timestamp); any action answers "what did it do" (log line + result state).

### Visual language (declarative)
- Light and dark themes; dark default (evening tuning sessions). Accent: single neutral blue for interactive elements; status colors reserved for status (green/amber/red/grey) and never used decoratively.
- No metaphors, no illustrations, no mascots, no taglines. Window titles are nouns. Buttons are verbs. Descriptions describe.
- Monospace for commands, paths, flags, JSON, and log output. Proportional for prose. Tabular numerals everywhere metrics appear.
- Sparklines and stacked bars are the only charts; no pie charts, no 3D, no gauges. Bars show used/total with the numbers labeled on the bar.
- Empty states state the cause and the next action ("No backends configured — complete Setup stage 3" with a button), never a bare "No data".

### Progressive disclosure rules
- Default view shows curated state; "Advanced" disclosure on cards and settings pages reveals flags, raw commands, and JSON.
- "Show commands" is available on every automated action, wizard or otherwise.
- Expert controls (placement editor, raw flag editing, policy JSON) are one click away, never behind a wizard, and always show current live values beside edited values.

### Deliberate divergences from the research brief (and why)
1. **Flat sidebar instead of Proxmox tree+tabs [P2].** The brief's tree fits hundreds of objects; this app manages ~12. A tree would add a navigation layer between Pete and the 5 services he restarts while tuning. The context-tabs half of the pattern is kept (M4's Summary/Configuration/Logs/Test tabs).
2. **Task drawer is collapsible and auto-managing, not always fully visible [P2].** A permanently tall drawer steals vertical space from the health cards on a single monitor. It auto-expands on new work, collapses on completion, persists as the Logs page — omnipresent as a 40px strip, full detail on demand.
3. **First-run wizard stages remain reachable post-setup as ordinary pages [P1/P8].** The brief positions the wizard as first-run-only; B keeps every stage's component table alive under Updates/Diagnostics so "re-run stage 3" is never a re-install of the app.
4. **Gateway gets its own page (M7), not folded into Backends.** The brief's service-rows pattern covers backends; the routing policy (model groups, fallback chains, task tagging) is a distinct object with distinct questions ("which backend did the gateway pick and why"), so it gets a distinct surface.

### Anti-pattern compliance
- [A1] No chat surface. The only prompt box in the app is M4's Test request tab: a diagnostic tool with raw JSON output, no history.
- [A2] No single server toggle. Five service cards, each with independent status, logs, restart.
- [A3] No admin maze. Settings is page-based with a left sub-nav, and every setting links from the object it affects.
- [A4] No decorative metaphors. Status dots, bars, tables.
- [A5] No modal wizards for routine config. The only modal wizard is first-run; routine edits are inline with diffs.
- [A6] No silent/stale state. Age labels on all live data, STALE markers, per-action feedback with log lines, failures stay visible until acknowledged.

---

## 3. Implementation plan (Tauri-primary, Rust backend)

### Architecture (accurate Tauri shape)
The UI layer is HTML/CSS/JS running in the OS webview (WebView2 on Windows 11). All system integration lives in Rust behind it: Tauri commands (request/response) for actions and config, Tauri events (async stream) for telemetry, logs, and progress. The Rust backend owns: process supervision (spawn/kill/health-check llama-server processes, LiteLLM, WinMLServer), GPU telemetry polling, download management, config file read/write with validation, and MXC policy enforcement. The webview never shells out, never touches the filesystem directly, never holds secrets — it renders state and issues commands. Secrets (GitHub token, Google OAuth material) are resolved by the Rust backend from Windows Credential Manager at use time and never cross into the webview except as presence booleans.

### Stack candidates — honest tradeoffs

**Tauri 2 (Rust backend + webview UI) — PRIMARY candidate.**
- For: single small binary; no runtime dependency (WebView2 ships in-box on Windows 11); Rust backend is a direct fit for the hard parts — process supervision, NVML GPU telemetry via the `nvml-wrapper` crate, and the MXC Rust SDK (`mxc-sdk` on crates.io) links in directly with no subprocess/FFI shim; llama.cpp Rust bindings (`llama-cpp-2`) exist if a native probe/embedding path is ever wanted (the design keeps llama-server subprocesses as the serving path — bindings are an option, not the architecture); CI produces signed NSIS/MSI installers via `tauri-action` with auto-update built in. Pete's workloads are systems-heavy; Rust matches the domain.
- Against: WebView2 rendering quirks to test (high-DPI, GPU-accelerated canvas for sparklines); UI component ecosystem is the generic web ecosystem (good) but Tauri-specific plugins are thinner than Electron's; Rust compile times slow CI iteration; hiring/familiarity — Pete is an expert engineer but his daily drivers are TS/Python/Kotlin, so Rust is a learning curve for his own future contributions; `nvml-wrapper` and `mxc-sdk` are young crates — version pinning and fallback paths (parse `nvidia-smi` XML; shell to `wxc-exec.exe`) are required, not optional.

**Electron (Node backend + Chromium) — strongest alternative.**
- For: largest desktop-app ecosystem; the MXC Node SDK (`@microsoft/mxc-sdk`) is official; Chromium rendering is the most predictable target for dense dashboards; Pete already works in TS daily. Against: 150MB+ installers and high idle memory for an always-on tray app; Node process supervision of GPU workloads is workable but less natural than Rust; auto-update story is mature but heavier; shipping a second Chromium next to WebView2/Edge is wasteful on Windows.

**.NET 8 + WinUI 3 (or WPF) — most native Windows choice.**
- For: first-class Microsoft SDKs — `Microsoft.Mxc.Sdk` on NuGet, Windows ML via NuGet, NVML via P/Invoke or existing wrappers; genuinely native controls, best OS integration (tray, notifications, startup tasks); single-framework story from UI to sandboxing. Against: Windows App SDK runtime dependency and MSIX packaging friction (sideloading certs, store vs sideload decisions); WinUI 3 is still maturing (known windowing/tray gaps vs WPF); Pete's contribution path is C#/XAML, a third language; cross-platform is off the table (acceptable — this app is Windows-only by design, but it forecloses any future Linux port of the manager).

**Locally-served web dashboard (Rust or Python server + browser UI) — weakest for this job.**
- For: zero install friction, trivial remote access from another machine. Against: no real tray presence or startup-task integration; process supervision from a user-mode web server is possible but the "appliance" feel (installer, notifications, auto-start services) has to be rebuilt by hand; browser tab is a worse home for an always-on service manager; GPU telemetry and MXC integration still need the native layer, so this saves nothing — it just moves the UI into a tab. Rejected as primary; a read-only status page served by the Rust backend is a reasonable stretch goal (M7+).

**Pure-Rust GUI (egui/Iced) — rejected.** Single binary and no webview, but the widget set (dense tables, tab strips, virtualized logs, forms with validation) is immature for this information density; charting and text rendering would be hand-built. Wrong tradeoff for a data-dense console.

### Rust-specific integration points
- **MXC:** `mxc-sdk` crate — policy construction, backend probing (PSEC 1.1 + `probes.baseContainerSupportsIngressHostLoopbackAllow`), spawn in Learning/Permissive/Enforcement, activity-report retrieval. Fallback: shell out to `wxc-exec.exe --config-base64` if the crate lags the schema. The Sandbox settings page (M8) maps 1:1 to the policy JSON schema sections.
- **llama.cpp:** managed as `llama-server` subprocesses (per-backend process group, graceful drain then kill, stdout/stderr piped to the log bus). `llama-cpp-2` bindings reserved for a future in-process health-probe or embedding sidecar — not the serving path, which stays with the official server binaries for flag parity.
- **GPU telemetry:** `nvml-wrapper` for per-GPU util/VRAM/temp/power/clocks and per-process VRAM attribution; `nvidia-smi --query --xml` as the fallback parser when NVML init fails (Tesla P100s in TCC/driver modes have historically had NVML quirks — the fallback is load-bearing, not decorative).
- **LiteLLM:** managed Python environment (uv) owned by the app; the Rust backend generates `config.yaml` from the Gateway page state and hot-reloads or restarts the gateway process.
- **Installer/updates:** `tauri-bundler` NSIS target for the wizard-style install Pete expects on Windows; MSI retained as an option. `tauri-plugin-updater` for app updates; component updates (llama.cpp builds, models) are app-level, handled by the Updates page, not the OS installer.

### UI framework choice inside Tauri (decision recorded)
Svelte 5 + SvelteKit (static) or plain Vite+Svelte: smallest bundle, fine-grained reactivity suits 1s telemetry streams without virtual-DOM churn, and the component model fits card/table density. React is the fallback if Pete prefers ecosystem over bundle size — the Tauri boundary (commands/events) is framework-agnostic, so this choice is reversible late. Styling: hand-rolled CSS with design tokens (no Tailwind — the layout system above is custom and dense; utility CSS fights it). Charts: lightweight canvas sparklines, hand-rolled (no chart lib needed for sparklines and stacked bars).

### Milestones and build order (stack-adjusted)
1. **M0 — Scaffold (wk 1–2).** Tauri 2 + Svelte shell, sidebar navigation, theme tokens, Rust command/event plumbing skeleton, logging bus (Rust → webview event stream), CI building unsigned NSIS. Exit: window opens, nav works, a Rust command round-trips.
2. **M1 — Process supervisor + Dashboard (wk 3–5).** Backend model (struct per llama-server: flags, port, PID, state machine), spawn/health-check/restart with backoff, graceful drain; M2 dashboard service cards + M4 Summary/Logs tabs; tray icon with per-backend status. Exit: app can launch, monitor, and restart the 4 backends + gateway.
3. **M2 — Setup wizard (wk 6–8).** Prerequisite checks (Windows build, driver version via NVML, CUDA presence, disk space), component table with ordered stages, llama.cpp build download + checksum verify, GGUF downloads with progress/resume, LiteLLM env bootstrap + config generation, VS Code extension install via `code` CLI, MCP server registration, "Show commands" transparency, manual-override capture. Exit: clean-machine run reaches a working stack. (Wizard comes after the supervisor because the wizard's stages install things the supervisor then manages — building the managed object first keeps the wizard honest.)
4. **M3 — GPU telemetry + placement (wk 9–10).** NVML polling loop with `nvidia-smi` fallback, per-process VRAM attribution, peak marks, M3 GPU cards + placement map, tensor-split editor with projected-VRAM validation and guided rolling restart. Exit: "where is each model" answered live.
5. **M4 — Configuration system (wk 11–12).** Typed config schema (Rust structs → JSON schema → generated Svelte forms), global-defaults/per-backend-override resolution, diff preview, apply/restart orchestration, config validation (n_ctx bounds, tensor-split sums). M4 Configuration tab, M7 gateway editor, M8 settings pages. Exit: no hand-edited files needed for routine tuning.
6. **M5 — Models + MXC sandbox (wk 13–15).** M5 library with live fit-check against NVML free-VRAM, checksum verification; MXC integration — probe PSEC/ingress, policy form, Learning-mode runner with activity-report ingestion and tightening proposals, credential-presence wiring to Credential Manager. Exit: sandboxed tool execution works in Learning mode; policy tightening is guided.
7. **M6 — Updates, diagnostics, polish (wk 16–18).** M8 Updates inventory + per-component update, full diagnostic runner, diagnostic bundle export, notifications, first-run finish screen, signed installer CI, auto-update. Exit: someone other than Pete can install and run it.
8. **M7 — Stretch.** Read-only web status page from the Rust backend; Windows ML secondary-target auto-probing (re-check capabilities button wired to real probes); usage/request analytics beyond the 100-request ring buffer.

Build order rationale: supervise before installing (M1 before M2) so the wizard installs into a system the app already knows how to run; telemetry before config editing (M3 before M4) so placement edits are validated against live numbers; MXC after the serving path is solid (M5) because sandboxing is the outer layer, not the core.

---

## 4. Test plan

### Levels
- **Unit (Rust, `cargo test`).** Fit-check arithmetic (model bytes vs free VRAM per GPU, split math), tensor-split validation (sums to 1.0, clamps), config schema validation (n_ctx bounds, port conflicts, flag allowlist), LiteLLM YAML generation (snapshot tests of generated config), MXC policy JSON construction (schema-conformance tests against the versioned schema), download resume offset math, version comparison/ordering.
- **Unit (Svelte, vitest).** Formatters (GB/MiB, durations, relative timestamps), table sorting/filtering, sparkline data windowing, state-store reducers for backend status transitions.
- **Integration (Rust).** Supervisor lifecycle against stub `llama-server` binaries (fake server that logs, binds a port, responds to /health): start → healthy → kill → backoff restart → drain. Config apply → file written → process restarted with new flags (assert on the stub's argv). Download manager against a local HTTP server: progress events, pause/resume byte ranges, checksum failure path. NVML telemetry with a mock NVML layer (trait-injected) including the `nvidia-smi` fallback path forced on. MXC policy round-trip via `wxc-exec.exe` in Learning mode on a canary command (Windows-only CI lane).
- **Integration (Tauri boundary).** Command/event contract tests: every Tauri command has a typed request/response test; event streams (telemetry tick, log line, download progress) asserted for shape and ordering under a scripted Rust backend with the real webview headless where feasible.
- **E2E (tauri-driver + WebDriver).** First-run wizard on a clean Windows VM snapshot: prerequisites detected, one model downloaded (small test GGUF), backend started, dashboard shows green — screenshot-diffed against baselines for layout regressions. Post-setup flows: restart a backend from its card and assert the status transitions running → restarting → running with log lines; edit n_ctx and assert the diff preview then the applied flag on the process command line. These run nightly, not per-commit (VM cost).
- **Hardware-in-the-loop (the 7865 itself).** Real-GPU suite run on Pete's machine on demand: NVML vs `nvidia-smi` agreement within tolerance, actual tensor-split load of Qwen3-8B across A4000+P100 with observed-vs-configured split comparison, VRAM accounting reconciles (sum of attributed processes ≈ NVML used), thermal/power readings sane under load. Marked `#[ignore]` in CI; runbook in the repo.
- **Manual.** Checklist per release: wizard on a fresh user profile; driver-update row; all 8 windows exercised; theme switch; drawer auto-expand/collapse; STALE indicator by blocking the telemetry poll; failure injection (kill a backend mid-request, unplug nothing — kill the process) and confirm the dashboard flags it within one poll interval with logs attached; accessibility pass (keyboard nav through the sidebar and all dialogs, focus visible, no color-only status).

### What gets tested per component
- **Wizard:** stage ordering enforced (blocked rows), skip recorded as state, failure rows show log lines + retry, "Show commands" output matches what actually executes (test asserts the displayed command string equals the spawned argv).
- **Supervisor:** crash backoff caps, graceful drain timeout then SIGKILL-equivalent, port-conflict detection before spawn, health-check flapping (3 strikes → degraded, not flapping UI).
- **Telemetry:** 1s tick under load without event-queue backlog (assert webview receives ≤1.2x ticks sent over 60s), STALE marker appears when ticks stop, peak marks reset.
- **Config:** invalid values rejected with field-level messages; diff preview completeness (every changed flag appears); apply failure rolls back to last-known-good and reports.
- **Models:** fit-check recomputes on VRAM change events; checksum mismatch blocks "mark installed"; delete guarded when a backend references the file.
- **MXC:** policy JSON validates against schema before spawn; Learning-mode report parses and proposes tightenings; Enforcement-mode denial surfaces in Recent activity with the denied path/action.
- **Updates:** version comparison edge cases (build tags, `-cuda` suffixes); pinned components never auto-update; failed update leaves previous version runnable.

### How
Rust: `cargo test` + `cargo nextest` in CI (Windows + Linux lanes; Windows-only tests gated). Frontend: `vitest` + `svelte-check`. E2E: `tauri-driver` on a dedicated Windows 11 VM with nested virtualization off (no GPU — stub servers; GPU assertions are hardware-in-the-loop only). Lint/format: `cargo clippy -- -D warnings`, `cargo fmt --check`, `eslint`. Coverage: `cargo-tarpaulin` for the Rust core (target 70% on supervisor/config/policy modules; telemetry excluded where hardware-bound). Every release cuts a diagnostic bundle from the E2E run and archives it with the installer artifact.
