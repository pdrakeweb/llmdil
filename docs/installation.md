# Installation — Windows

## Prerequisites

| Requirement | Minimum | Notes |
|---|---|---|
| Windows | 11 24H2 (build ≥ 26100) | MXC process-container floor |
| NVIDIA driver | R550 branch or newer (Windows ≥ 551.61 — the CUDA 12.4 floor per NVIDIA's release notes) | R580 branch (e.g. 581.57 Studio) verified with the Tesla P100 — see the verified matrix below |
| WebView2 | Evergreen runtime | In-box on Windows 11; the installer checks and offers the bootstrapper if missing |
| Disk | 120 GB free | 4 GGUF models (~60–90 GB) + llama.cpp builds + headroom |
| VS Code | Current stable | `code` CLI on PATH; the setup wizard installs the extensions |
| Network | Required for setup | Model and build downloads; optional at runtime except the OpenRouter tier |

### Verified GPU / driver / CUDA matrix

Researched October 2026 — this is settled, not experimental:

- **Driver:** current R580-branch drivers still support the Tesla P100
  (Pascal) on Windows 11. No driver dead-end.
- **CUDA:** CUDA 12.x is the ceiling for Pascal — CUDA 13 removed `sm_60`
  compilation. The driver still *runs* Pascal; only the compiler moved on.
  CUDA 12.x-built binaries run on R580 drivers via minor-version
  compatibility.
- **llama.cpp build:** the application pins upstream `bin-win-cuda-12.4-x64`
  release assets, which ship the Pascal targets. One build serves the RTX
  A4000 (Ampere) and both P100s. CUDA 13.x builds are never used on this
  machine.

If the in-app wizard reports a driver/build mismatch, update the NVIDIA
driver first — do not hunt for a different llama.cpp build.

## Installing the application

1. Download `Local-LLM-Service-Manager_<version>_x64-setup.exe` from the
   [Releases page](https://github.com/pdrakeweb/local-llm-service-manager/releases).
2. Run it. Windows SmartScreen may show an "unrecognized app" prompt on first
   release — click *More info → Run anyway* (expected until the installer is
   code-signed).
3. The installer wizard walks through:
   - **Welcome** — what the application manages.
   - **License** — Apache-2.0.
   - **Install location** — defaults to `%LOCALAPPDATA%\Local LLM Service Manager`.
   - **Options** — Start Menu shortcut (on), Desktop shortcut (off by default).
   - **Install** — copies the application. No drivers, models, or build tools
     are installed here; the in-app setup wizard handles the LLM stack.
   - **Finish** — optional *Launch Local LLM Service Manager*.
4. On first launch, the **setup wizard** takes over — see
   [`usage.md`](usage.md) for its 10 steps.

The installer is per-user: no UAC elevation, no system-wide changes.

### Silent install

```powershell
Local-LLM-Service-Manager_0.1.0_x64-setup.exe /S
```

Installs with defaults to `%LOCALAPPDATA%`. Add `/D=C:\path` to override the
directory (must be the last argument, no quotes even with spaces).

## Uninstalling

Use *Settings → Apps → Installed apps → Local LLM Service Manager →
Uninstall*, or re-run the installer and choose *Uninstall*.

The uninstaller removes the application only. It asks before touching
`%APPDATA%\llm-manager` (configuration, logs) and the model library —
downloaded GGUFs are large and are kept by default.

## Troubleshooting

| Symptom | Fix |
|---|---|
| SmartScreen blocks the installer | *More info → Run anyway* (unsigned first releases) |
| "WebView2 runtime not found" | Accept the bootstrapper offer, or install the Evergreen Standalone Installer from Microsoft |
| Wizard: "no supported GPU detected" | Update the NVIDIA driver (see matrix above); check Device Manager shows all three cards |
| Wizard: driver/build mismatch | Update the driver; the pinned cuda-12.4 build requires R550+ (Windows ≥ 551.61) |
| Port conflict on `:4000` / `:8081–8084` | The wizard's port check names the conflicting process — stop it or change ports in Settings |
| Installer won't start | Re-download (partial download); check the SHA-256 on the release page |
