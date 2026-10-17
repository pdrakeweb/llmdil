# Local LLM Service Manager

Windows desktop application (Tauri 2 + Rust) that installs, configures,
supervises, and monitors a local LLM serving stack on a single workstation.

Target machine: Dell Precision 7865 — Threadripper PRO 5945WX, RTX A4000 16 GB
+ 2× Tesla P100 16 GB, Windows 11 Pro.

## What it does

- Runs a first-run **setup wizard**: 10 auto-executing steps (hardware/driver
  detection → llama.cpp install → tensor-split config → model downloads with
  checksums → LiteLLM gateway → VS Code extensions → MCP servers → Windows ML
  backend → MXC policies → inference smoke test), each checking off on
  completion, with an overall progress bar and an on-demand **re-run audit**
- Supervises `llama-server` (CUDA build) subprocesses, one per backend, with
  tensor-split placement across the GPUs
- Manages the LiteLLM gateway (`:4000`) unifying the backends, with an
  OpenRouter cloud tier for failover and overflow
- Monitors GPU telemetry (NVML, with `nvidia-smi` XML fallback)
- Manages the model library: downloads with SHA-256 verification, VRAM fit checks
- Wires VS Code for Continue.dev and Cline
- Configures MCP servers (GitHub, Google Drive)
- Registers Windows ML `WinMLServer` as a secondary backend for small
  single-GPU models
- Manages MXC (Microsoft Execution Containers) sandbox policies for agent tool
  execution

## Install

Download the installer from the
[Releases page](https://github.com/pdrakeweb/local-llm-service-manager/releases)
and follow [`docs/installation.md`](docs/installation.md). The installer sets
up the application; the in-app setup wizard then installs and configures the
LLM stack itself.

Requirements: Windows 11 24H2+, NVIDIA driver R535 or newer (R580 branch
verified with the Tesla P100), 120 GB free disk, VS Code. The full verified
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
  `manager-models`, `manager-mxc`, `manager-winml`
- `src-tauri/` — Tauri 2 application shell and command/event API
- `ui/` — frontend
- `.github/workflows/` — CI (tests) and release autobuild (Windows installer)

`crates/MODULES.md` maps each crate to its spec sections and public API.

## License

Apache-2.0. See [LICENSE](LICENSE).
