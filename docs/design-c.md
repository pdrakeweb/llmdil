# Design C — Cluster Service Map (NOC-style)

Direction: the Dell Precision 7865 is treated as a small inference cluster.
The UI is its live service map. Setup is cluster bring-up. Nothing hides
behind a single toggle; every node is addressable.

## Philosophy

1. The machine is a small cluster; the UI is its live service map —
   clients → gateway → backends → GPUs → models — with status propagating
   up the graph and capacity visible on every edge.
2. Every component is addressable: each backend process, GPU, model, MCP
   server, and MXC policy node has its own status, metrics, logs, and
   actions. Multi-backend reality is never collapsed into one switch.
3. Setup is staged infrastructure bring-up: prerequisites → components →
   models → services → policies → launch, with the topology map filling in
   live as each node comes online — then remaining as the permanent home view.

## Deliberate divergences from the research brief

- **Graph-first navigation instead of Proxmox tree-plus-tabs as the primary
  surface.** The brief's pattern 2 (tree + context tabs) is retained as a
  compact secondary view, but the home view is a spatial service map. Reason:
  this system is small (<40 nodes) and fixed in structure; adjacency in a
  layered graph carries the dependency information (which backend feeds which
  GPU, which policy bounds which backends) that a tree makes you reconstruct
  by expanding branches. A tree hides relationships; the graph shows them.
- **Topology map inside the setup wizard (Bring-up stage).** The brief does
  not propose this. Reason: staged bring-up needs a single surface that
  answers "what is up and what is not yet" without reading a log. The map
  that the operator will live in afterwards is the right surface to watch
  the cluster come online.
- **Alerts as a first-class page with user-editable rules.** The brief folds
  alerting into notifications/Unraid-style severity lists. Reason: Pete runs
  long unattended inference sessions; he needs threshold rules he defines
  (VRAM headroom, queue depth, backend down) with log correlation, not just
  a notification feed.

What is kept from the brief: staged component rows with per-row status
(UniGetUI pattern), per-GPU cards with process-level VRAM attribution
(NVIDIA dashboards + Task Manager), model catalog with VRAM fit-check
(LM Studio), service rows with inline restart and per-service logs (Unraid),
page-based settings separate from monitoring (Open WebUI admin), validated
structured config editing (OpenClaw), first-run landing on setup not an empty
dashboard (LM Studio). Anti-patterns respected throughout: no chat-primary
surface (the backend Test tab is a utility, not a home view), no single
"server running" toggle, no modal wizards for routine config, no decorative
metaphors, no silent or stale state (every action reports queued → running →
done/failed with the log line attached).

---

# PART 1 — WINDOW MOCKUPS

Conventions used in every mockup below: status dots are green (healthy),
amber (degraded/warning), red (down/error), blue (info/running operation),
grey (idle/unknown). All numbers are monospace. "Drawer" means the bottom
task/event drawer, always present in the console, collapsible to a ticker.

---

## M1 — Setup Wizard: First-Run Provisioning

Purpose: staged bring-up of the full stack — prerequisites, components,
models, service definitions, MXC policies, launch — ending on the live
service map. Runs once; re-runnable per stage later from Settings.

Layout, top to bottom:
- Header: window title "Provisioning — Local LLM Service", stage stepper
  (1 Hardware check, 2 Components, 3 Models, 4 Services, 5 Policies,
  6 Bring-up) with per-stage status dot; right side: "Re-run checks",
  "Save & exit" (resumes later).
- Main area: stage content (see per-stage below). Footer: Back / Next
  (Next disabled until the stage validates; the validation message names
  the blocking row), plus a stage-specific primary action.
- Bottom drawer: operation log stream (install output, download progress)
  with Abort on running operations.

Stage 1 — Hardware check. Table rows, each: status dot, check name,
detected value, required value, action.
Rows: GPU 0 RTX A4000 16GB — CUDA capable; GPU 1 Tesla P100 16GB — CUDA
capable; GPU 2 Tesla P100 16GB — CUDA capable; NVIDIA driver version
(detected vs >= required); CUDA toolkit; Visual C++ redistributable;
disk free on model volume (>= sum of selected models + 20% headroom);
network reachability (huggingface.co, pypi); VS Code installed (path).
Failing rows show a Fix button (e.g. "Open driver download", "Install
CUDA toolkit") that queues the fix into the drawer; re-check runs on
completion. Cannot advance while a blocking row is red.

Stage 2 — Components. Staged component list; order encodes the dependency
chain. Columns: order #, component, required version, detected version,
status, action. Rows: llama.cpp CUDA build (source + build flags shown);
LiteLLM (Python package, venv path); Continue.dev extension; Cline
extension; MCP server: GitHub (config present/missing); MCP server:
Google Drive (config present/missing); Windows ML llama.cpp backend
registration. Later rows are disabled until earlier rows are ok.
"Install all" queues the chain; each row streams into the drawer.
Per-row actions: Install / Update / Repair / Details (expands to show
install source, flags, and log excerpt on failure).

Stage 3 — Models. Model catalog table: model name, parameters, quant,
file size, target placement (e.g. "A4000", "P100 pool", "side-load"),
VRAM fit indicator (projected per-GPU VRAM bar after placement),
checksum status, action (Download / Verify / Remove).
Below: download queue with per-file progress bar, speed, ETA,
pause/resume/cancel; completed downloads verify SHA and report into
the drawer. Fit indicator recomputes live as models are added to the
plan; a model that does not fit its target is blocked with the numbers
shown (required vs available per GPU).

Stage 4 — Services. Four pre-filled backend definition rows
(planner-27b, coder-30b, coder-7b, toolrun-8b). Each row expands to:
model dropdown (only downloaded models), GPU assignment checkboxes,
tensor-split ratio fields (validated to sum to 1.0), port, context size,
parallel slots, server flags (advanced, collapsed). "Validate" checks
port clashes, VRAM overcommit per GPU, and missing models; problems are
listed inline on the offending row, and Next is blocked until clean.
Add/remove backend supported.

Stage 5 — Policies. MXC sandbox policy setup: preset selector
("Deny-by-default", "Permissive logging", "Custom"), policy list with
per-rule rows (action allow/deny, scope, tool pattern), and a dry-run
button that evaluates a sample tool call against the policies and shows
allow/deny with the matching rule named.

Stage 6 — Bring-up. Left: ordered start sequence (gateway first, then
backends in dependency order) with per-step status and streamed logs.
Right: the topology service map, filling in live — nodes appear and turn
green as each service reports healthy. Failure stops the sequence at the
failed step with the log excerpt and a Retry button; already-started
services stay up. Final button: "Open console" (lands on M2).

Primary interactions: per-row Fix/Install/Repair; stage validation
gating Next; operation abort in the drawer; dry-run on Services and
Policies stages; resume-after-exit.

---

## M2 — Main Window: Service Map (home view)

Purpose: the NOC home view. One glance answers: what is up, what is
degraded, where is the load, where is the headroom.

Layout, top to bottom, left to right:
- Header bar: app title; machine name (PRECISION-7865); global health
  rollup ("6/7 services healthy" — counts gateway, backends, MCP servers;
  GPUs reported separately as hardware); alert bell with active-alert
  count (click → M7); aggregate throughput (tok/s in+out across backends);
  total VRAM headroom ("11.2 / 48 GB free"); clock.
- Left nav rail (icon + label): Service Map, Backends, GPUs, Models,
  Logs, Alerts, Policies, Settings.
- Center: the topology graph canvas. Layered columns, left to right:
  Clients (VS Code, Continue.dev, Cline, CLI/test) → Gateway
  (LiteLLM :4000) → Backends (planner-27b :8081, coder-30b :8082,
  coder-7b :8083, toolrun-8b :8084) → GPUs (A4000 16GB, P100-0 16GB,
  P100-1 16GB) → Models (one node per loaded GGUF, attached to its
  backend). A second row beneath: MCP servers (github, gdrive) with
  edges to the gateway/backends that use them. MXC policy is drawn as a
  boundary node enclosing the backends, labeled with the active preset
  (e.g. "MXC: deny-by-default"); edges from clients to backends that
  cross it are tagged "policy-checked".
  - Node content: status dot, name, one key metric (GPU: VRAM bar;
    backend: queue depth + tok/s; gateway: req/s; model: params/quant).
  - Edges: backend→GPU edges labeled with tensor-split %; thickness
    proportional to current token throughput; edge color follows the
    worse endpoint's health. Client→gateway→backend edges labeled with
    req/s. A backend with no traffic shows a thin grey edge — visible,
    not hidden.
  - Status propagation: a down backend reddens its edges and marks the
    gateway "degraded" if any child is down; a GPU over 90% VRAM ambers
    and ambers every backend placed on it.
- Right inspector panel (selection context; empty state shows cluster
  summary): for the selected node — status, key metric table, recent
  events (last 5), actions (Restart, Open logs, Open detail page).
- Bottom drawer: event ticker (alerts, operation completions); expands
  to the full event/operation list with per-operation Abort.

Primary interactions: click node → inspector; double-click node → full
detail page (M3/M4); drag to pan, wheel to zoom; layer filter toggles
(show clients, show MCP, show policies); click an alert in the header →
selects the source node and offers "View correlated logs"; hover edge →
tooltip with exact tok/s, split %, error rate.

---

## M3 — GPU Detail Page

Purpose: per-GPU hardware state and VRAM ownership. Opened from the map
(double-click a GPU node) or the nav rail (GPUs → list → select).

Layout, top to bottom:
- Header: GPU name ("GPU 1 — Tesla P100 16GB"), PCI bus/device/function,
  driver version, status dot. Right: time-range selector for the charts
  (1m / 5m / 1h) and "Open nvidia-smi" (copies the equivalent CLI).
- Stat tile row (6 tiles): Utilization % with 60s sparkline; VRAM used /
  total with bar and %; Temperature °C with rising/falling indicator and
  high-water mark (resettable); Power draw W vs limit; Clocks
  (graphics / memory MHz); Fan %.
- "VRAM attribution" table: process name + PID + owning backend
  (e.g. llama-server, pid 4120, backend coder-30b), VRAM MiB, share of
  GPU total %. Row click → backend detail page (M4). Unattributed VRAM
  (driver reserve, unknown processes) shown as its own row — never
  silently absorbed.
- "Tensor-split participation": list of backends splitting across this
  GPU with their split ratios and per-backend resident VRAM.
- Footer note: data source and poll interval ("NVML, 1s poll"), and
  "N/A" rendering for any unreadable field (never blank, never zero).

Primary interactions: time-range switching; high-water-mark reset;
process row → backend page; threshold shortcut ("Create alert rule from
this metric" → M7 with the metric pre-filled).

---

## M4 — Backend Detail Page

Purpose: per-llama-server-instance state, config, logs, and a test
utility. One page per backend; the anti-pattern of a single global
"server running" toggle is structurally impossible here.

Layout, top to bottom:
- Header: backend name (coder-30b), status dot, loaded model
  (Qwen3-Coder-30B-Q4_K_M), port :8082, PID, uptime. Right: Start /
  Stop / Restart buttons; "Reload model" dropdown (only downloaded
  models that fit this backend's GPUs).
- Stat tile row: tokens/s in, tokens/s out, active requests, queue
  depth, KV cache used %, TTFT p50/p99, context usage %.
- Tabs:
  - Overview: placement mini-diagram (this backend's edges to its GPUs
    with split %), server flags summary (read-only chips), recent
    events for this backend.
  - Config: validated form of server flags (context size, parallel
    slots, batch size, tensor-split, GPU layers); fields that require
    restart are badged; Save stages changes and shows exactly which
    services will restart on Apply. Global defaults shown greyed with
    per-backend override toggle (brief pattern 6).
  - Logs: this backend's log tail with follow, severity filter, search;
    "Open in Logs" jumps to M6 pre-filtered.
  - Test: small utility — prompt box, model params (temperature, max
    tokens), Send → streams the response with TTFT and tok/s reported.
    Labeled "Backend test utility", not chat.
- Restart behavior: Stop → Start is two explicit actions; Restart shows
  a progress state on the page and streams into the drawer; a failed
  start leaves the page in an error state with the log excerpt and the
    previous known-good flags one click away ("Revert to last good").

Primary interactions: start/stop/restart/reload; config edit with
restart-impact preview; log tail; test-prompt send.

---

## M5 — Model Library

Purpose: GGUF inventory, downloads with fit-check, and placement
planning across the 3 GPUs.

Layout, top to bottom:
- Toolbar: "Add model" (HF URL or local file), model storage path
  (changeable), disk usage ("184 / 500 GB"), "Verify all checksums".
- Model table. Columns: name, parameters, quant, file size, SHA status
  (verified / pending / failed), target backend(s), currently loaded
  where (backend + GPUs, or "—"), actions (Download, Verify, Load →,
  Delete). Load → opens a backend picker filtered to backends whose
  GPUs can fit it.
- Download queue section: per-file progress bar, speed, ETA,
  pause/resume/cancel; completed items auto-verify and report to the
  drawer; failed items keep their error and a Retry button.
- Placement planner: per-GPU VRAM budget bars (A4000, P100-0, P100-1)
  that update live as target placements change; assigning a model whose
  target GPUs cannot hold it shows an inline overcommit warning with the
  numbers (required vs free per GPU) and blocks the assignment.
- Each model's fit indicator is computed against live free VRAM, not
  totals — a model that fit yesterday may not fit today, and the UI says
  so explicitly.

Primary interactions: add/download/verify/delete; load-to-backend with
fit filtering; placement editing with live budget bars; queue control.

---

## M6 — Logs

Purpose: first-class, correlatable log surface for every managed
process. Not a dialog, not a tab buried in a detail page.

Layout, left to right:
- Left filter column: service tree with checkboxes (LiteLLM gateway,
  planner-27b, coder-30b, coder-7b, toolrun-8b, installer/downloads);
  severity filter (debug/info/warn/error); text search; "merge streams"
  toggle (interleaved by timestamp vs grouped per service).
- Center: log stream. Follow toggle, pause, line wrap, timestamps with
  millisecond precision; error lines highlighted; click a line for
  actions: Copy, "Show surrounding" (±30s across all selected services),
  "Correlate to alert" (jumps to M7 with the time window set).
- Right (collapsible): correlation pane — when opened from an alert,
  shows the alert's source node, the alert time, and the log lines from
  all related services in a ±2 min window, so a backend crash and the
  gateway's upstream errors read as one incident.
- Bottom: Export (filtered view to file), Clear view (view only, never
  deletes logs), log retention setting link.

Primary interactions: filter/search/follow; line-level actions;
alert-driven correlation; export.

---

## M7 — Alerts & Rules

Purpose: active alert triage and user-defined threshold rules with log
correlation. Pete runs long unattended sessions; this page is how the
machine asks for attention.

Layout, top to bottom:
- Active alerts table: severity, fired-at time, source node (clickable →
  selects the node on the map), message ("coder-30b queue depth > 8 for
  60s"), state (firing / acknowledged / resolved), actions: Acknowledge,
  Open node, View correlated logs (→ M6 correlation pane).
- Rules editor table: rule name, metric dropdown (backend_down,
  gpu_vram_pct, gpu_temp_c, queue_depth, ttft_p99_ms, tok_s_drop_pct,
  download_failed, policy_deny_rate), threshold, evaluation window,
  severity, notify (tray / banner / none), enabled toggle. "Add rule",
  "Test rule" (evaluates against the last 5 minutes of metrics and
  reports would-have-fired/would-not).
- History tab: resolved alerts with duration and the rule that fired;
  "Mute rule for 1h" available from any row.

Primary interactions: acknowledge/mute; rule CRUD; test-against-history;
alert → node → logs round-trip in two clicks.

---

## M8 — Settings

Purpose: all configuration in one page-based surface, separate from the
monitoring views (brief pattern 6). No modal wizards for routine edits.

Layout, left to right:
- Left section nav: Backends, Gateway (LiteLLM), GPU placement,
  Model storage, MXC policies, VS Code & extensions, MCP servers,
  Windows ML backend, Updates, Appearance & advanced.
- Center: the selected section as a validated form.
  - Backends: global default server flags + per-backend override table
    (brief pattern 7: typed fields, requires-restart badges).
  - Gateway: LiteLLM config (model list mapping to backends, ports,
    router strategy), validated.
  - GPU placement: same editor as M5's placement planner (shared
    component, not a copy).
  - MXC policies: full policy editor (rule list, allow/deny, scope,
    tool patterns) with dry-run tester; shows the Rust SDK version
    enforcing them.
  - VS Code & extensions: installed versions, reinstall/repair, per-
    extension backend endpoint settings (which gateway URL Continue.dev
    and Cline point at).
  - MCP servers: per-server config status, start/stop, config file
    editor with schema validation, test-connection button.
  - Windows ML backend: registration status, endpoint, enable/disable
    as a LiteLLM upstream.
  - Updates: component version table (llama.cpp build, LiteLLM, drivers,
    app itself) with Check/Update actions; update channel selector.
- Header actions: "Validate all" (reports every problem in one list),
  Apply (shows the exact set of services that will restart and asks for
  confirmation), Export / Import config (JSON).

Primary interactions: section navigation; validated editing with
restart-impact preview; config export/import; per-component updates.

---

# PART 2 — DESIGN DOC

## Goals

1. Make the full stack visible: 4 llama-server backends, the LiteLLM
   gateway, 3 GPUs, loaded models, MCP servers, and MXC policy boundaries
   — each with its own status, metrics, logs, and actions.
2. Make capacity legible: VRAM headroom per GPU, queue depth per backend,
   token throughput per edge, so placement and tuning decisions are made
   from numbers, not guesses.
3. Make setup a guided, resumable bring-up that ends on the same map the
   operator will monitor — no dead-end empty dashboard on first run.
4. Make failure loud and localizable: every action reports its outcome,
   alerts name their source node, and logs correlate to the alert in two
   clicks. No silent failures, no stale listings.
5. Keep routine configuration in persistent, validated surfaces — never in
   modal wizards — with restart impact shown before apply.

Non-goals: chat (Continue.dev/Cline already cover it; the Test tab is a
diagnostic utility), multi-user/RBAC, remote fleet management, automatic
model recommendations.

## Layout system

One window, four persistent regions (M2 is the template; all console
pages share the chrome):

- Header bar (top, ~48px): title, machine identity, global health rollup,
  aggregate throughput, VRAM headroom, alert bell, clock. The rollup is
  computed from node states — never a separate "server running" flag.
- Nav rail (left, ~200px, icon + label): the eight pages. Order matches
  operational frequency: Map, Backends, GPUs, Models, Logs, Alerts,
  Policies, Settings.
- Content area (center): the page. The Service Map page replaces this
  region's normal layout with the graph canvas + right inspector.
- Event drawer (bottom, collapsible): ticker strip when collapsed
  (latest alert or operation, alert count badge); expands to the full
  event list and operation log with per-operation Abort. Operations
  (downloads, installs, restarts) always stream here regardless of which
  page is open.

Detail pages (M3 GPU, M4 backend) share a second template: header with
identity + status + primary actions, stat tile row, then tabs or
sections. Breadcrumb: Map > Backends > coder-30b.

## Navigation model

Graph-first with page fallback:

- The Service Map is the home view and the primary navigation surface.
  Click selects (inspector), double-click opens the detail page, alert
  click selects the source node.
- The nav rail reaches every page directly; the rail also carries status
  dots (e.g. Alerts shows the firing count, GPUs shows the worst GPU
  state) so navigation itself is informative.
- Detail pages link laterally: GPU attribution row → backend page;
  backend config → placement planner; alert → node → correlated logs.
  Every lateral jump preserves a back path via breadcrumb.
- A compact tree view (gateway → backends → models; hardware → GPUs →
  processes) is available as a toggle on the map for keyboard/screen-
  reader navigation and for operators who prefer it — secondary, not
  primary.

Selection is global: selecting a backend on the map, in the tree, or in
the Backends list selects the same object everywhere, and the inspector
follows.

## Information hierarchy

1. Health: is anything down or degraded (header rollup, node colors,
   alerts page).
2. Capacity: VRAM headroom, queue depth, throughput — the numbers that
   decide placement and tuning (stat tiles, edge thickness, budget bars).
3. Activity: request rates, recent events, operation progress (drawer,
   event lists).
4. Configuration: only on drill-in (detail tabs, Settings). Current
   values stay visible while editing; nothing is hidden in a modal.

Stale data is labeled: any metric older than its poll interval shows a
"stale" marker; unreachable backends render as error nodes with the last
error, never as empty panels.

## Visual language

Declarative, dense, NOC-register:

- Dark theme default; light theme supported but not the priority.
- Status is carried by dots and bars, never by illustration: green / amber
  / red / blue / grey, consistent across map, rail, tables, and drawer.
- Monospace numerals everywhere; tabular figures in tables; sparklines
  are 60-second rolling windows with explicit "N/A" for unreadable
  fields.
- Graph edges: thickness proportional to token throughput, color follows
  the worse endpoint; tensor-split percentages on backend→GPU edges;
  req/s on client edges. No decorative animation — motion only for
  state changes and progress.
- Density target: Task Manager / Grafana, not a consumer app. Tables
  over cards; one screen should show all 4 backends, 3 GPUs, and the
  gateway without scrolling on a 1440p display.
- Titles are declarative ("Service Map", "GPU Detail", "Alert Rules").
  No taglines, no marketing prose, no metaphors in labels. Design writing
  describes.

---

# PART 3 — IMPLEMENTATION PLAN

## Architecture (stack-independent)

```
UI layer (HTML/CSS/JS)            Rust core (single binary)
─────────────────────             ─────────────────────────
Service Map canvas                Supervisor: spawn/healthcheck/
Detail pages, tables              restart llama-server + LiteLLM
Config forms (validated)   ←IPC→  Telemetry: NVML poll, VRAM attribution
Logs viewer (streaming)    cmds/  Downloader: HF downloads, resume, SHA
Alert rules editor         events Config: schema, validation, migrate
Setup wizard stages               Policy engine: MXC Rust SDK
                                  GGUF metadata parsing
```

The UI never touches the OS directly. All system integration — process
management, GPU telemetry, downloads, policy enforcement — lives in Rust
and is exposed to the UI over Tauri's IPC (commands for request/response,
events for the 1 Hz telemetry tick and log/operation streams). This
boundary is what makes the Rust constraint productive rather than
cosmetic.

## Stack evaluation (Tauri primary, weighed fairly)

Criteria: Rust integration, installer/distribution, UI capability for the
topology-graph approach, Windows desktop integration, maturity, build
complexity.

### Tauri 2.x (Rust backend + WebView2 UI) — primary candidate

For:
- The Rust constraint is fully leveraged: supervisor, NVML telemetry,
  downloader, config, and the MXC Rust SDK all live in one Rust binary
  with no FFI boundary. llama-cpp-2 / llama-cpp-rs bindings are
  available for GGUF metadata parsing (model params, quant, tensor
  layout) without shelling out — usable selectively (see below).
- Distribution fits: tauri-bundler produces NSIS/MSI installers from CI
  (GitHub Actions windows-latest) with no runtime dependency beyond
  WebView2, which ships with Windows 11. Small bundle (single-digit MB
  plus the app's own assets) vs ~150 MB for Electron.
- The UI layer is HTML/CSS/JS in the system webview — the correct
  technology for this design's hardest UI problem, the topology graph:
  SVG/Canvas rendering, pan/zoom, and layered layout are standard,
  well-understood web techniques. A native widget toolkit would make the
  graph harder, not easier.
- Tray icon, autostart, native notifications, and multi-window support
  cover the drawer/ticker and alerting needs.

Against (honest):
- WebView2 is a real dependency: pinned versions can lag Chromium
  features, and enterprise machines occasionally have it removed. On
  Pete's own machine this is a non-issue; for anyone else it is an
  installer prerequisite to handle.
- The JS↔Rust IPC boundary is a second codebase to maintain. Streaming
  1 Hz telemetry plus log tails over Tauri events is well within its
  capacity at this scale (<40 nodes), but it is serialization overhead
  that a pure-Rust UI would not have, and debugging spans two runtimes.
- Tauri's multi-window and menu APIs are less mature than Electron's;
  complex window management (detached GPU page on a second monitor, for
  example) would need verification, not assumption.
- A frontend framework still has to be chosen (Svelte or Solid recommended
  for small bundle and fine-grained reactivity; React is viable but
  heavier). That choice carries its own learning curve.
- Rust async (tokio) plus Tauri's event loop plus the webview thread is a
  genuinely tricky concurrency model to get right the first time —
  plan for it in M0, not later.

### Electron — evaluated, not recommended

For: the most mature desktop-web stack; full Chromium (no WebView2
variance); enormous ecosystem for charts/graphs; multi-window is
battle-tested. Against: ~150 MB bundle for an app whose backend must be
Rust anyway — which forces either a Rust sidecar binary (two processes,
two installers, IPC over localhost) or a Node backend that duplicates
the Rust work. The Rust constraint makes Electron pay the bundle cost
while getting none of the integration benefit. Rejected on weight and
on the sidecar complexity, not on capability.

### .NET WinUI 3 — evaluated, not recommended

For: the most native Windows option — best OS integration, real native
controls, straightforward P/Invoke for NVML and job objects, MSIX
distribution, and Pete develops on Windows so the toolchain is familiar.
Against: it contradicts the Rust constraint. The MXC Rust SDK, the
supervisor, and telemetry would live in a Rust DLL behind a C ABI,
which reintroduces exactly the FFI boundary the constraint was meant to
avoid — with worse debugging than Tauri's IPC. And WinUI 3 has no good
story for the topology graph: it would be custom Win2D canvas code,
hand-rolled hit-testing, and hand-rolled layout — the graph is this
design's centerpiece, and building it natively is the highest-risk UI
work on the table. Rejected because it fights both the constraint and
the design.

### Rust service + locally-served web dashboard — fallback candidate

For: keeps 100% of the Rust integration benefit; the UI is the same
HTML/CSS/JS as Tauri's frontend, served from the Rust binary (e.g. axum)
on localhost — zero WebView2 dependency, accessible from other devices
on the LAN, trivial to debug in a real browser. Against: no real desktop
integration (tray icon, autostart, and notifications become separate
small problems — a tray helper or a scheduled task), the user must keep
a browser tab open, and localhost HTTP is a larger security surface to
get right (bind to loopback, token auth). This is the fallback if
WebView2 proves problematic: structure the Rust core so the Tauri IPC
layer and an HTTP layer are both thin adapters over the same core API,
and the fallback costs weeks, not a rewrite.

Verdict: Tauri primary; keep the core/IPC boundary clean so the
service-plus-web-dashboard fallback stays cheap. Electron and WinUI 3
are out for reasons of weight and constraint-fit, not fashion.

## Rust-specific integration points

- MXC sandbox policies: MXC ships a Rust SDK — link it directly into
  the core binary. Policy evaluation happens in-process (no subprocess,
  no HTTP hop); the Policies page and the wizard's dry-run call the same
  evaluation function the enforcement path uses, so dry-run results are
  trustworthy by construction.
- llama.cpp bindings: use selectively. For M0–M2, GGUF metadata (params,
  quant, tensor shapes for the fit-check) is better served by a pure-Rust
  `gguf` crate than by full llama-cpp-2 bindings — the bindings pull in
  bindgen and CUDA linkage at build time, which complicates CI
  significantly. Adopt llama-cpp-2 only if a feature needs the actual
  inference engine in-process (e.g. an in-app tokenizer for exact context
  estimation); otherwise keep llama.cpp as supervised subprocesses.
- GPU telemetry: `nvml-wrapper` against the driver-shipped nvml.dll
  (Tesla P100 is NVML-supported on Windows). Wrap it: every metric read
  goes through a trait with an nvidia-smi-parsing fallback, so a missing
  or broken NVML still yields a degraded-but-honest UI ("N/A", stale
  markers) instead of a crash.
- Process supervision: Windows job objects so killing a backend kills
  its whole process tree; health checks against each backend's
  `/health` endpoint; restart policies per backend (never / on-failure /
  always) configurable in M4.
- Installer: tauri-bundler NSIS target from GitHub Actions; versioned
  updates via the built-in updater against a release feed (M6 Updates
  page drives it).

## Topology-graph rendering implications (webview UI)

- Layout is deterministic and layered (clients → gateway → backends →
  GPUs → models; MCP/policy side columns), computed in TypeScript — no
  force simulation, no per-frame physics. Node positions are persisted
  in config so user adjustments survive restarts.
- Rendering: SVG for nodes and edges (node count stays under ~60, DOM
  is fine), `<canvas>` for the per-node sparklines, edge width mapped
  from tok/s each tick. Pan/zoom via viewBox transform; updates at 1 Hz
  on data change, not 60 fps — WebView2 handles this comfortably with
  GPU-accelerated canvas.
- Keep all layout code in the frontend; Rust ships node/edge state
  (health, metrics) over events. Do not compute layout in Rust — it
  couples the core to presentation and buys nothing at this node count.
- Accessibility: the graph is keyboard-navigable via the compact tree
  toggle (same selection model), and every node exposes its metrics as
  text in the inspector.

## Milestones and build order

- M0 — Rust core skeleton: config schema + validation, supervisor
  (spawn/healthcheck/job-object teardown), NVML telemetry trait with
  sRGB fallback, log tailer, IPC surface (commands + events). Exit
  criteria: a headless core can start/stop the 4 backends and report
  per-GPU VRAM from a CLI harness.
- M1 — Tauri shell + read-only console: Service Map rendering live data,
  header rollup, event drawer, nav rail. Exit: map reflects real backend
  and GPU state; stale/unreachable states render honestly.
- M2 — Backend management: M4 detail pages (start/stop/restart/reload,
  per-backend logs, test utility), Backends list, config editing with
  restart-impact preview.
- M3 — Model management: M5 library, download queue with resume +
  SHA verify, fit-check against live VRAM, placement planner.
- M4 — Setup wizard: M1's six stages built on M0–M3 components
  (prereq checks, component installer, model stage, service definitions,
  policy dry-run, bring-up with the live map). Exit: clean-machine run
  provisions the full stack unattended except driver installs.
- M5 — Alerting: M7 rules engine (threshold evaluation in Rust, rules
  stored in config), active-alert triage, M6 log correlation pane.
- M6 — Polish and upkeep: M8 remaining sections (MCP, WinML, VS Code
  extensions), Updates page + auto-updater wiring, tray behavior,
  first-run vs console routing, performance pass on the graph.

Build order rationale: the core/IPC contract (M0) and the read-only map
(M1) de-risk the two hardest unknowns (Windows process supervision and
graph rendering) before any installer or alerting work depends on them.
The wizard (M4) comes after the pieces it orchestrates exist — it is an
orchestrator, not a foundation.

---

# PART 4 — TEST PLAN

## Levels

- Unit: pure logic, no OS, no GPU. Fast, run in CI on every commit.
- Integration: Rust core against fakes (stub NVML, fake llama-server
  HTTP stub, local file fixtures). Run in CI on Windows runners.
- End-to-end: Tauri app driven via tauri-driver/WebDriver on a Windows
  VM with real NVIDIA drivers; exercises the UI against the real core.
  Nightly, not per-commit (slow, needs a GPU runner).
- Manual: checklist-driven on Pete's Precision 7865 for the things fakes
  cannot cover (tensor-split placement, real driver states, installer
  UX). Per milestone.

## What gets tested per component, and how

| Component | Unit | Integration | E2E / Manual |
|---|---|---|---|
| Config schema + validation | Malformed flags rejected; tensor-split sums to 1.0; port clashes detected | Config migrate v1→v2 on fixture files | Manual: corrupt config file → app reports the exact error, never silently defaults |
| Supervisor | Restart-policy state machine (never/on-failure/always) | Fake backend stub that crashes on demand; job-object teardown kills the tree; health-check timeout → error state | Manual: kill -9 a llama-server → UI shows error node with log excerpt within 2 polls |
| Telemetry (NVML) | Metric parsing, stale-marker logic | Stub NVML returning fixed values; nvidia-smi fallback path with recorded output | E2E: real driver; Manual: pull a GPU's data cable scenario N/A → "N/A" shown, never 0 |
| VRAM fit-check | Placement math on fixtures (fits / overcommit cases) | Fit-check against stub telemetry with fragmented VRAM | Manual: load the real 4-model plan; planner numbers match nvidia-smi within 5% |
| Downloader | URL parsing, resume-offset math | Local HTTP server with throttling + mid-download kill → resume works; SHA mismatch → failed state | Manual: real HF download of one GGUF; pause/resume/cancel each verified |
| Setup wizard | Stage-gating logic (Next blocked until valid) | Full wizard against fakes in order; resume-after-exit restores stage state | Manual: clean VM run; every Fix button exercised; bring-up map fills in |
| Service Map rendering | Layered layout algorithm on fixture graphs (no overlaps, stable positions) | — | E2E: nodes selectable, double-click opens detail, alert click selects source node; Manual: pan/zoom at 1440p, 1 Hz updates, no jank |
| Alert rules | Threshold/window evaluation on synthetic metric series; test-against-history | Rule fires on stub telemetry crossing threshold; ack/mute state transitions | Manual: real VRAM pressure → tray notification; correlated logs open ±2 min |
| Log streaming | Line parsing, severity classification | 10k-line burst → UI stays responsive, follow/pause correct | E2E: filter/search across merged streams |
| MXC policy dry-run | Rule matching on fixture policies | SDK evaluation agrees with dry-run UI on the same inputs | Manual: deny-by-default blocks a test tool call; UI names the matching rule |
| IPC contract | Serialization round-trips for all commands/events | Frontend type-checks against generated bindings; breaking change fails CI | E2E: every page loads against the real core with zero console errors |
| Installer/updater | — | NSIS build in CI; install/uninstall leaves no residue; update feed parsing | Manual: install on clean Win11, WebView2-missing path shows the prerequisite |

## Cross-cutting test concerns

- Stale-state honesty: integration tests assert that unreachable
  backends render as error nodes (with last error), never as empty
  panels; unit tests assert the stale marker appears past the poll
  interval.
- Action feedback: every E2E flow that mutates state (install, restart,
  load model) asserts the drawer shows queued → running → done/failed
  with the log line attached.
- Performance: telemetry tick (1 Hz, full node set) serialized under
  5 ms; map re-render under 16 ms on the target machine; log view holds
  50k lines without dropping follow.
- The anti-pattern checklist (from the brief) is a manual gate per
  milestone: no chat-primary surface, no single global toggle, no modal
  routine config, no decorative metaphors, no silent/stale state.
