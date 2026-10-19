# Usage

## First run: the setup wizard

The setup wizard runs automatically on first launch. It is a 12-step
checklist; steps execute in order, each showing live state
(queued → running → done, or failed with retry). A progress bar tracks
overall completion with elapsed time and ETA. You can also run any step
individually.

| # | Step | What it does |
|---|---|---|
| 1 | Detect hardware and drivers | GPU inventory, driver version, CUDA compatibility, port availability |
| 2 | Install llama.cpp CUDA build | Downloads the pinned build (llama.cpp **b11658**, cuda-12.4 x64), verifies SHA-256 |
| 3 | Configure tensor-split | Computes per-model GPU splits from discovered VRAM |
| 4 | Download models and verify checksums | The enabled models in the curated catalog (6 entries in v0.1.40, incl. the opt-in Planner-MTP build); pause/resume/cancel; checksum before install |
| 5 | Install PostgreSQL (gateway database) | Installs the bundled PostgreSQL (pinned 18.6-1 binaries) under the app data directory and initializes the gateway database |
| 6 | Start LiteLLM gateway | Sets up the managed Python environment, generates `config.yaml`, starts `:4000` |
| 7 | Install VS Code extensions | Continue.dev + Cline (pinned), configured against `:4000` |
| 8 | Configure MCP servers | GitHub + Google Drive servers; checks credential presence (values never displayed) |
| 9 | Register Windows ML backend | Probes `WinMLServer`; registers one small model as a secondary backend |
| 10 | Install MXC runtime | Installs the pinned MXC runtime |
| 11 | Apply MXC policies | Writes the default sandbox policy; Learning mode recommended first; runs a harmless self-test |
| 12 | Smoke-test inference | One test request per backend through the gateway; records baseline latencies |

**Failed steps** show the last log lines inline with *Retry*, *Open full log*,
and *Skip with note*. **Show commands** on any step expands the exact
commands, URLs, checksums, and file paths with copy buttons.
**Mark as manually installed** records anything you set up yourself so the
wizard stops trying to manage it.

**Re-run audit** (wizard finish screen, Dashboard, or Settings) re-executes
all checks without reinstalling: re-verifies prerequisites, re-checksums
models and the llama.cpp build, re-probes backend health, re-validates ports
and config. If an audit finds a checksum mismatch it offers to re-download —
it never reinstalls unprompted.

## Main windows

- **Dashboard** — health-at-a-glance: service cards (with gateway
  statistics), GPU strip, model placement, 24h throughput, and a
  *Start All Services* bar when anything is stopped. The toolbar's *View*
  selector switches the same data between Appliance (default), Console
  (dense tables), and Topology (node map) presentations; the choice
  persists per window.
- **Service Map** — the full cluster topology: clients → gateway → backends
  → GPUs → models, with the MXC sandbox boundary and a live event timeline.
- **GPUs** — per-GPU gauges and history; Console view adds process
  attribution and tensor-split detail.
- **Backends / backend detail** — per-backend stats, latency charts,
  health-check history, server flags; start/stop/restart; Test-request tab
  sends one raw prompt and shows the JSON response. Enable/disable is a
  two-position state switch on the card, not a button. The flags panel
  carries the expert-offload slider (with architecture guardrails and a
  *Save with override* path for unverified positions) and, on
  MTP-capable models only, the MTP draft-length selector
  (Off / 1 / 2 / 3 / 4). The **Add a backend** form has a
  *Launch parameters* section (context, batch, offload, GPU placement)
  with the same controls. Every backend card carries an icon-only
  go-to-logs button that deep-links to its Logs scope.
- **Models** — library with VRAM fit checks, download queue, checksum status.
- **Logs** — unified log stream across backends, gateway, the app, MCP
  servers, the wizard, and Optimize, with level filtering and a
  diagnostic-bundle export. The view is scope-addressable:
  `#/logs?scope=<id>&follow=1` is the deep link the go-to-logs buttons on
  backend, gateway, MCP, and Optimize surfaces use.
- **Gateway** — gateway groups and routing rules (pattern → model group →
  backends; typed ContextOverflow rules can inject a compaction advisory
  instead of only rerouting), strategies
  (simple-shuffle, least-busy, latency-based), OpenRouter cloud tier and
  fallback policy.
- **MCP Servers** (`#/mcp`) — registered MCP servers and their exposure,
  with the same enable/disable state switch idiom. Starting the gateway
  interactively first prompts for any gateway MCP server whose credential
  is missing: each such server is offered its signup / API-key link, a
  credential field, and a *Store credential* action (or Skip).
- **Optimize** (`#/optimize`) — per-backend consultation, baseline
  measurements and experiments, the operator directive, Hugging Face
  candidate discovery, and a go-to-logs button for the verbose Optimize
  log (Logs scope `optimize`).
- **Activation Plan** (`#/activation-plan`) — saved-but-not-live changes
  collected as one plan (pending changes count, drain & apply, apply now,
  cancel changes). A banner at the top of the main column carries the
  same plan summary on every view.
- **Settings** — tabbed: General, Updates & diagnostics, Notifications
  (Windows toast categories: Operations, Failures, Activation),
  MXC sandbox (policy editor with Learning-mode activity reports),
  Remote Access (Admin API enable switch and admin users), and
  Supervision (per-backend process | service supervision mode).
  Credential references and per-component pin and auto-update toggles
  also live here. (Update *checking* and *applying* are not in v0.1.x —
  see "Updates" below.)

## Daily workflow

1. Glance at the Dashboard: all services green, GPUs nominal.
2. After a Learning-mode period, review the MXC activity report and tighten
   the policy (Settings / MXC).
3. Model downloads, checksum verification, and VRAM fit checks live in Models.
   Re-downloading a newer GGUF revision is manual in v0.1.x (automated update
   checks land in a later release — see "Updates" below).
4. Anything looks wrong → Logs filtered to the service, or Re-run audit for
   a full verification pass.

## Updates

v0.1.x tracks per-component pins and auto-update toggles (llama.cpp build,
LiteLLM, each GGUF, VS Code extensions, MXC runtime/schema, GPU driver), but
does **not** check for or apply updates yet — "Check now" and per-row Update
arrive with the P6 updates phase (staged build swap with smoke validation,
per spec §12). The UI reports this explicitly rather than claiming a check
happened.

## Where things live

- Application: `%LOCALAPPDATA%\LLMdil`
- Config and logs: `%APPDATA%\local-llm-service-manager`
- Models: `%APPDATA%\local-llm-service-manager\models` (configurable in Settings)
- llama.cpp builds: `%APPDATA%\local-llm-service-manager\bin`

## Troubleshooting

| Symptom | Where to look |
|---|---|
| Backend won't start | Backend detail → flags and last log lines; Logs filtered to the backend |
| GPU shows STALE | Telemetry lost the GPU — check `nvidia-smi` in a terminal; reseat/driver issue if absent there too |
| Slow responses | Backend detail latency chart; check queue depth and whether the OpenRouter tier is absorbing overflow (Gateway) |
| Tool calls blocked unexpectedly | Settings / MXC → recent decisions; Learning-mode report shows what the policy saw |
| Config corrupted | `%APPDATA%\local-llm-service-manager\config.json` is versioned with automatic backup on migration; restore from `config.json.bak` |
| Everything is green but answers are wrong | Backend detail → Test-request tab: bypasses routing and hits the backend directly |
