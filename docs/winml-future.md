# WinML: deferred — future improvement

Status: **deferred** (Pete's decision, 2026-10-15). The full WinML
wizard setup that was approved earlier the same day is dropped from
the roadmap until the WinMLServer surface is ready for it. The
existing probe-only `winml-register` wizard step and the `winml`
config section stay in place but inert — nothing installs, enables,
or starts a WinML server, and the app makes no claim that WinML
works.

Full research findings: `docs/winml-server-research-2026-10-15.md`.

Status check against the code (v0.1.40): still deferred, and the
inert surface is exactly the probe-only one described above. The
registry carries four WinML ops — `winml_probe`, `winml_register`,
`winml_unregister`, `winml_status` (desktop-only, in
`crates/manager-app/src/registry.rs`) — backed by
`crates/manager-winml`, which probes and records a registration;
nothing in the tree downloads, installs, enables, or starts a
WinML server. The known-wrong assumptions the research flagged are
still in the code, harmless only because the path is inert:
`winml_server_exe_path` still returns
`C:\Program Files\WindowsML\WinMLServer.exe`, and the gateway
generator still emits the WinML deployment as `openai/winml` with
the placeholder key `local` under the `tool-runner` group when
`winml.enabled` is set (`crates/manager-gateway/src/lib.rs`).
There is no shipped WinML serving path.

## Why it is not ready (verified 2026-10-15, primary sources)

- WinMLServer exists **only in the experimental NuGet line**:
  `Microsoft.Windows.AI.MachineLearning` 2.7.2021-experimental, plus
  `Microsoft.Windows.AI.MachineLearning.LibLlama.Core`
  0.5.0.2022-experimental for GGUF support. The latest stable line
  (2.4.89) contains no server binaries at all.
- There is no installer, no Download Center package, no winget
  package, and no `C:\Program Files\WindowsML` — the path the app's
  current WinML code assumes does not exist. The binaries are loose
  files under `runtimes/win-x64/native/` inside the nupkgs.
- Only the **CPU** LibLlama backend is verified; the GPU backend
  package ids could not be confirmed against any primary source.
- Microsoft describes this surface as experimental/preview; the
  September 2025 Windows ML GA does not cover the server.
- The runtime license sets `requireLicenseAcceptance`; any future
  install step must surface it.

## The design constraint that shapes any future build

Every server start mints a **new random 64-hex access key** (printed
to stdout as `WINMLSERVER_ACCESS_KEY`); requests without
`Authorization: Bearer <key>` get 401, and the served model name is
the launch-time `--model-id`. Consequences:

- A probe-only registration (what the app has today) can never work:
  nothing external can know the key.
- The app must **own the lifecycle**: start `WinMLServer.exe` under
  supervision with the chosen model (qwen3-8b), parse the key and
  model id from stdout, inject both into the generated gateway
  config, and regenerate/refresh that config on every WinML restart.
- One model per server process.

## Revisit when

- A **stable** (non-experimental) `Microsoft.Windows.AI.MachineLearning`
  release ships WinMLServer binaries, or Microsoft publishes a
  supported standalone distribution for the server; and
- the LibLlama GPU backend package ids are verifiable from a primary
  source (otherwise the build is CPU-only and must say so).

At that point reuse the install pattern this app already has from
the MXC and PostgreSQL steps: pinned download, SHA-256 verification
(self-computed pins are in the research file — Microsoft publishes
none), extraction into the app data directory, a supervised process,
and a wizard step gated on Windows 11 24H2 (build 26100)+.
