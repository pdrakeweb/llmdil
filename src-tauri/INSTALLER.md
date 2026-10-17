# Installer (NSIS)

The Windows installer is a proper NSIS wizard built by `tauri-bundler` from
[`tauri.conf.json`](tauri.conf.json). Per-user install mode — no UAC elevation.

## Wizard pages

1. **Welcome** — stock MUI welcome page.
2. **License** — shows the repo [`LICENSE`](../LICENSE) (Apache-2.0).
3. **Directory** — per-user install directory (default under `%LOCALAPPDATA%`);
   overridable at install time.
4. **Install files** — copies app binaries. No launch prompt here: the
   installer asks about launching exactly once, on the Finish page.
5. **Finish** — MUI finish page carrying the single launch decision: the
   **Launch LLMdil** checkbox runs the app when the wizard closes, next to
   a **Create desktop shortcut** checkbox (both from the custom template
   [`installer-template.nsi`](installer-template.nsi): `MUI_FINISHPAGE_RUN`
   and the repurposed `MUI_FINISHPAGE_SHOWREADME` checkbox).

Language: English only, no language selector. Start Menu folder
"LLMdil" is created; a desktop shortcut is created only when the
Finish page's **Create desktop shortcut** box is checked.

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
