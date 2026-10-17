# Design A — Dense Developer Console (IDE-adjacent)
Local LLM Service Manager — design direction
Prepared: 2026-10-08. No code. Status: brainstorm candidate, to be chosen against other directions.

## Direction name
Dense Developer Console (IDE-adjacent). Working title for the app: **LLM Service Console**.

## Philosophy (3 lines)
Everything the stack is doing, visible at once — services, GPUs, models, logs — in one dense window, VS Code-adjacent. Keyboard-driven and terminal-dense; no marketing surface, no hidden state, no single "running" toggle standing in for five real processes. Configuration is edited where it lives (per-object, with validation), never in a modal maze.

## 0. Where this direction deliberately diverges from the research brief, and why

1. **Flat services table as the default console view, tree as secondary.** The brief's strongest pattern is the Proxmox tree + context tabs. This direction keeps the tree, but defaults the console to a flat services table (Unraid Docker-page style). Reason: the #1 day-to-day action is "restart backend N" or "check why backend N died" — a table puts status, port, model, VRAM, uptime, and restart per backend in one scannable row. The tree is available for the hierarchy (gateway → backends → models; GPUs → processes) but hierarchy is not the daily scanning shape. No state is hidden either way; both views read the same object store.
2. **Config lives per-object in tabs, plus a single Defaults page — no separate Admin Panel.** The brief borrows Open WebUI's Admin Panel as a distinct configuration surface. This direction rejects that for a single-user desktop app: every object (backend, gateway, model, MCP server, MXC policy set) has a Config tab in its context panel with validated structured editing, and there is one global "Defaults" page holding the global-default → per-model/backend override chain. This kills the "where does this setting live" problem the brief flags as anti-pattern 3 (admin maze).
3. **Logs are both a drawer and a page.** The brief specifies the always-visible Proxmox-style task drawer as the log surface. This direction keeps the drawer (tasks, downloads, startup sequences), but adds a persistent Logs page for cross-backend search, filter-by-process, and full-history browsing — because a collapsed drawer cannot hold a searchable 200k-line llama-server log, and tailing five processes at once needs a real view, not a strip.
4. **Fit-check is a hard gate, not an indicator.** The brief reuses LM Studio's "fits in available VRAM" indicator. This direction makes it a blocking check in the download flow: the manager computes projected VRAM placement per GPU (tensor-split ratios included) before the download starts, and a model that cannot be placed without over-committing is refused with the numbers shown, unless the user explicitly overrides. Downloading 15 GB that cannot run is the failure mode; an indicator alone does not prevent it.

---

## 1. Mockups (structured textual; 8 windows)

Conventions used throughout: monospace-dense layout, ~13px base, tabular numerals. Status vocabulary, used everywhere without exception: `ok` (green), `degraded` (amber), `failed` (red), `starting` (blue pulse), `stopped` (grey), `unknown` (grey italic — used for unreachable, never "No data" with no explanation). Every action that takes >1s is a tracked task: queued → running → done/failed, with the triggering log lines attached. No action is silent; no listing goes stale (all views poll or subscribe; a refresh age is shown in the status bar).

---

### Mockup 1 — Stack Provision (first-run setup wizard)
**Purpose:** Turn first run into a provisioning run with an ordered dependency chain, not a marketing funnel. Covers: GPU driver check, CUDA runtime, llama.cpp CUDA build, LiteLLM, 4 GGUF downloads with fit-check, VS Code extension install (Continue.dev, Cline), MCP server registration (GitHub, Google Drive), Windows ML backend registration, MXC policy baseline, final health check. Re-running later repairs drift; the same wizard is reachable as "Reprovision" from Settings.

**Layout (top-to-bottom, left-to-right):**
- **Top bar (fixed):** left: app name + "Stack Provision"; center: overall progress (step N of M, aggregate bar); right: Close (allowed mid-run; completed steps persist; re-running resumes after the last green step), "Open logs" toggle.
- **Main split (two columns):**
  - **Left column (60%): staged checklist.** Ordered phases with dependency ordering enforced:
    - Phase 1 Prerequisites: NVIDIA driver version (detected vs required, with Download button linking the exact driver), CUDA toolkit presence, Windows build check, disk space for models (shows GB free vs required ~85 GB).
    - Phase 2 Core binaries: llama.cpp CUDA build (version, source, download/verify), LiteLLM (pip/venv path, version pin).
    - Phase 3 Models: four GGUF rows — Qwen3.8-27B planner, Qwen3-Coder-30B, Qwen2.5-Coder-7B, Qwen3-8B tool-runner. Each row: name, quant, size on disk, target placement (e.g. "A4000, tensor-split 1.0"), fit-check result (pass/fail with free-VRAM math), download progress bar, checksum verify status. Downloads queue in placement order; pause/resume per row; tray notification on completion.
    - Phase 4 Integrations: VS Code detected (yes/no), extensions install status per extension, MCP servers (GitHub, Drive) with config-file write confirmation, Windows ML llama.cpp backend registration (endpoint registered, /v1 reachable), MXC policy baseline written (policy file path shown).
    - Phase 5 Health check: starts each backend, hits each /v1/models endpoint, runs a 1-token smoke completion per backend, reports pass/fail per backend with a link to its startup log.
  - **Per-row elements:** status icon + status word, name, detected vs required version, action button (Install / Update / Repair / Skip — Skip requires a reason recorded in the run report), inline live log expander (last 6 lines of that step's output, full log in the drawer).
  - **Right column (40%): live run log.** Scrolling terminal pane (monospace, color-coded lines) of the current step's output, auto-scrolling with a "follow" toggle; above it, a compact GPU strip (3 mini cards: name, VRAM free GB) so model-download fit can be sanity-checked mid-run.
- **Bottom bar:** "Run all" (default), "Run from first failing step", "Export run report" (markdown: per-step status, versions, timings, log excerpts). Estimated remaining time from measured step durations, not guesses.
- **Primary interactions:** click a step to expand its detail and logs; retry an individual failed step (validates only its prerequisites); skip with recorded reason; abort stops after the current step. Keyboard: Up/Down move between steps, Enter expands, R retries, L jumps to that step in the full log.

---

### Mockup 2 — Service Console (main dashboard, default view)
**Purpose:** Answer "what is running, where, and is anything wrong" in under 5 seconds. This is the app's home after provisioning.

**Layout:**
- **Top bar (fixed):** left: window title + global status summary (worst-of roll-up: "3 ok · 1 degraded · 1 failed"); center: global search (Ctrl+K — jumps to services, models, GPUs, config keys); right: tray/notifications bell (severity counts: Alert/Warning/Notice), app settings gear, window controls.
- **Left rail (icon + label, 200px):** Console (this view), GPUs, Models, Logs, Components, Policies (MXC), Settings. Badge counts on rail icons (failed services on Console, pending updates on Components).
- **Main area, top-to-bottom:**
  1. **Services table (primary, ~55% height):** rows for each managed process: LiteLLM gateway + 4 llama-server backends (planner/A4000, coder-30B/P100 pool, coder-7B/P100 pool, tool-runner/side-loaded) + registered Windows ML backend (marked "external", read-only-ish). Columns: status dot+word, name, role tag (planner / coder / tool), endpoint (port, click to copy), loaded model, GPU assignment, VRAM held (GB), uptime, tok/s (last 60s, sparkline inline), restart count (24h), row actions: Restart, Stop/Start, Logs (jumps to Logs page filtered), Config (opens context panel Config tab). Failed rows expand inline to show the last error line + "Open startup log". No single master toggle anywhere.
  2. **GPU strip (~20% height):** 3 compact cards side by side (RTX A4000 16GB, Tesla P100 #1 16GB, Tesla P100 #2 16GB). Each: utilization % (large), VRAM bar (used/total GB), temp °C, power W, 60s sparkline; below the bar, per-process VRAM attribution lines ("llama-server planner: 13.8 GB"). Cards are clickable → GPUs page.
  3. **Task drawer (bottom, collapsible, always present):** live tail of running/finished tasks (downloads, restarts, installs): task name, progress, elapsed, abort button on running tasks. Double-click opens full log. Collapsed height = one line; expands to 40% of window.
- **Right context panel (slide-in, 380px):** opens on row selection; tabs: Summary | Config | Logs | Test. Summary: resource bars (VRAM on assigned GPUs, context usage), launch flags as read-only chips, endpoints. Config: structured editor (see Mockup 6 pattern, scoped to this backend). Logs: tail of this process only. Test: small completion probe — prompt box, "Send", streaming output, token timing stats (TTFT, tok/s) — the permitted small backend-test utility, never a chat home.
- **Primary interactions:** restart/stop/start per row; Ctrl+K search; click GPU card → GPU detail; keyboard: j/k move rows, x restart selected, l open logs, c open config. Sorting columns; filters: All / Running / Failed / Degraded.

---

### Mockup 3 — GPU Detail (per-GPU page; also the GPUs rail page)
**Purpose:** Deep hardware view for one GPU at a time, plus cross-GPU placement truth. Replaces any need for Task Manager during tuning.

**Layout (GPUs rail page):**
- **Left sub-rail:** GPU 0 (RTX A4000), GPU 1 (Tesla P100), GPU 2 (Tesla P100) — each entry shows the busiest-engine utilization % (Task Manager rule: never let an idle-looking average hide load) and VRAM %.
- **Center, top-to-bottom:**
  1. **Metric block (Task Manager-style):** 60s scrolling area charts for utilization %, VRAM used/total, temperature, power draw vs limit. Below: readouts — driver version/date, CUDA version, clocks (graphics/memory), PCIe bus/device/function, power limit, fan (N/A noted explicitly for P100s if unreadable — never blank).
  2. **Process attribution table:** PID, process name, backend association (links to the service row), VRAM per GPU, % of this GPU. Sortable by VRAM. Rows for llama-server processes, plus any foreign consumers (flagged "unmanaged" — e.g. a stray python process holding 2 GB — with a Kill action behind a confirm).
  3. **Model placement map:** which models' layers live on this GPU (tensor-split fractions, e.g. "Qwen3-Coder-30B: 42% of layers"), KV-cache allocation, context-size headroom. This is the cross-GPU truth the monitoring tools surveyed never show.
- **Right side:** "Compare" toggle — overlays the other two GPUs' utilization sparklines in muted grey behind the selected GPU's chart, for spotting imbalance during tensor-split tuning.
- **Primary interactions:** select GPU in sub-rail; kill unmanaged process (confirm dialog with the exact command shown); "Rebalance placement" suggests tensor-split ratio changes when one GPU is saturated and another idle (suggestion only — applies via the backend's Config tab, with validation); peak/hold markers resettable (high-water marks per metric).

---

### Mockup 4 — Model Library (GGUF management)
**Purpose:** Own the four-model set: what is on disk, what fits, what is loaded where, downloads and updates. LM Studio's discovery pattern, hardened for a fixed fleet.

**Layout:**
- **Top:** storage summary bar (models directory, used/free GB), "Add model" (URL or HF repo + file picker with GGUF validation), "Refresh".
- **Table rows, one per model (the 4 Qwen models + any added):**
  - Columns: name, quant, size on disk, parameters, context window, role assignment (planner/coder/tool — editable dropdown, since routing policy is the point), placement target (backend + GPU split), fit-check (projected VRAM vs free, pass/fail with numbers), loaded status (which backend process, since when), actions: Load/Unload (per backend), Download/Re-download, Verify checksum, Delete (confirm; refuses while loaded).
  - Expanded row: download history, checksum, source URL, per-GPU layer map when loaded, "used by" (Continue.dev model config references — shows which editor configs point at it, so renames don't silently break the IDE).
- **Download queue section (below table):** queued/in-progress downloads with progress bars, speeds, ETA, pause/resume/cancel; completed downloads auto-verify checksum then run fit-check; failures show the error and a retry that resumes.
- **Primary interactions:** drag-free; all keyboard accessible (d download selected, v verify, Del delete with confirm). Fit-check re-runs on demand ("Recompute against current GPU state").

---

### Mockup 5 — Logs
**Purpose:** First-class log surface: per-process tails, cross-process search, history. The drawer handles live tasks; this page handles everything else.

**Layout:**
- **Left filter rail:** process list with status dots (gateway, 4 backends, Windows ML backend, installer/provision runs, MXC policy events). Multi-select. Below: level filter (error/warn/info/debug), free-text search, time-range picker, "errors only" toggle.
- **Center:** unified or per-process stream (toggle: "Merged" vs "Split"). Merged view prefixes each line with process tag + timestamp; Split shows one pane per selected process side by side. Monospace, 1M-line ring buffer per process with "load older" paging from the on-disk log files. Auto-scroll follow toggle; pause button freezes without losing the buffer.
- **Line interactions:** click a line → detail popover (full line, parsed fields if JSON logs, "copy", "find similar", "jump to config" for flag-parse errors). Error lines link to the relevant backend's Config tab.
- **Top-right:** Export (filtered range to file), Clear buffer (never deletes on-disk), live indicator ("streaming" vs "paused" vs "process dead — showing last N lines").
- **Primary interactions:** Ctrl+K within page searches line content; e toggles errors-only; m toggles merged/split; clicking a backend name jumps to its row in Console.

---

### Mockup 6 — Settings / Configuration (Defaults + component inventory + app prefs)
**Purpose:** All configuration that is not per-object, plus the standing component inventory. Replaces the Open WebUI-style admin maze with one page and per-object tabs.

**Layout (tabbed page):**
- **Tab 1 — Defaults:** the global-default → override chain. Sections: llama-server launch flags (tensor-split default, threads, batch size, context size, KV-cache quant, parallel slots), LiteLLM gateway (port, routing table template, timeouts, retries), logging (levels, retention), update policy (check cadence, auto vs manual). Each field: type-validated control, "changed from default" marker, "requires restart of: backends/gateway" marker. Per-backend overrides live in the backend's Config tab (Mockup 2) and are shown here as a read-only summary table ("planner: --tensor-split 1.0,0,0 (override)").
- **Tab 2 — Components:** UniGetUI-style inventory table: every managed component (driver, CUDA, llama.cpp build, LiteLLM, each GGUF, each VS Code extension, each MCP server, MXC policy pack, Windows ML registration). Columns: name, required version, installed version, status (current/outdated/missing/failed), last checked, action (Update/Repair/Reinstall). Bulk "Update all" with the same task-drawer feedback. This is where version drift gets repaired after first-run.
- **Tab 3 — Integrations:** VS Code (path, detected extensions, reinstall), MCP servers (GitHub, Drive: config file paths, enable/disable, "test connection"), Windows ML backend (endpoint, re-register), MXC summary (policy pack version, link to Policies page).
- **Tab 4 — App:** theme (dark default; light), log retention, task history retention, tray behavior (minimize to tray, notifications on task completion/failure), keyboard shortcut reference (full list, editable), telemetry (off by default; explicit opt-in), data directory locations (models, logs, configs — all movable).
- **Primary interactions:** every edit validates inline (bad flag value blocks save with the reason); "Apply" vs "Apply + restart affected services" is an explicit choice, never implicit; config writes are journaled (previous versions kept, "Restore" per object).

---

### Mockup 7 — MXC Policies
**Purpose:** Visible, editable sandbox policy for agent tool execution — not a hidden config file. MXC ships a Rust SDK; this page is the human face of it.

**Layout:**
- **Left:** policy sets list (e.g. "default-agent", "code-exec", "file-write") with status (active/inactive), last modified, modified-by.
- **Center:** structured rule editor — not raw JSON. Rule rows: tool pattern (e.g. `shell.*`), scope (paths/commands allowed), decision (allow/deny/prompt), conditions. Add/edit/delete rows; validation against the MXC schema; "simulate" mode — paste a tool call, see allow/deny + which rule matched, without executing.
- **Right:** recent policy decisions log (timestamp, tool call, decision, matched rule) — the audit trail. Filter by decision.
- **Top:** "Dry-run toggle" (log decisions without enforcing — for testing new policies), policy pack version, "Deploy" writes the policy file and hot-reloads the sandbox (or marks restart-required where hot-reload is unsupported).
- **Primary interactions:** simulate-before-deploy workflow is the default path; every change is versioned with a diff view ("what changed since v12").

---

### Mockup 8 — Provision/Component History (auditable past)
**Purpose:** Answer "what changed, when, and did it work" — the memory of the setup job. (Could merge into Components tab; kept separate in this mockup for density control.)

**Layout:**
- **Timeline table:** every provisioning run, update, repair, config change: timestamp, actor (user/app/scheduled), component, from→to version, result (done/failed), duration, link to full task log.
- **Drift alerts:** components whose installed version no longer matches the pinned requirement get a row here and a badge on the Components rail icon.
- **Primary interactions:** click a row → full log of that run; "Roll back" on config changes (restores the journaled previous version); export history as markdown.

---

## 2. Design document

### Goals
1. **One window tells the whole truth.** At any moment Pete can see every backend's status, every GPU's load and VRAM attribution, every model's placement, and every running task — without opening a second surface. Nothing meaningful lives only in a modal or a chat panel.
2. **Setup is a provisioning run, not a funnel.** Ordered, resumable, per-step visible, with live logs and an exportable run report. Re-running repairs drift; it is the same surface, not a separate "repair" flow.
3. **Config is structured and validated where it lives.** Per-object Config tabs plus one Defaults page; global defaults with per-backend overrides; bad values are rejected with reasons, never silently written.
4. **Every action has visible feedback and a log.** Queued → running → done/failed, with the log lines attached. Unreachable backends read as errors with the last-known state, never as empty panels or "No data".
5. **Keyboard-first, mouse-optional.** Full shortcut map; every list navigable; every action has a key. Density serves scanning speed, not decoration.

### Layout system
- **Shell:** fixed top bar (title + global roll-up status + search + notifications + settings) → left rail (7 sections, icon + label, badge counts) → main content (view-specific) → bottom task drawer (always present, collapsible to one line) → status bar (refresh age, connection states, version).
- **Grid:** 12-column; content max density ~13px base font, tabular numerals everywhere numbers appear. Panels have 1px borders, 4px radius, no shadows, no gradients. Two accent colors only: status green/amber/red/blue-grey; interactive elements use a single neutral accent (VS Code blue family).
- **Views:** Console (table + GPU strip), GPUs (sub-rail + charts + attribution + placement), Models (table + queue), Logs (filter rail + stream), Components/Settings (tabs), Policies (list + editor + audit), History (timeline).
- **Context panel:** right slide-in (380px), object-scoped tabs (Summary | Config | Logs | Test). Same component reused for services, models, GPUs, MCP servers.

### Navigation model
- Rail-driven, one level deep; sub-navigation inside a view (GPU sub-rail, Settings tabs) rather than nested rails. Ctrl+K global search jumps to any object, config key, or log pattern. Breadcrumb in the top bar for context-panel depth ("Console > llama-server planner > Config"). No view is more than two clicks from any other; the task drawer is reachable from everywhere because it never leaves.
- First-run routing: the app opens Stack Provision on first launch (per the brief's reusable pattern 8), not an empty console. After provisioning completes, the console becomes the default; provisioning remains reachable as Reprovision.

### Information hierarchy
1. **Status first:** worst-of roll-up in the top bar; per-object status dots in every list; failures expand inline to the error line.
2. **Resources second:** VRAM held, per-process attribution, placement maps, tok/s — the numbers Pete tunes against.
3. **Actions third:** restart/stop/config/logs inline on the object they affect; destructive actions confirm with the exact command shown.
4. **History fourth:** logs, task history, config journal — one click away, never the default view.

### Visual language (declarative)
- Dark theme default; light theme supported. Monospace for logs, numbers, flags, paths; proportional sans for labels and prose.
- Status is carried by dot + word, never color alone. Utilization bars are flat fills with numeric labels; sparklines are 1px lines, no area fill.
- No metaphors, no avatars, no illustrations, no marketing copy. Titles are nouns ("Service Console", "Model Library", "Stack Provision"). Buttons are verbs ("Restart", "Verify checksum", "Deploy").
- Empty states state the cause and the fix ("Backend unreachable on :8081 — last seen 14:02. Check process or view startup log."), never "No data".

---

## 3. Implementation plan

### Stack constraint
When written, this software will be written in **Rust**. That makes **Tauri (Rust backend + webview UI)** the primary candidate: the UI layer is HTML/CSS/JS running in the OS webview, with Rust behind it for everything system-level — process management (spawning/supervising the llama-server processes and LiteLLM), GPU telemetry (NVML via Rust bindings), config file management, and MXC policy enforcement via the MXC Rust SDK. Tauri produces small single-binary Windows installers via CI with no runtime dependency.

### Candidate stacks, weighed honestly

**A. Tauri 2.x — Rust core, webview UI (PRIMARY CANDIDATE)**
- Fit: the Rust constraint is satisfied natively. Process supervision, NVML telemetry, file watching, and the MXC Rust SDK all live in the same Rust process — no FFI boundary, no second runtime. llama.cpp Rust bindings (llama-cpp-2) exist if the app ever wants in-process inference probing, though the design only needs to supervise llama-server processes, not link them. Single-binary MSI via CI, small footprint (~10 MB), no Electron-style Chromium bundling, uses the system WebView2 on Windows 11 (present by default).
- Risks, stated plainly: the webview is a browser engine, so the dense console UI (virtualized tables, 60 Hz sparklines, multi-pane log streaming) must be built with care for performance — WebView2 is capable, but it is not a native retained-mode UI; long log buffers and live charts are the stress points and need virtualization from day one. Tauri's plugin ecosystem covers the basics (fs, process, notification, tray, updater) but is thinner than Electron's; anything exotic (global hotkeys, custom window chrome behaviors) costs custom Rust. Multi-window support exists but is less mature than Electron's. Developer velocity for the UI layer is web-standard (good), but the team must be comfortable in both Rust (backend) and a web framework (frontend).
- Verdict: best fit for the stated constraint and the system-integration workload, provided the UI is engineered for webview performance from the start.

**B. Electron**
- Fit: the most mature desktop-web stack; multi-window, tray, and updater stories are battle-tested; the dense UI patterns here (virtualized tables, live charts) are well-trodden ground. Rust could still be used via a sidecar binary or NAPI module, but that reintroduces the two-runtime complexity the constraint is trying to avoid.
- Tradeoffs: ~150 MB+ installer, full Chromium per app, higher idle memory — noticeable on a machine whose RAM is budgeted for 96 GB of model serving, though in practice the console is a lightweight process. Two-language maintenance (Rust sidecar + JS) if the Rust constraint is honored; pure-JS if it is relaxed.
- Verdict: the safe UI bet and the fastest path to a polished dense console, but it fights the Rust constraint and ships the heaviest runtime of the options.

**C. .NET WinUI 3 (Windows App SDK)**
- Fit: genuinely native Windows UI; best possible Task-Manager-grade fidelity, lowest UI overhead, first-class Windows services/tray integration, and C# interop with NVML is straightforward. Single-platform is fine — this app is Windows-only by definition.
- Tradeoffs: it is not Rust. Honoring the constraint would mean Rust behind a C ABI consumed from C#, which is workable (Rust cdylib + P/Invoke) but splits the codebase across two ecosystems and two build chains — the worst of both worlds for a small project. WinUI 3's control toolkit is thinner than the web ecosystem for dense data-viz (virtualized grids, sparklines, log viewers all need custom or third-party controls), and the Windows App SDK's deployment story (packaged MSIX vs unpackaged) adds friction the other options avoid.
- Verdict: the best native-UI option, but the poorest fit for the Rust constraint; only worth it if native UI fidelity outranks the single-language requirement.

**D. Locally-served web dashboard (Rust backend serving localhost UI)**
- Fit: Axum/Actix backend in Rust serving a web UI to the browser — zero desktop-framework code, the UI is plain web, and remote access from another machine on the LAN falls out for free. Deployment is a single Rust binary + static assets.
- Tradeoffs: no native window, tray icon, notifications, or autostart without additional work (a small companion or manual setup); the "app" feel Pete expects from a desktop service manager is lost unless a thin wrapper is added — at which point it becomes a worse Tauri. Browser tab management (accidental closes, tab sprawl) is a real UX cost for an always-on console.
- Verdict: the simplest to build and a fine headless/admin mode, but not the desktop app this design describes. Worth keeping as a secondary interface (the Rust backend's HTTP API could serve both the Tauri webview and an optional browser view).

**Recommendation:** Tauri as primary, with the Rust backend exposing a clean internal API (commands/events) that a future browser-served mode could reuse — i.e., architect the backend so option D falls out later without rework. Electron is the fallback if webview performance or multi-window needs prove painful in prototyping; re-evaluate after the Milestone 2 prototype, not before.

### Rust-specific integration points
- **Process supervision:** Rust spawns and supervises the 4 llama-server processes + LiteLLM (child process management, restart policies, stdout/stderr capture into the log pipeline). No shelling out to fragile scripts.
- **GPU telemetry:** NVML via Rust bindings (e.g. `nvml-wrapper`) for per-GPU utilization, VRAM, temps, power, and per-process VRAM attribution — the data behind Mockups 2 and 3. Poll at 1–2 Hz; push to the UI over Tauri events.
- **MXC sandbox:** MXC's Rust SDK integrates directly into the backend for policy enforcement and the simulate/audit features of Mockup 7 — no subprocess or FFI seam.
- **llama.cpp Rust bindings (llama-cpp-2):** not required for the v1 design (the app supervises llama-server processes over HTTP), but available if a future "in-process smoke test" or direct inference probe is wanted. Keep the HTTP boundary in v1; bindings are an optimization, not a dependency.
- **Config management:** Rust owns the config files (llama-server flags, LiteLLM YAML, MXC policies) with serde-based typed models — validation errors surface as structured data to the UI, which is what makes the "reject bad values with reasons" design real rather than aspirational.
- **Installer:** Tauri bundler produces a small MSI via CI; no runtime dependency beyond WebView2 (inbox on Windows 11).

### UI layer (inside Tauri)
Web framework: a reactive component framework with fine-grained updates (Svelte/Solid preferred over React for this density — fewer re-render costs on 60 Hz telemetry; final choice at prototype). Virtualized tables and log views from day one; canvas-based sparklines; state via Tauri events (backend → UI push) rather than polling from JS.

### Milestones and build order
1. **M1 — Backend core (Rust):** process supervisor (spawn/supervise/capture logs for N processes), NVML telemetry loop, typed config models (serde) for one backend + LiteLLM, task runner with progress/abort. CLI-testable without UI.
2. **M2 — Console prototype (Tauri shell + UI):** services table, GPU strip, task drawer, per-process log tail. Performance spike: virtualized log view + 2 Hz telemetry push; go/no-go on Tauri vs Electron fallback here.
3. **M3 — GPU detail + placement map:** per-GPU charts, process attribution, tensor-split placement view.
4. **M4 — Model library:** download queue with resume, checksum verify, fit-check gate against live NVML data.
5. **M5 — Stack Provision wizard:** ordered staged checklist on top of the M1 task runner; prerequisite checks (driver, CUDA, disk); run reports.
6. **M6 — Config system:** Defaults page + per-object Config tabs, validation, restart-requirement markers, config journal/rollback.
7. **M7 — Logs page + MXC Policies page:** merged/split streaming, search; policy editor with simulate mode via the MXC Rust SDK.
8. **M8 — Components/updates + history:** inventory table, update flows, drift detection, audit timeline; tray integration, notifications, autostart; installer (MSI) via CI.
9. **M9 — Hardening:** installer end-to-end on a clean Windows 11 VM, first-run provision of the full stack, 72-hour soak (backend crashes, GPU pressure, log volume), then release.

Build order rationale: supervision + telemetry first (everything reads from them), console before wizard (the wizard reuses the task runner the console already exercises), config and policies after the objects they configure exist.

---

## 4. Test plan

### Levels
- **Unit:** Rust backend modules in isolation. **Integration:** backend subsystems against real local dependencies (NVML, process supervision, config files). **UI component:** webview components with mocked Tauri events. **End-to-end:** full app against stubbed or real backends. **Manual:** checklist-driven passes on real hardware (the 7865 itself).

### What gets tested per component, and how
- **Process supervisor (Rust, unit + integration):** spawn/stop/restart semantics, restart-policy backoff, stdout/stderr capture ordering, kill of process trees (no orphaned llama-server), port-conflict detection. Integration: supervise a stub HTTP server binary; kill -9 it mid-run and assert restart + log continuity. Unit: backoff math, state machine transitions.
- **NVML telemetry (integration + manual):** per-GPU utilization/VRAM/temp/power readings sane and updating; per-process attribution sums to ≤ total; handles GPU-busy-by-other-process. Integration against real NVML on the 7865 (no GPU in CI — this suite is hardware-gated and runs on the target machine). Manual: cross-check numbers against nvidia-smi during a loaded inference run.
- **Task runner (unit + e2e):** queued → running → done/failed transitions, abort mid-download resumes correctly, progress reporting accuracy, failure attaches log excerpt. E2E: run a multi-step provision against stub installers.
- **Fit-check gate (unit + integration):** projected VRAM math per tensor-split ratio vs live free VRAM; refusal case blocks download with the numbers; override path records explicit user consent. Unit: placement math on fixtures. Integration: against live NVML with the real 4-model set.
- **Config system (unit + e2e):** serde validation rejects bad flag values with structured reasons; defaults → override merge order; restart-required markers correct per field; journal rollback restores byte-identical previous config. E2E: change tensor-split in UI, assert backend restarts with the new flags and the old config is restorable.
- **Model downloads (integration + e2e):** resume after abort, checksum verification failure quarantines the partial file, queue ordering respects placement order. Integration against a local stub file server (no real HuggingFace in CI).
- **Provision wizard (e2e + manual):** full ordered run on a clean Windows 11 VM snapshot: prerequisites fail correctly when the driver is missing, steps resume after the last green step, run report exports. Manual: the real first-run on the 7865.
- **Console UI (component + manual):** services table renders 6 rows with live tok/s sparklines; virtualized log view holds 1M lines without frame drops (performance assertion in the component test with synthetic data); keyboard shortcuts all fire. Manual: density/scan-time review with Pete — "can you find the failed backend in under 5 seconds."
- **Logs page (component + e2e):** merged/split views, search across processes, follow/pause semantics, error-line → config deep links resolve.
- **MXC policies (unit + e2e):** simulate mode verdicts match enforced verdicts on a fixture corpus of tool calls; deploy writes valid policy the SDK accepts; audit log records every decision. Never executes real tool calls in tests — fixtures only.
- **Updates/components (e2e):** drift detection flags a manually-downgraded component; update flow runs through the task drawer with notifications.
- **Installer (manual + CI):** CI builds the MSI per commit; manual install/uninstall on clean VM asserts: single binary, no runtime prompt, autostart + tray work, first-run routes to Stack Provision.
- **Soak (manual):** 72-hour run on the 7865 with all backends loaded: no telemetry drift, no log-buffer unbounded growth, backend crash → failed status + inline error + notification within 30s, restart recovers.

### Test infrastructure notes
- Rust: `cargo test` for unit; integration tests in `tests/` with hardware-gated suites behind a `gpu` feature flag (skipped in CI, run on the 7865).
- UI: component tests with mocked Tauri event streams; Playwright-style e2e against the built Tauri app for the wizard and config flows.
- No GPU in CI is accepted and explicit: anything touching NVML is tested on the target machine, never faked in CI beyond fixtures.
