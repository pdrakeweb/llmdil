# Installer (NSIS)

The Windows installer is a proper NSIS wizard built by `tauri-bundler` from
[`tauri.conf.json`](tauri.conf.json). Per-user install mode — no UAC elevation.

## Wizard pages

1. **Welcome** — stock MUI welcome page.
2. **License** — shows the repo [`LICENSE`](../LICENSE) (Apache-2.0).
3. **Directory** — per-user install directory (default under `%LOCALAPPDATA%`);
   overridable at install time.
4. **Install files** — copies app binaries; at the end a prompt offers
   **"Launch Local LLM Service Manager now?"** (implemented in
   [`installer-hooks.nsi`](installer-hooks.nsi), `NSIS_HOOK_POSTINSTALL`).
5. **Finish** — stock MUI finish page.

Language: English only, no language selector. Start Menu folder
"Local LLM Service Manager" is created; **no desktop shortcut** (stock tauri
NSIS template does not create one).

## Uninstall: user data is kept by default

The uninstaller always removes app binaries. User data in
`%APPDATA%\local-llm-service-manager` (config.json, logs, downloaded GGUF
models, managed Python env) is **kept** unless the user answers "Yes" to the
removal prompt (`NSIS_HOOK_PREUNINSTALL` in
[`installer-hooks.nsi`](installer-hooks.nsi); the prompt defaults to "No").

## Silent install flags (built into NSIS)

```cmd
installer.exe /S                 :: silent install, no wizard pages
installer.exe /S /D=C:\My\Dir    :: silent install to a custom directory
```

`/D=` must be the **last** argument and must not be quoted, even if the path
contains spaces.

## Before the first release build

1. Add application icons: `icons/icon.ico`, `icons/32x32.png`,
   `icons/128x128.png`, `icons/128x128@2x.png` and reference them via
   `bundle.icon` in `tauri.conf.json` (tauri-bundler requires icon files at
   bundle time).
2. Verify `bundle.windows.nsis.license` still points at the repo LICENSE.
3. The release workflow (`.github/workflows/release.yml`) builds this
   installer on `windows-latest` and attaches the NSIS bundle to the GitHub
   Release on every `v*` tag push. No updater artifacts are produced in v1
   (no auto-update; see spec §12).
