# Bundled sidecar binaries

`tauri.conf.json` declares two `externalBin` sidecars that the NSIS
installer places beside the app executable in the install dir
(`%LOCALAPPDATA%\LLMdil`):

- `llm-service-host.exe` — the Windows service shim
  (`crates/manager-services`, bin `llm-service-host`).
- `llmdil-log-receiver.exe` — the Prometheus-local log receiver
  (`crates/manager-logreceiver`, bin `llmdil-log-receiver`), whose
  `deploy` / `remove` subcommands the app drives from Settings →
  Supervision → Log receiver.

The binaries themselves are build outputs and are never committed.
Build pipelines stage them here, named with the target-triple
suffix Tauri expects, before any `src-tauri` build step runs:

```powershell
cargo build -p manager-services --bin llm-service-host
cargo build -p manager-logreceiver --bin llmdil-log-receiver
Copy-Item target\debug\llm-service-host.exe `
  src-tauri\binaries\llm-service-host-x86_64-pc-windows-msvc.exe
Copy-Item target\debug\llmdil-log-receiver.exe `
  src-tauri\binaries\llmdil-log-receiver-x86_64-pc-windows-msvc.exe
```

(the release workflow stages `--release` builds the same way).
`prometheus-gates.yml` performs this staging before its src-tauri
steps; a local src-tauri build needs the same two commands first.
