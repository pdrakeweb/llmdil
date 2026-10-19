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
- **llama.cpp build:** the application pins upstream llama.cpp **b11658**
  (`llama-b11658-bin-win-cuda-12.4-x64.zip`, plus the matching cudart
  archive), which ships the Pascal targets. One build serves the RTX
  A4000 (Ampere) and both P100s. CUDA 13.x builds are never used on this
  machine.
- **Gateway database:** the wizard installs a bundled PostgreSQL
  (pinned `postgresql-18.6-1-windows-x64-binaries.zip`) under the app data
  directory — no separate database install is required.
- **LiteLLM:** the gateway runs in a managed Python environment; the
  version in use is the one named by the `litellm` component pin in the
  app configuration (1.105.1 is the version verified with this release;
  the wizard records the probed version at gateway start).

If the in-app wizard reports a driver/build mismatch, update the NVIDIA
driver first — do not hunt for a different llama.cpp build.

## Installing the application

1. Download `LLMdil_<version>_x64-setup.exe` from the
   [Releases page](https://github.com/pdrakeweb/llmdil/releases).
2. Run it. Windows SmartScreen may show an "unrecognized app" prompt on first
   release — click *More info → Run anyway* (expected until the installer is
   code-signed).
3. The installer wizard walks through:
   - **Welcome** — what the application manages.
   - **License** — Apache-2.0.
   - **Install location** — defaults to `%LOCALAPPDATA%\LLMdil`.
   - **Install** — copies the application. A Start Menu folder "LLMdil" is
     created. No drivers, models, or build tools
     are installed here; the in-app setup wizard handles the LLM stack.
   - **Finish** — carries the single launch decision: the *Launch LLMdil*
     checkbox, beside a *Create desktop shortcut* checkbox (a desktop
     shortcut is created only when that box is checked).
4. On first launch, the **setup wizard** takes over — see
   [`usage.md`](usage.md) for its 12 steps.

The installer is per-user: no UAC elevation, no system-wide changes.

### Silent install

```powershell
LLMdil_0.1.40_x64-setup.exe /S
```

Installs with defaults to `%LOCALAPPDATA%`. Add `/D=C:\path` to override the
directory (must be the last argument, no quotes even with spaces).

## Uninstalling

Use *Settings → Apps → Installed apps → LLMdil →
Uninstall*, or re-run the installer and choose *Uninstall*.

The uninstaller removes the application binaries. User data in
`%APPDATA%\local-llm-service-manager` (config.json, logs, downloaded GGUF
models, managed Python env) is **kept** unless you answer "Yes" to the
removal prompt — the prompt defaults to "No", and downloaded GGUFs are
large and are kept by default.

## Troubleshooting

| Symptom | Fix |
|---|---|
| SmartScreen blocks the installer | *More info → Run anyway* (unsigned first releases) |
| "WebView2 runtime not found" | Accept the bootstrapper offer, or install the Evergreen Standalone Installer from Microsoft |
| Wizard: "no supported GPU detected" | Update the NVIDIA driver (see matrix above); check Device Manager shows all three cards |
| Wizard: driver/build mismatch | Update the driver; the pinned cuda-12.4 build requires R550+ (Windows ≥ 551.61) |
| Port conflict on `:4000` / `:8081–8084` | The wizard's port check names the conflicting process — stop it or change ports in Settings |
| Installer won't start | Re-download (partial download); check the SHA-256 on the release page |
