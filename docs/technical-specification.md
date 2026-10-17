# Technical Specification — Local LLM Service Manager

Status: draft for implementation estimation. UI design approved Oct 8, 2026
(`files/ui-design-brainstorm/design-b-hybrid.md`). No implementation begins before
Pete approves the design; this spec is the buildable record of what was approved.

Acronyms used once here and then bare: MXC (Microsoft Execution Containers),
NVML (NVIDIA Management Library), TTFT (time to first token), PSEC
(process-container execution contract), GGUF (GPT-Generated Unified Format).

---

## 1. Overview

### 1.1 What it is

A Windows desktop application (Tauri 2, Rust backend) that installs, configures,
supervises, and monitors a local LLM serving stack on a single workstation. It is a
service manager, not a chat client: it owns the lifecycle of `llama-server`
processes, a LiteLLM gateway, model files, GPU telemetry, MCP server configuration,
VS Code extension wiring, Windows ML secondary-backend registration, and MXC
sandbox policy for agent tool execution. One data layer, three presentational modes
per data window (Appliance | Console | Topology) per the approved hybrid design.

### 1.2 Target machine

Dell Precision 7865 Tower, Threadripper PRO 5945WX (no NPU), 96 GB DDR4 ECC
(planned), RTX A4000 16 GB + 2× Tesla P100 16 GB, Windows 11 Pro 24H2+. Single-user,
single-machine. All inference stays on the host; MXC sandboxes only agent tool
execution.

### 1.3 Non-goals (v1)

- No chat/conversation surface. The only prompt box is the backend Test-request
  diagnostic tab (raw JSON out, no history).
- No remote/multi-machine management. A read-only status page is a stretch goal.
- No embedded inference via llama.cpp Rust bindings in v1; `llama-server`
  subprocesses are the serving path for flag parity with upstream releases.
- No Linux/macOS port. Windows 11 only by design.
- No first-party MXC integration for Continue.dev/Cline in v1 (no published
  integration exists; wrapping their tool execution is integration work, tracked as
  a risk, not a v1 deliverable). The app manages MXC policy, probes, Learning-mode
  reports, and credential injection for its own sandboxed invocations.

---

## 2. Architecture

### 2.1 Stack

- **Shell:** Tauri 2. Rust backend; WebView2 frontend. Frontend framework TBD
  (Svelte 5 candidate); the Tauri command/event boundary is framework-independent
  and must stay so.
- **Serving:** standalone `llama-server` (CUDA build) subprocesses, one per backend.
- **Gateway:** LiteLLM in an app-managed Python environment (uv), config generated
  from app state.
- **Telemetry:** `nvidia-smi --query --xml` parser as the active v1 source, behind a
  `TelemetrySource` trait seam; NVML runtime loading (`nvml.dll`) is a
  documented follow-up (see §5). The trait boundary means no caller changes
  when NVML wiring lands.
- **Sandboxing:** `mxc-sdk` Rust crate; `wxc-exec.exe` CLI fallback.
- **Installer:** `tauri-bundler` NSIS target; MSI retained as an option.
  `tauri-plugin-updater` for app self-update.

### 2.2 Process model

The app supervises child processes; it never embeds inference.

- One `llama-server` process group per backend (4 planned). The supervisor owns
  spawn args, health probing, graceful drain (SIGTERM-equivalent, 30 s timeout,
  then kill), crash backoff (exponential, capped), and stdout/stderr capture into
  the log bus.
- One LiteLLM gateway process. Config hot-reload where supported; restart otherwise.
- Optional `WinMLServer.exe` process for the secondary Windows ML target (single
  small model only; see §6.5).
- MCP servers (GitHub, Google Drive) run as configured local processes under the
  app's supervision when set to local mode; remote mode is configuration only.
- The webview never spawns processes, touches the filesystem, or holds secrets.
  All privileged operations are Tauri commands executed by the Rust backend.

### 2.3 Component diagram

```
┌─────────────────────────────────────────────────────────────────┐
│ WebView2 UI (Appliance | Console | Topology presenters)          │
│  Tauri commands (request/response)  ◄──►  Tauri events (stream)  │
└──────────────────────────────┬──────────────────────────────────┘
                               │
┌──────────────────────────────▼──────────────────────────────────┐
│ Rust backend                                                     │
│  ┌────────────┐ ┌───────────┐ ┌──────────┐ ┌──────────────────┐   │
│  │ Supervisor │ │ Telemetry │ │ Downloads│ │ Config store     │   │
│  │ (llama-    │ │ (nvidia-  │ │ (GGUF,   │ │ (serde-typed,    │   │
│  │  server ×4,│ │ nvidia-smi│ │ builds)  │ │ versioned)       │   │
│  │ LiteLLM,   │ │ poll loop │ │          │ │                  │   │
│  │ MCP, WinML)│ │           │ │          │ │                  │   │
│  └────────────┘ └───────────┘ └──────────┘ └──────────────────┘   │
│  ┌────────────┐ ┌───────────┐ ┌────────────────────────────────┐  │
│  │ MXC client │ │ WinML     │ │ Log bus (per-scope ring       │  │
│  │ (policy,   │ │ probe +   │ │ buffers → UI events)           │  │
│  │ spawn,     │ │ spawn     │ │                                │  │
│  │ reports)   │ │           │ │                                │  │
│  └────────────┘ └───────────┘ └────────────────────────────────┘  │
└──────────────────────────────┬──────────────────────────────────┘
                               │ spawns / probes / reads
┌──────────────────────────────▼──────────────────────────────────┐
│ OS / hardware                                                    │
│  llama-server :8081-:8084 │ LiteLLM :4000 │ WinMLServer :8090     │
│  RTX A4000 + 2× Tesla P100 (nvidia-smi) │ Windows Credential Manager │
│  MXC processcontainer runtime │ VS Code (`code` CLI)            │
└─────────────────────────────────────────────────────────────────┘
```

Clients (Continue.dev/Cline in VS Code, MXC-sandboxed tools) reach the stack only
through LiteLLM `:4000`. Sandboxed tools may additionally reach `127.0.0.1` ports
per MXC policy.

### 2.4 Data flow

- Telemetry loop (Rust, 1 s default): `nvidia-smi` poll → in-memory snapshot store →
  `telemetry-tick` event to UI (full snapshot at 1 s; UI downsamples for sparklines).
- Log bus: every supervised process gets a scoped ring buffer (default 10k lines);
  `log-line` events stream to UI; UI filters locally.
- Commands are synchronous request/response except long operations (installs,
  downloads, restarts), which return a task id and stream `task-progress` events;
  the global task drawer renders them.

---

## 3. System requirements

### 3.1 OS and runtime

| Requirement | Minimum | Verified by |
|---|---|---|
| Windows 11 | 24H2, build ≥ 26100.9278 | Wizard prerequisites (MXC processcontainer floor) |
| NVIDIA driver | Version supporting the pinned llama.cpp CUDA build (see §12) | `nvidia-smi` driver query vs pinned minimum |
| CUDA | CUDA-enabled llama.cpp build; toolkit install optional (build ships its own runtime where possible) | Wizard check: `nvidia-smi` present, driver ≥ minimum |
| WebView2 | In-box on Win11 | Wizard check; bootstrapper fallback |
| Disk | ≥ 120 GB free (4 GGUFs ~60–90 GB + builds + headroom) | Wizard disk-space check |
| VS Code | Installed, `code` CLI on PATH | Wizard check |
| Network | Required for setup (downloads); optional at runtime except OpenRouter tier | Wizard notes offline limits |

### 3.2 Hardware assumptions

RTX A4000 16 GB (primary, planner) + 2× Tesla P100 16 GB (coder pool). The app
must not assume exactly this: GPU inventory is discovered at runtime (v1:
`nvidia-smi` XML; the NVML path is behind the `TelemetrySource` trait seam)
and
all placement math is computed from discovered VRAM. The P100s are Pascal
(sm_60); the compatibility matrix is verified, not assumed — see §3.4. The
wizard fails closed if the pinned build's minimum compute capability exceeds a
discovered GPU.

### 3.3 Wizard verification matrix

Every prerequisite row reports detected value vs required value, pass/fail, and a
remediation action or link on fail. Rows: Windows build, NVIDIA driver version,
CUDA/driver compatibility, WebView2 runtime, VS Code + `code` CLI, disk space,
port availability (:4000, :8081–:8084, :8090), MXC PSEC version +
`probes.baseContainerSupportsIngressHostLoopbackAllow` result (recorded verbatim;
drives Model 1 vs Model 2 loopback choice in §11.2).

### 3.4 Verified GPU / driver / CUDA matrix (researched Oct 8, 2026)

The P100 compatibility question is settled — this is a pinned constraint, not a risk.

| Layer | Verified status |
|---|---|
| Windows 11 driver for P100 | Current R580-branch drivers still carry Pascal. Studio Driver 581.57 (Oct 2025) lists the Tesla P100-PCIE-16GB for Windows 10/11【3515469153690274804†L116-L118】; driver 580.178.04 confirmed running P100s in production【100099123665993478†L18-L20】; NVIDIA datacenter driver 537.13 also lists the P100【3515469153690274804†L285-L287】. |
| CUDA toolkit ceiling for Pascal | CUDA 12.x (through 12.9) is the last series that compiles for sm_60. CUDA 13.0 removed offline compilation and library support for Maxwell/Pascal/Volta — per NVIDIA's own 13.0 release notes, support for these architectures "is considered feature-complete" and "newer toolkits will be unable to target these architectures"【100099123665993478†L269-L273】. The driver still *runs* Pascal fine; only the compiler moved on【100099123665993478†L26-L28】. |
| Driver/runtime compat | Binaries built against CUDA 12.x run on R580 drivers via CUDA minor-version compatibility【5652994368314682905†L150-L153】 — no conflict between "latest driver" and "Pascal support". |
| llama.cpp build to pin | Upstream `bin-win-cuda-12.4-x64` release assets: the cuda-12.4 builds ship the Pascal targets (sm_50/sm_61 PTX floors)【5652994368314682905†L248-L252】. **Never pin a cuda-13.x build** — sm_60/sm_61/sm_70 were dropped from CUDA 13 builds【5652994368314682905†L24-L30】. |
| A4000 alongside | The A4000 is Ampere (sm_86), fully supported under CUDA 12.x — one pinned cuda-12.4 build serves all three GPUs. |
| Fallbacks | If upstream ever drops the cuda-12.4 assets: community dual-track builds (e.g. kodx/llama.cpp-cuda, unsloth `cuda12-legacy` bundle) publish explicit cuda-12 legacy binaries covering sm_60【5652994368314682905†L92-L95】. Last resort: source build with CUDA 12.x toolkit and `-DCMAKE_CUDA_ARCHITECTURES=60` (prebuilt preferred — Windows nvcc/CMake toolchain pairings are fragile). |

Wizard consequences: the driver check requires R550+ (Windows driver >= 551.61,
per NVIDIA's CUDA 12.4 release notes) on any machine that will run the pinned
cuda-12.4 build; the build check pins cuda-12.4 assets by SHA-256 and rejects
cuda-13.x assets on a
Pascal-bearing machine; the smoke test (§4.1 step 10) runs on both the A4000 and a
P100 backend so a bad pin is caught at install time, not at first use.

---

## 4. Functional requirements

Global UI rules (from the approved design): status dot + text label always paired;
every live number carries a relative age ("3s ago"); data older than 10 s without a
successful poll is labeled STALE, never silently shown; no color-only meaning; one
canonical surface per object with cross-links.

### 4.1 Setup wizard

The wizard is a path, not a place: its stages remain reachable post-setup under
Settings → Updates & Diagnostics as re-runnable checks.

**Step model.** Each step is a state machine: `queued → running → done | failed`,
with `skipped` and `manual` (user overrode) as terminal states. Failed steps expose
the last 20 log lines inline plus Retry, Open full log, and Skip with note. A step
cannot run until its declared prerequisites are `done` or `manual` (blocked rows
show "waiting on: <step>"). Steps run automatically in order once the wizard
starts; the user may also run an individual step.

**Progress.** Overall completion bar: `done / total`, percentage, elapsed time and
ETA. Per-step row: checkbox state (checked / animated running / queued / failed /
skipped), duration, and for downloads a throughput sparkline.

**Step list (10 steps).**

| # | Step | What it does |
|---|---|---|
| 1 | Detect hardware/drivers | §3.3 verification matrix; records GPU inventory, driver, PSEC probe |
| 2 | Install llama.cpp CUDA build | Download pinned build, verify checksum, install to app dir |
| 3 | Configure tensor-split | Compute per-model splits from discovered VRAM; write backend flag sets; validate sums to 1.0 |
| 4 | Download models + checksum verify | 4 curated GGUFs (see §4.6), pause/resume/cancel, SHA-256 verify before "installed" |
| 5 | Start LiteLLM gateway | Bootstrap managed Python env (uv), generate `config.yaml`, start on :4000, health-check |
| 6 | Install VS Code extensions | `code --install-extension` for Continue.dev + Cline (pinned versions); write config pointing at :4000 |
| 7 | Configure MCP servers | Install/register GitHub + Google Drive MCP servers; credential presence check (values never shown) |
| 8 | Register Windows ML backend | Probe WinMLServer availability; register one small model (tool-runner class) as secondary LiteLLM backend; record capability limits verbatim |
| 9 | Apply MXC policies | Probe PSEC/ingress; write default policy JSON; mode selector (Learning recommended first); run policy self-test (harmless sandboxed command, activity report shown inline) |
| 10 | Smoke-test inference | Tagged test request per backend through the gateway; assert non-empty completion + sane TTFT; record baseline latencies |

**Transparency.** "Show commands" on every automated step expands the exact
command(s), URLs, checksums, and file paths with Copy buttons. Manual override per
row: "Mark as manually installed" (path/version fields) and "Use custom…" (custom
build URL, local model file). Overrides are first-class state labeled `manual`.

**Re-run audit (on demand).** Available from the wizard finish screen, Dashboard,
and Settings. Re-executes all *check-type* work without reinstalling: re-verify
prerequisites, re-checksum installed models and the llama.cpp build, re-probe
backend health, re-validate ports/config, re-probe WinML + MXC. Reports per-step
pass/fail in the same checklist UI. Install-type steps are offered, not forced, on
audit failure ("model checksum mismatch → re-download?").

### 4.2 Dashboard

Health-at-a-glance. View selector (Appliance | Console | Topology) in the toolbar;
Appliance default; choice persists per window.

- **Appliance:** 5 service cards (LiteLLM :4000 + 4 backends) with status, port,
  loaded model, uptime, VRAM held, req/s + tok/s (1 m avg), per-card
  restart/stop/start and "logs" jump. GPU strip: 3 compact cards (util %, VRAM
  bar, temp, power), busiest-engine metric shown. Model placement mini-map
  (tensor-split stacked bars). Recent activity (last 8 events, cross-linked).
  Header shows the single most severe active issue, clickable to the issues list.
- **Console:** dense tables — service table with sparklines, GPU table with bars,
  event lines. Same numbers, smaller type, keyboard-navigable.
- **Topology:** mini cluster node map (clients → LiteLLM → backends → GPUs →
  models, MXC boundary, throughput edge labels); node click deep-links to the
  Service Map window with the node highlighted.

### 4.3 Service Map

First-class window (nav item), C-02 re-skinned in the B language. Full cluster
topology: client nodes (Continue.dev, Cline, sandboxed tools), LiteLLM :4000,
4 backends, 3 GPUs with utilization rings, loaded models, MCP servers, MXC dashed
boundary. Edge labels are throughput numbers only. Amber/red edges for degraded or
failed links (e.g. degraded MCP link). Severity-dot event timeline + cluster
summary inspector. Node click → that object's canonical window.

### 4.4 GPUs

- **Appliance:** 3 full GPU cards — name, PCI bus/device/function, driver, status;
  metric grid (util % + 60 s sparkline, VRAM used/total + bar, memory util %,
  temp with peak mark, power/limit, clocks, fan); per-process VRAM table (process,
  PID, MiB, backend association — unassociated consumers shown); models-on-GPU
  list (layers n/total, split share %, KV cache MB). Placement map: per-model
  stacked bars across GPUs + RAM offload, configured vs observed `--tensor-split`
  with amber flag on mismatch. Poll interval selector (1 s/5 s/off), reset peak
  marks, snapshot to JSON.
- **Console:** same data as dense tables.
- **Placement editor** (expert disclosure): per-model tensor-split sliders with
  live projected-VRAM-per-GPU readout and fit validation; Apply runs a guided
  rolling restart (drain 30 s, restart one backend at a time, verify health after
  each). Current live values stay visible beside edited values until applied.

### 4.5 Backend detail

One canonical surface per `llama-server` process. Tabs: Summary | Configuration |
Logs | Test request.

- **Summary:** status + uptime + PID + port; resource bars (VRAM per GPU, RAM,
  context usage n_ctx used/allocated); request stats (totals, tokens in/out, avg
  TTFT, tok/s, error rate 1 h/24 h); health-check history (last 20 probes);
  read-only copyable launch command.
- **Configuration:** structured form of server flags — model path (picker),
  n_ctx, n_batch, tensor-split, split-mode, parallel slots, KV quant, flash
  attention, port; validation (n_ctx ≤ model max, split sums to 1.0), "requires
  restart" badges; Save → "Apply & restart" or "Save for next restart". Global
  defaults shown greyed behind per-backend overrides. Every apply shows a diff
  summary first; failed apply rolls back to last-known-good.
- **Logs:** backend-scoped log stream (embedded viewer), level filter, jump-to-errors.
- **Test request:** prompt textarea, temperature/max-tokens overrides, Send →
  raw JSON + timing breakdown (TTFT, tok/s). Diagnostic tool only.
- View selector: Appliance (above) | Console (dense flag table + stats) |
  Graph (per-backend request-flow: LiteLLM → backend → GPU nodes with tok/s edge
  labels and queue-depth badge).

### 4.6 Models

Tabs: Catalog | Installed | Downloads.

- **Catalog:** the 4 curated GGUFs — Qwen3.8-27B (planner), Qwen3-Coder-30B
  (coder), Qwen2.5-Coder-7B (coder-fast), Qwen3-8B (tool-runner) — with params,
  quant, size, and a live fit-check column vs free VRAM (green fits / amber fits
  with offload / red does not fit, with the arithmetic shown: "needs ~16.8 GB:
  A4000 free 9.1 + P100-0 free 8.4 → fits with split 55/45"). Fit-check recomputes
  on VRAM-change events and explains changes ("P100-1 now holds coder-7b, 6.2 GB
  less free"). Row click → detail pane: role in plan, recommended placement,
  recommended flags, source URL + checksum.
- **Curated artifacts (v0.1.0).** All Q4_K_M from Qwen's official GGUF repos;
  the planner slot keeps the spec display name "Qwen3.8-27B" and ships pointed
  at the closest real artifact, Qwen3-32B (dense 32B-class):

  | Slot | Display name | Artifact repo | File |
  |---|---|---|---|
  | planner | Qwen3.8-27B | `Qwen/Qwen3-32B-GGUF` | `Qwen3-32B-Q4_K_M.gguf` |
  | coder | Qwen3-Coder-30B | `Qwen/Qwen3-Coder-30B-GGUF` | `Qwen3-Coder-30B-Q4_K_M.gguf` |
  | coder-fast | Qwen2.5-Coder-7B | `Qwen/Qwen2.5-Coder-7B-Instruct-GGUF` | `qwen2.5-coder-7b-instruct-q4_k_m.gguf` |
  | tool-runner | Qwen3-8B | `Qwen/Qwen3-8B-GGUF` | `Qwen3-8B-Q4_K_M.gguf` |

  Sizes in the catalog are Q4_K_M estimates for fit-check projection only.
  SHA-256 values ship as `PLACEHOLDER-pin-real-sha256-at-install-time`: the
  wizard confirms each URL and pins the real hash into the config at install
  time, and checksum verification fails closed on the placeholder — a model
  can never be marked "installed" against an unpinned hash.
- **Installed:** per-GGUF rows — size, quant, checksum-verified date, assigned
  backend(s); actions: Load into backend (picker), Verify checksum, Reveal in
  Explorer, Delete (guarded if a backend references the file).
- **Downloads:** queue with progress, speed, ETA, pause/resume/cancel; resume
  from byte offset where the server allows; checksum failure blocks "installed"
  and offers re-download.

### 4.7 Logs

Scope selector (All / Gateway / per-backend / Installer / App), level filter,
text search, Follow toggle, Export. Virtualized table: timestamp, scope tag, level,
message; error rows expandable to context. The bottom task drawer is persistent
app-wide and full-page here: every long operation as a row (operation, target,
progress, elapsed, Abort, "view log"). Auto-expands on new work, collapses to one
line on completion, never steals focus. "Copy as diagnostic bundle" zips recent
logs + config snapshot + GPU snapshot. View selector: Appliance | Console (dense
mono stream with level-distribution strip on top).

### 4.8 Gateway

LiteLLM :4000 status (version, config path, uptime), Restart, Edit config,
"Open LiteLLM UI" if enabled.

- **Model groups table:** `planner`, `coder-fast`, `coder`, `tool-runner` →
  member backends → routing strategy → fallback chain. Structured editor:
  member checkboxes, strategy dropdown, fallback drag-ordering, config-diff
  preview before apply; hot-reload where supported else restart offered.
- **Routing rules:** plain-language rules with LiteLLM config equivalents and
  source (curated vs override). "Test routing": tagged test request → shows
  chosen backend + strategy trace.
- **Recent requests:** last 100 ring buffer — timestamp, group, backend, tokens
  in/out, latency, status; failures expandable.
- **Health strip:** per-backend latency sparklines as seen by the gateway.
- View selector: Appliance | Topology (routing-flow overlay: rule → group →
  backend, strategy badges, latency edge labels, dashed fallback edges including
  OpenRouter cloud).

### 4.9 Settings / MXC

Left sub-nav: General | Backends | Models & placement | Gateway | Sandbox (MXC) |
Editor & MCP | Windows ML | Updates | Diagnostics.

- **General:** launch on startup, start minimized to tray, poll intervals, log
  retention, data directories, theme, notifications.
- **Backends:** global default flag template; auto-restart policy (on crash:
  restart up to N with backoff; on config change: manual/guided/auto).
- **Models & placement:** default placement per model, KV-cache defaults,
  download dir, checksum policy.
- **Sandbox (MXC):** policy editor as structured form over the JSON schema
  (filesystem read/write/denied, network egress rules, loopback toggles, UI);
  mode selector (Learning / Permissive / Enforcement) with current mode bannered
  on the Dashboard; guided "run Learning 7 days → review" flow that opens the
  activity report and proposes per-rule tightening (accept/reject); PSEC +
  ingress probe results shown verbatim; credential presence (never values) for
  injected secrets.
- **Editor & MCP:** extension inventory (installed vs pinned), MCP server rows
  (command, env, enabled), per-server connection test.
- **Windows ML:** secondary-target panel — WinMLServer registration status,
  registered model, /v1 endpoint, LiteLLM backend entry preview; banner with the
  researched limits (no tensor-split, coarse GPU selection, experimental) and a
  "re-check capabilities" probe button. Register/unregister; never primary UI.
- **Updates:** component inventory (llama.cpp build, LiteLLM, models, extensions,
  MXC runtime, GPU driver) — installed vs latest-known, per-row Update, Update
  all, per-component pin + auto-update toggles, last-checked timestamps.
- **Diagnostics:** full diagnostic runner (prereqs + backend health + port
  conflicts + disk + config validation) with pass/fail report and remediation
  actions; open data folder; export diagnostic bundle; app log level.
- Footer pattern on every settings page: unsaved-changes indicator, Discard /
  Apply, and explicit "Apply & restart affected services" naming the affected
  services. No modal wizards for routine config.

---

## 5. GPU telemetry

**Sources (v0.1.0).** Active: `nvidia-smi --query --xml` (device handles,
per-GPU metrics; per-process attribution is best-effort on this path).
Deferred: NVML runtime loading (`nvml.dll` via the `TelemetrySource` trait
seam in `manager-telemetry`) — Pascal-era NVML quirks on the P100s make the
`smi` path the reliable v1 choice; NVML lands in a hardening pass with no
caller changes required. Telemetry source per GPU is shown in the
UI ("NVML" / "nvidia-smi fallback").

**Poll loop (Rust).** Default 1 s cadence, user-selectable 1 s / 5 s / off.
Single poller thread; snapshot store is the single writer. On poll failure:
mark affected GPUs STALE after one missed tick; keep last-known values visible
with the STALE label; never synthesize.

**Metrics per GPU.**

| Metric | v1: nvidia-smi (active) | NVML (deferred) |
|---|---|---|
| Utilization % (graphics/compute) | yes | yes |
| VRAM used/total, memory util % | yes | yes |
| Temperature °C + peak mark | yes | yes |
| Power draw W / limit W | yes (draw; limit where exposed) | yes |
| Clocks (graphics/memory MHz) | yes | yes |
| Fan % | yes | yes |
| Per-process VRAM (PID, name, MiB) | `pmon`-style query (best-effort) | accounting stats |
| PCI bus/device/function, driver version | yes | yes |
| Busiest engine (for the "idle-looking card" rule) | limited — shown only when available | per-engine util |

**Derived state.** Free VRAM per GPU (drives fit-check), peak marks (resettable),
60 s ring buffers per metric for sparklines (UI downsamples), tensor-split
observed values (from per-process attribution vs configured flags).

**Performance budget.** One full poll of 3 GPUs < 100 ms; event-queue check in
tests: webview receives ≤ 1.2× ticks sent over 60 s under load (no backlog).

## 6. Backend management

### 6.1 Backend inventory (planned)

| Backend id | Model | Placement | Port |
|---|---|---|---|
| planner | Qwen3.8-27B | A4000 + P100-0 tensor-split (~60/40) | :8081 |
| coder | Qwen3-Coder-30B | P100 pool tensor-split | :8082 |
| coder-fast | Qwen2.5-Coder-7B | P100 pool | :8083 |
| tool-runner | Qwen3-8B | P100 pool (side-loaded) | :8084 |

Placement is computed from discovered VRAM at setup (§4.1 step 3) and editable via
the placement editor (§4.4); the table above is the default plan, not a constant.

### 6.2 Lifecycle

State machine per backend: `stopped → starting → running(healthy|degraded) →
stopping → stopped`, plus `failed` (crash) with backoff restart per policy.
Health: HTTP `/health` poll (default 5 s) + 3-strike flapping rule (3 consecutive
failures → degraded, not UI flapping; recovery requires 2 consecutive passes).
Start: port-conflict check before spawn; spawn with the assembled flag set;
record exact argv (shown read-only, copyable). Stop: graceful drain (30 s, no new
requests via gateway weighting) then kill. Restart offers graceful vs immediate.

### 6.3 Flag management

Canonical flag set per backend: model path, `--port`, `--n-ctx`, `--n-batch`,
`--tensor-split`, `--split-mode`, `--parallel` (slots), `--cache-type-k`,
`--flash-attn`, plus a raw-flags escape hatch (string, shown verbatim in argv).
Validation: n_ctx ≤ model's trained maximum (from GGUF metadata where readable,
else curated table); tensor-split sums to 1.0 within epsilon; port free.
Global defaults template with per-backend overrides; diff preview on every
change; failed apply rolls back to last-known-good config and reports.

### 6.4 LiteLLM gateway

Managed Python env via `uv` under the app data dir (path shown in UI).
`config.yaml` is generated from app state (model groups, backends, strategies,
fallbacks, OpenRouter credentials reference) — never hand-edited; the Gateway
page shows the exact YAML before apply. Hot-reload attempted via LiteLLM's
config reload; if unsupported by the pinned version, restart offered with the
same drain semantics as backends. Request log: 100-request ring buffer in memory
(timestamp, group, backend, tokens in/out, latency, status) feeding §4.8.

### 6.5 Windows ML secondary target

`WinMLServer.exe` managed as an optional supervised process on :8090, registered
in LiteLLM as a plain OpenAI-compatible backend. Constraints enforced from
research: single-GPU models only (no tensor-split surface exists), coarse
`--target gpu` with undocumented physical-GPU selection (record which adapter
responds at registration; warn if ambiguous), experimental status bannered.
Eligible model class: tool-runner-sized single-GPU loads (e.g. a Qwen3-8B
instance) — never the planner or coder-30b. "Re-check capabilities" re-probes
and updates the banner; if Microsoft later publishes tensor-split/device-index
controls, this section gets revisited, not the primary path.

## 7. Routing policy

Model groups (LiteLLM `model` entries routing to backend(s)):

| Group | Members | Default strategy | Fallback chain |
|---|---|---|---|
| `planner` | planner :8081 | latency-based | coder :8082 → OpenRouter |
| `coder-fast` | coder-fast :8083 | least-busy | coder :8082 → OpenRouter |
| `coder` | coder :8082 | simple-shuffle | OpenRouter |
| `tool-runner` | tool-runner :8084 (+ WinML :8090 if registered) | least-busy | OpenRouter |

**Task tagging contract.** Clients tag requests (Continue.dev/Cline configs set
the group directly; the gateway does not infer). Policy: `coder-fast` = defined
code tasks only, never tools; tool-needing work → `planner` or `coder`;
utility/tool-result chores → `tool-runner`.

**OpenRouter cloud tier.** Three triggers, in order: (1) failover — all group
members unhealthy; (2) overflow — queue depth or p99 latency above configured
thresholds for N seconds; (3) rubric-based escalation — a decider model (local
`planner`) scores task complexity/cost-efficiency and routes up. API key from
Windows Credential Manager, injected into the LiteLLM process env only. Cost
guardrails: per-day spend cap (default set at setup, user-editable), free-tier
models preferred in the fallback model list; cap breach → fallbacks disabled
with a Dashboard banner (fail closed to local, never silently spend).

## 8. MCP integration

GitHub and Google Drive MCP servers, each with mode local | remote | disabled.

- **Local mode:** app supervises the server process (spawn args from config),
  health-checked like backends (lighter cadence, 15 s). Credentials resolved from
  Windows Credential Manager by the Rust backend and injected into the server
  process env at spawn; the UI shows presence only.
- **Remote mode:** URL + credential-presence config only; the app does not
  supervise.
- **Editor & MCP page:** per-server connection test (tool-list probe), enable/
  disable, env display (keys only, values redacted).
- Sandboxing note: local MCP servers invoked by agents run inside the MXC
  boundary per §11 (Copilot reference model); remote MCP stays outside with
  policy-checked connections.

## 9. VS Code integration

- Extension install via `code --install-extension` with pinned versions for
  Continue.dev and Cline; version drift shown in Updates inventory.
- Config generation: `config.yaml` (Continue) / settings (Cline) pointing at
  LiteLLM :4000 with the four model groups preconfigured and task-tagging
  defaults matching §7. Existing user config is backed up before overwrite
  (timestamped), and diff shown before apply.
- The app does not embed MXC wrapping into these extensions in v1 (§1.3).

---

## 10. Configuration model

Single JSON file, serde-typed, versioned with migrations:
`%APPDATA%\local-llm-service-manager\config.json` (schema `version: u32`,
current 1). Secrets are never stored here — only credential *references*
(Credential Manager target names). On version bump, migrate forward; keep a
`.bak` of the last-known-good config; failed validation on load → fall back to
`.bak` and banner.

```rust
struct AppConfig {
    version: u32,
    data_dir: PathBuf,              // models, builds, logs, python env
    view_modes: HashMap<String, ViewMode>, // window_id -> Appliance|Console|Topology
    backends: Vec<BackendConfig>,
    gateway: GatewayConfig,
    models: Vec<ModelConfig>,
    telemetry: TelemetryConfig,     // poll interval, retention
    mxc: MxcConfig,
    winml: WinMlConfig,
    updates: UpdatePolicy,          // per-component pin/auto toggles
    notifications: NotificationConfig,
}

struct BackendConfig {
    id: String,                     // "planner" | "coder" | "coder-fast" | "tool-runner"
    enabled: bool,
    model_file: PathBuf,
    port: u16,
    flags: ServerFlags,             // n_ctx, n_batch, tensor_split: Vec<f32>, split_mode, ...
    raw_flags: Option<String>,
    restart_policy: RestartPolicy,  // max_retries, backoff_base_secs
    overrides_global: bool,
}

struct GatewayConfig {
    port: u16,                      // 4000
    groups: Vec<ModelGroup>,        // name, members, strategy, fallbacks
    routing_rules: Vec<RoutingRule>,// plain-language rule + litellm fragment + source
    openrouter: CloudTierConfig,    // enabled, cred_ref, daily_cap_usd, model_allowlist
    request_log_len: usize,         // 100
}

struct ModelConfig {
    id: String,
    gguf_path: PathBuf,
    quant: String,
    params_b: f32,
    sha256: String,
    verified_at: Option<DateTime<Utc>>,
    source_url: String,
    assigned_backends: Vec<String>,
}

struct MxcConfig {
    mode: MxcMode,                  // Learning | Permissive | Enforcement
    policy: serde_json::Value,      // validated against mxc schema version
    policy_schema_version: String,
    credential_refs: Vec<CredentialRef>, // target names only
    learning_review_after_days: u32,
}
```

Config writes are atomic (write temp + rename). Every mutation path used by the
UI goes through validation before write; the raw JSON is never the editing UI.

---

## 11. Security model

### 11.1 App privileges

Runs as the logged-in user, no elevation. Elevation is never requested silently;
operations needing it (none planned in v1) would be explicit one-shot prompts.
The app does not open inbound ports; all managed servers bind `127.0.0.1`.

### 11.2 MXC policy model

Default-deny baseline, shaped for the approved topology (inference on host,
tools sandboxed):

- **Filesystem:** read/write only to approved project directories; read-only to
  required tool runtimes (node, python, git); deny `.ssh`, credential stores,
  unrelated Documents paths, other personal data.
- **Network:** default-deny egress; allow loopback to LiteLLM :4000 and
  llama-server ports; allow only required GitHub, Google API, and OpenRouter
  endpoints. Loopback model chosen at setup from the PSEC probe: Model 1
  (direct WFP-filtered loopback) if
  `probes.baseContainerSupportsIngressHostLoopbackAllow` passes on PSEC 1.1;
  else Model 2 (proxy-only via host loopback proxy). Host→container initiation
  is not relied upon (documented proxy limitation on some builds); control
  channels are container-initiated or stdio pipes.
- **Credentials:** resolved host-side from Windows Credential Manager per
  invocation and injected as env vars; the container starts with a blank
  environment. Secrets never appear in policy JSON, logs, or the webview
  (presence booleans only).
- **UI:** disabled unless a tool explicitly needs it.
- **Modes:** Learning (block + record activity report) → review flow proposes
  least-privilege tightening per rule (accept/reject) → Enforcement. Permissive
  available for authoring. No GPU capability requested in v1 (inference stays on
  host; container-side GPU is not production-ready per research).

### 11.3 Update integrity

- App updates: signed via `tauri-plugin-updater` (signature verification before
  apply; failed verification aborts with a banner).
- llama.cpp builds and GGUFs: SHA-256 verified against curated checksums before
  "installed"; mismatch blocks use and offers re-download.
- The wizard's "Show commands" output must equal the spawned argv (asserted in
  tests) — transparency is a security property here, not just UX.

---

## 12. Updates

Component inventory (Settings → Updates): llama.cpp build, LiteLLM (pinned
version), each GGUF, VS Code extensions, MXC runtime/schema, GPU driver.
Per component: installed version, latest known (from local cache with
"checked Xs ago" — never claims live without a check), per-row Update,
per-component pin and auto-update toggles, "Check now".

- **llama.cpp build updates:** curated build list with Pascal (sm_60) support
  constraint; update = download + checksum + staged swap (new build validated
  with a smoke inference before backends are moved to it; rollback on failure).
- **Model updates:** re-download + checksum; never auto-delete the old file
  until the new one verifies.
- **Driver/CUDA re-checks:** driver version re-checked on schedule and on
  demand; mismatch vs the llama.cpp build's minimum → banner with remediation,
  backends keep running (no surprise restarts).
- **App self-update:** via updater plugin; prompts, never forced mid-inference;
  supervised processes are drained and restored across the update.

---

## 13. Non-functional requirements

- **Performance:** cold start to interactive UI < 3 s; telemetry tick-to-render <
  250 ms at 1 s cadence; log stream handles 1k lines/s without UI jank
  (virtualized table); backend restart (graceful) completes < 60 s end-to-end
  including health verification. Idle CPU < 1%, idle RAM < 250 MB for the app
  itself (excluding supervised processes).
- **Offline operation:** everything except downloads, update checks, and the
  OpenRouter tier works offline. Offline state is explicit (banner), not degraded
  mystery.
- **Reliability:** supervisor restarts crashed backends per policy with backoff;
  config corruption falls back to last-known-good; telemetry gaps are labeled
  STALE; no silent failures anywhere (every failure surfaces with log lines and
  an acknowledged/unacknowledged state).
- **Accessibility:** full keyboard navigation (sidebar, dialogs, view switcher
  via 1/2/3 when a window is focused); visible focus; status never color-only;
  respects Windows high-contrast where feasible in WebView2.
- **Data retention:** logs per-scope ring buffers (default 10k lines, configurable);
  request ring 100; telemetry 60 s high-res + 24 h downsampled for the dashboard
  chart. Diagnostics folder holds snapshots and bundles until retention expiry.

## 14. API surface

Tauri commands (request/response) and events (async). Signatures in Rust-ish
pseudocode; exact types derive from §10.

**Setup / wizard**
- `setup_get_prerequisites() -> Vec<PrereqCheck>`
- `setup_run_step(step_id: String) -> TaskId`
- `setup_retry_step(step_id: String) -> TaskId`, `setup_skip_step(step_id, note) `
- `setup_get_commands(step_id: String) -> Vec<PlannedCommand>` (must equal spawned argv)
- `setup_mark_manual(step_id: String, path: String, version: String)`
- `audit_run_full() -> TaskId` (re-run audit; check-type steps only)

**Backends**
- `backends_list() -> Vec<BackendStatus>`
- `backend_start(id) / backend_stop(id, graceful: bool) / backend_restart(id, graceful: bool) -> TaskId`
- `backend_get_config(id) -> BackendConfig`, `backend_preview_diff(id, patch) -> ConfigDiff`
- `backend_apply_config(id, patch, apply_mode: RestartNow | OnNextRestart) -> TaskId`
- `backend_get_launch_command(id) -> String`
- `backend_test_request(id, prompt: String, params: TestParams) -> TestResult`

**Gateway**
- `gateway_status() -> GatewayStatus`
- `gateway_get_config() -> GatewayConfig`, `gateway_preview_yaml() -> String`
- `gateway_apply_config(patch: GatewayPatch) -> TaskId`
- `gateway_test_routing(tagged_request) -> RoutingTrace`
- `gateway_recent_requests() -> Vec<RequestRecord>`

**Models**
- `models_catalog() -> Vec<CatalogEntry>` (with live fit-check)
- `model_download(id) / model_pause_download(id) / model_resume_download(id) / model_cancel_download(id)`
- `model_verify_checksum(id) -> VerifyResult`, `model_delete(id) -> ()` (guarded)
- `model_assign(id, backend_id, placement: TensorSplit)`

**GPUs / telemetry**
- `telemetry_snapshot() -> GpuSnapshot` (also streamed via event)
- `telemetry_set_interval(ms: u64)`, `telemetry_reset_peaks()`, `telemetry_save_snapshot() -> PathBuf`
- `placement_propose(model_id, split: Vec<f32>) -> PlacementProjection` (projected VRAM/GPU + fit verdict)
- `placement_apply(model_id, split: Vec<f32>) -> TaskId` (guided rolling restart)

**Logs / tasks**
- `logs_query(scope: LogScope, level, search, limit) -> Vec<LogLine>`
- `task_abort(task_id)`, `tasks_list() -> Vec<TaskInfo>`
- `diagnostics_export_bundle() -> PathBuf` (zip: logs + config snapshot + GPU snapshot)

**MCP / VS Code**
- `mcp_list() -> Vec<McpServerStatus>`, `mcp_test_connection(id) -> TestResult`
- `mcp_set_mode(id, mode)`, `vscode_install_extensions() -> TaskId`
- `vscode_write_config(backup: bool) -> ConfigDiff`

**MXC**
- `mxc_probe() -> MxcProbeResult` (PSEC version, ingress-support flag, verbatim)
- `mxc_get_policy() -> serde_json::Value`, `mxc_set_policy(policy) -> ValidationReport`
- `mxc_set_mode(mode)`, `mxc_run_self_test() -> ActivityReport`
- `mxc_get_activity_report(since) -> ActivityReport`
- `mxc_propose_tightening(report) -> Vec<PolicyProposal>` (accept/reject per rule)

**Windows ML**
- `winml_probe() -> WinMlCapabilities` (availability, limits, verbatim)
- `winml_register(model_id) / winml_unregister()`, `winml_status() -> WinMlStatus`

**Settings / updates**
- `settings_get(section) / settings_set(section, patch) -> ValidationReport`
- `updates_check_all() -> TaskId`, `updates_apply(component_id) -> TaskId`
- `updates_pin(component_id, version)`, `updates_set_auto(component_id, bool)`

**Events (Rust → UI)**
- `telemetry-tick(GpuSnapshot)`, `log-line(LogLine)`, `task-progress(TaskProgress)`,
  `backend-status(BackendStatus)`, `gateway-request(RequestRecord)`,
  `alert-raised(Alert)`, `vram-changed(FreeVramMap)` (drives fit-check recompute),
  `config-changed(ConfigChangeNotice)`.

## 15. Test plan

- **Unit (Rust, `cargo test`):** fit-check arithmetic, tensor-split validation
  (sums to 1.0), config validation (n_ctx bounds, port conflicts, flag allowlist),
  LiteLLM YAML generation (snapshot tests), MXC policy JSON schema conformance,
  download resume-offset math, version ordering (build tags, `-cuda` suffixes),
  "Show commands" string == spawned argv.
- **Unit (frontend, vitest):** formatters (GB/MiB, durations, relative time),
  table sort/filter, sparkline windowing, status-transition reducers.
- **Integration (Rust):** supervisor lifecycle against stub `llama-server`
  (start → healthy → kill → backoff restart → drain); config apply → file +
  restarted argv; download manager vs local HTTP (progress, pause/resume ranges,
  checksum-failure path); `TelemetrySource` trait-injected mock + forced `nvidia-smi`
  fallback; MXC policy round-trip in Learning mode on a canary command
  (Windows-only lane).
- **Tauri boundary:** typed request/response test per command; event-stream shape
  and ordering under a scripted backend.
- **E2E (tauri-driver, clean Windows VM snapshot, stub servers, no GPU):**
  first-run wizard to working stack with a small test GGUF; backend restart card
  flow with status transitions + log lines; n_ctx edit → diff preview → applied
  argv. Nightly, screenshot-diffed for layout regressions.
- **Hardware-in-the-loop (the 7865, on demand, `#[ignore]` in CI):** nvidia-smi vs
  `nvidia-smi` agreement within tolerance; real tensor-split load of Qwen3-8B
  across A4000+P100, observed-vs-configured split comparison; VRAM accounting
  reconciles (attributed ≈ reported used); thermal/power sane under load.
- **UI per view mode:** switcher present exactly where the §4 matrix says;
  per-window persistence across restart; corrupt value → Appliance fallback;
  Console/Topology show identical values to Appliance for the same timestamp;
  keyboard 1/2/3 switching.
- **Manual release checklist:** wizard on fresh profile; driver-update row; all
  windows exercised; theme switch; drawer auto-expand/collapse; STALE via
  blocked telemetry; kill-a-backend-mid-request → dashboard flags within one
  poll with logs attached; keyboard-only pass.

## 16. Implementation phases

Ordered; v1 = phases 0–5. Estimates are sequencing, not commitments.

- **P0 — Scaffold.** Tauri 2 + frontend shell, sidebar nav, theme tokens,
  command/event plumbing, log bus, CI building unsigned NSIS. Exit: window
  opens, nav works, one command round-trips.
- **P1 — Supervisor + Dashboard.** Backend state machine, spawn/health/drain/
  backoff, tray icon; Dashboard Appliance + Console views; backend Summary/Logs
  tabs. Exit: app launches, monitors, restarts 4 backends + gateway.
- **P2 — Setup wizard.** Prereq matrix, ordered step machine, llama.cpp download
  + verify, GGUF downloads with resume, LiteLLM env + config gen, extension
  install, MCP registration, Show-commands transparency, manual overrides,
  Re-run audit. Exit: clean-machine run reaches a working stack.
- **P3 — GPU telemetry + placement.** `nvidia-smi` poll loop (NVML deferred, see §5),
  per-process attribution, peak marks, GPUs window (both views), placement map,
  tensor-split editor with projected-VRAM validation + rolling restart.
  Exit: "where is each model" answered live.
- **P4 — Configuration system.** Typed schema → generated forms,
  global/per-backend resolution, diff preview, apply/rollback, gateway editor,
  full Settings pages. Exit: no hand-edited files for routine tuning.
- **P5 — Models + MXC + WinML.** Model library with live fit-check and checksum
  policy; MXC probe/policy form/Learning-mode reports/tightening flow/
  credential-presence wiring; WinML probe/register as secondary backend.
  Exit: sandboxed tool execution in Learning mode; WinML registered or
  provably unavailable with reasons recorded.
- **P6 — Updates, diagnostics, hardening.** Updates inventory + staged build
  swap with smoke validation; full diagnostic runner; bundles; notifications;
  signed installer CI; auto-update with drain/restore. Exit: installable by
  someone other than Pete.
- **P7 — Stretch.** Read-only web status page; WinML capability re-probe
  automation; request analytics beyond the ring buffer; Continue.dev/Cline MXC
  wrapping if an integration path emerges.

Rationale: supervise before installing (P1 before P2) so the wizard installs
into a system the app already runs; telemetry before config editing (P3 before
P4) so placement edits validate against live numbers; sandboxing after the
serving path is solid (P5) — it is the outer layer, not the core.

## 17. Open questions and risks

1. **P100 driver/CUDA support on Windows 11 — VERIFIED (Oct 8, 2026), now a pinned
   constraint, not a risk.** See §3.4: R580-branch drivers still support the P100
   on Win11; CUDA 12.x is the toolkit ceiling for Pascal (CUDA 13 dropped sm_60
   compilation); upstream cuda-12.4 llama.cpp builds carry the Pascal targets, so
   one pinned build serves the A4000 + both P100s. The wizard enforces the pin
   (rejects cuda-13.x assets on Pascal-bearing machines) and smoke-tests both GPU
   types at install time.
2. **MXC GPU passthrough.** Not production-ready (OpenShell lists it as a
   non-goal; `gpu_acceleration` capability unverified end-to-end). Non-blocking:
   inference stays on the host. Risk only if container-side embeddings are later
   wanted.
3. **PSEC 1.1 + ingress-support on his build.** Decides Model 1 vs Model 2
   loopback. Unknown until the probe runs on the 7865; the app handles both,
   but Model 2 (host proxy) is meaningfully more code.
4. **WinMLServer limits.** No tensor-split, coarse GPU selection, experimental,
   x64 discrete-GPU packaging unverified. Contained: secondary target only,
   single small model, bannered limits.
5. **No Continue.dev/Cline MXC integration exists.** Wrapping their tool
   execution is real SDK integration work, not configuration. v1 covers the
   app's own sandbox policy and probes; the IDE path is P7 at best.
6. **Frontend framework choice.** Svelte 5 is the candidate; the Tauri boundary
   is framework-independent so this is reversible late — but charting density
   (sparklines at 1 s cadence across views) should be prototyped early in
   whichever framework to validate the no-VDOM-churn assumption.
7. **NVML runtime loading / `mxc-sdk` crate maturity.** NVML (`nvml.dll`) loading is deferred to a hardening pass (v1 ships the `nvidia-smi` path behind the `TelemetrySource` trait seam); `mxc-sdk` is young; pin versions and
   keep the fallbacks (`nvidia-smi` XML parse; `wxc-exec.exe --config-base64`)
   as first-class paths with tests, not dead code.
8. **LiteLLM hot-reload support.** Depends on the pinned version; the app must
   handle "reload unsupported → restart" without losing in-flight requests
   (drain first).
9. **96 GB RAM plan vs 128 GB symmetric.** Wizard disk/RAM checks and the
   placement math must read actual RAM, not the plan; RAM-offload rows in the
   placement map depend on it.
10. **Scope creep toward a second machine.** The architecture is single-host;
    any future remote management wants a clean split between the supervisor
    core and the UI — worth one package boundary in the Rust code even though
    remote UI is a non-goal.
