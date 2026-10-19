# LLMdil

Local Language Model Distribution & Inference Layer

Windows desktop application (Tauri 2 + Rust) that installs, configures,
supervises, and monitors a local LLM serving stack on a single workstation.

Target machine: Dell Precision 7865 — Threadripper PRO 5945WX, RTX A4000 16 GB
+ 2× Tesla P100 16 GB, Windows 11 Pro.

Current release: **v0.1.40** (October 18, 2026).

## What it does

- Runs a first-run **setup wizard**: 12 auto-executing steps (hardware/driver
  detection → llama.cpp install → tensor-split config → model downloads with
  checksums → bundled PostgreSQL install → LiteLLM gateway → VS Code
  extensions → MCP servers → Windows ML backend → MXC runtime → MXC policies
  → inference smoke test), each checking off on completion, with an overall
  progress bar and an on-demand **re-run audit**
- Supervises `llama-server` (CUDA build) subprocesses, one per backend, with
  tensor-split placement across the GPUs; a backend can instead run under
  Windows service supervision (Settings → Supervision, per-backend
  process | service mode, via the `llm-service-host.exe` shim)
- Manages the LiteLLM gateway (`:4000`) unifying the backends into gateway
  groups, with routing rules (including typed ContextOverflow rules that can
  inject a compaction advisory) and an OpenRouter cloud tier for failover
  and overflow
- Stages configuration changes in an **activation plan** (pending changes,
  drain & apply) instead of applying them live
- Provides an **Optimize** page (`#/optimize`): per-backend consultation,
  measurements and experiments, an operator directive, Hugging Face
  candidate discovery, and a verbose Optimize log (Logs scope `optimize`)
- Monitors GPU telemetry (NVML, with `nvidia-smi` XML fallback); the
  Dashboard also carries gateway statistics
- Manages the model library: downloads with SHA-256 verification, VRAM fit
  checks, an expert-offload slider with architecture guardrails and an
  explicit override, and — on MTP-capable models only — an MTP draft-length
  selector (Off / 1 / 2 / 3 / 4)
- Wires VS Code for Continue.dev and Cline
- Configures MCP servers (GitHub, Google Drive, and others) from the
  **MCP Servers** page, including a credential prompt at gateway start:
  servers missing a credential are offered a signup / API-key link and a
  **Store credential** action before the start proceeds
- Registers Windows ML `WinMLServer` as a secondary backend for small
  single-GPU models
- Manages MXC (Microsoft Execution Containers) sandbox policies for agent tool
  execution
- Exposes an Admin API whose settings and admin users are managed from the
  desktop UI (Settings → Remote Access), and reports activity through
  Windows toast notifications (Operations / Failures / Activation
  categories, Settings → Notifications)

## Install

Download the installer from the
[Releases page](https://github.com/pdrakeweb/llmdil/releases)
and follow [`docs/installation.md`](docs/installation.md). The installer sets
up the application; the in-app setup wizard then installs and configures the
LLM stack itself.

Requirements: Windows 11 24H2+, NVIDIA driver R550 or newer (Windows
≥ 551.61, the floor for the pinned CUDA 12.4 llama.cpp build b11658; R580
branch verified with the Tesla P100), 120 GB free disk, VS Code. The full verified
GPU/driver/CUDA matrix is in the
[technical specification](docs/technical-specification.md) §3.4.

## Usage

See [`docs/usage.md`](docs/usage.md) for the first-run walkthrough, the main
windows, and troubleshooting.

## Documentation

- [`docs/installation.md`](docs/installation.md) — installing on Windows
- [`docs/usage.md`](docs/usage.md) — using the application
- [`docs/technical-specification.md`](docs/technical-specification.md) — full
  technical specification (architecture, requirements, API, test plan, phases)
- [`docs/design-b-hybrid.md`](docs/design-b-hybrid.md) — approved UI design
  record (Design B base with Console and Topology alternate views)
- [`docs/mockups/`](docs/mockups/) — rendered UI mockup galleries

## Development

Prerequisites: Rust stable toolchain, Node 24+.

```powershell
# Run all unit tests
cargo test

# Build the UI
cd ui; npm install; npm run build

# Full Windows build + installer (runs in CI on version tags)
# See .github/workflows/release.yml
```

Repository layout:

- `crates/` — Rust workspace: `manager-config`, `manager-wizard`,
  `manager-supervisor`, `manager-telemetry`, `manager-gateway`,
  `manager-models`, `manager-mxc`, `manager-winml`, `manager-api`,
  `manager-app`, `manager-mcp`, `manager-optimize`, `manager-present`,
  `manager-services`
- `src-tauri/` — Tauri 2 application shell and command/event API
- `ui/` — frontend
- `.github/workflows/` — CI (tests) and release autobuild (Windows installer)

`crates/MODULES.md` maps each crate to its spec sections and public API.

## License

Apache-2.0. See [LICENSE](LICENSE).
