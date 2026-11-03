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

## Bundled sidecars

`bundle.externalBin` in `tauri.conf.json` bundles two binaries
beside the app executable in the install dir: `llm-service-host.exe`
(the service shim) and `llmdil-log-receiver.exe` (the log receiver,
deployed from Settings → Supervision → Log receiver). Build
pipelines stage them under `binaries/` with the target-triple
suffix before any src-tauri build step — see
[`binaries/README.md`](binaries/README.md).

## Uninstall: user data is kept by default

The uninstaller always removes app binaries. User data in
`%APPDATA%\local-llm-service-manager` (config.json, logs, downloaded GGUF
models, managed Python env) is **kept** unless the user answers "Yes" to the
removal prompt (`NSIS_HOOK_PREUNINSTALL` in
[`installer-hooks.nsi`](installer-hooks.nsi); the prompt defaults to "No").
The same hook attempts to remove the log receiver service (running
the bundled receiver's `remove` subcommand elevated) when one is
registered; a declined prompt or a silent uninstall leaves the
registration in place with an instruction for removing it manually
instead of failing the uninstall.

## Silent install flags (built into NSIS)

```cmd
installer.exe /S                 :: silent install, no wizard pages
installer.exe /S /D=C:\My\Dir    :: silent install to a custom directory
```

`/D=` must be the **last** argument and must not be quoted, even if the path
contains spaces.

## Release build checklist

Current release: v0.1.40. The installer artifact is
`LLMdil_<version>_x64-setup.exe` (e.g. `LLMdil_0.1.40_x64-setup.exe`),
installing per-user to `%LOCALAPPDATA%\LLMdil`.

1. Application icons are in place — `icons/icon.ico`, `icons/32x32.png`,
   `icons/128x128.png`, `icons/128x128@2x.png` (plus `icon.icns` /
   `icon.png`) — and referenced via `bundle.icon` in `tauri.conf.json`
   (tauri-bundler requires icon files at bundle time).
2. Verify `bundle.licenseFile` in `tauri.conf.json` still points at the
   repo LICENSE (`../LICENSE`).
3. The release workflow (`.github/workflows/release.yml`) builds this
   installer on `windows-latest` and attaches the NSIS bundle to the GitHub
   Release on every `v*` tag push; `.github/workflows/install-test.yml`
   exercises the built installer. No updater artifacts are produced in
   v0.1.x (no auto-update; see spec §12).
