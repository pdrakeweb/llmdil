# WinMLServer distribution and contract — research findings

Researched: October 15, 2026 (America/New_York). Purpose: determine how the Local LLM
Service Manager's setup wizard can install and run Microsoft's Windows ML server
(`WinMLServer.exe`) as a pinned, hash-verified artifact, the way it already handles
MXC (`@microsoft/mxc-sdk`) and PostgreSQL (EDB binaries zip).

Bottom line: **WinMLServer is separately distributable, but not the way the app
currently assumes.** There is no installer, no `C:\Program Files\WindowsML` location,
and no unauthenticated endpoint. The server ships as loose binaries inside a NuGet
package — which fits the app's MXC pattern (download → verify → extract whole folder
→ run from the app data dir) with no admin rights. Two app assumptions are flatly
wrong against the real server: the exe path, and the absence of authentication
(the real server mints a random access key on every start and returns HTTP 401
without it, so the app's existing probe and gateway registration would fail).

---

## 1. Canonical distribution channel

`WinMLServer.exe` ships inside the **NuGet package `Microsoft.Windows.AI.MachineLearning`**
(the self-contained Windows ML runtime package), under `runtimes/win-x64/native/`
(and `win-arm64`). It is developed in the open at **microsoft/WindowsML**
(https://github.com/microsoft/WindowsML); that repo's releases are pointers only —
the release notes link to nuget.org and carry no binary assets.

Facts verified against the packages themselves (downloaded from nuget.org's flat
container and inspected locally, Oct 15, 2026):

- The server exists **only in the experimental line**: package **2.7.2021-experimental**
  (published Oct 7, 2026, announced the same day in Microsoft's Foundry on Windows
  devblog, "AI Development on Windows: from PyTorch and llama.cpp to Windows ML",
  https://devblogs.microsoft.com/foundry-on-windows/build-on-winml-oct-7-26/).
  The latest stable, **2.4.89** (Sep 22, 2026), contains **zero** WinMLServer files
  (verified by archive listing). Anyone pinning "latest stable" gets no server.
- Inside 2.7.2021-experimental, `runtimes/win-x64/native/` holds the full co-located
  runtime set the exe needs (see §3). `WinMLServer.exe` FileVersion is
  `2.7.2021.6134`, ProductVersion `2.7.2021-experimental+17f60685e2`.
- A second package is required to serve GGUF models:
  **`Microsoft.Windows.AI.MachineLearning.LibLlama.Core` 0.5.0.2022-experimental**
  (depends on exactly `[2.7.2021-experimental]` of the runtime package). It carries
  the llama.cpp adapter (`WinMLRuntimeLlama.dll`, `WinMLllama.dll`, ggml DLLs) and
  the **CPU backend only** (`LlamaBackends/cpu/ggml-cpu.dll`). Its nuspec states a
  separate LibLlama *backend* package is needed for GPU acceleration; the exact
  backend package ids were not verified in this pass (a guessed
  `…LibLlama.Vulkan` id returns 404 on nuget.org) — resolve from the package
  family at implementation time.
- Other channels, for completeness:
  - `Microsoft.WindowsAppSDK.ML` is the framework-dependent flavor of the same
    runtime (needs the Windows App SDK runtime provisioned on the machine).
    Microsoft Learn ("Install and deploy Windows ML",
    https://learn.microsoft.com/en-us/windows/ai/new-windows-ml/distributing-your-app)
    recommends framework-dependent generally, but the self-contained package is
    the right pin for the wizard: no machine-level runtime prerequisite.
  - The Windows ML *framework runtime* can be provisioned via winget/bootstrap
    (per Microsoft's sample repos), but that is a deployment mode for apps using
    the APIs — **no winget package delivers `WinMLServer.exe`**, and no optional
    Windows component or Microsoft Download Center installer for the server was
    found in any primary source.

## 2. Version, artifact format, size, hashes

Artifact format: **.nupkg** (a zip). No MSI/MSIX/EXE installer exists. Microsoft
publishes **no SHA-256** for these packages; as with the EDB PostgreSQL zip, the
app must compute and pin its own. Measured Oct 15, 2026:

| Artifact | Download URL (nuget.org flat container) | Size (bytes) | SHA-256 (self-computed) |
|---|---|---|---|
| Microsoft.Windows.AI.MachineLearning 2.7.2021-experimental | https://api.nuget.org/v3-flatcontainer/microsoft.windows.ai.machinelearning/2.7.2021-experimental/microsoft.windows.ai.machinelearning.2.7.2021-experimental.nupkg | 60,767,134 | `e14ddfe2fa9f0d13fd9047d18c7e58e5801d293235b6e75fdb3ece4ef404484f` |
| Microsoft.Windows.AI.MachineLearning.LibLlama.Core 0.5.0.2022-experimental | https://api.nuget.org/v3-flatcontainer/microsoft.windows.ai.machinelearning.libllama.core/0.5.0.2022-experimental/microsoft.windows.ai.machinelearning.libllama.core.0.5.0.2022-experimental.nupkg | 7,001,064 | `edf1ff58afdc41d75c7fe3e0f752ab8b6e1c86d81aaba7ee5b8dcfe75124e907` |

Inner binaries (win-x64), from the extracted runtime package:

| File | Size (bytes) | SHA-256 |
|---|---|---|
| WinMLServer.exe | 380,592 | `85dd3a20a9c499122d5951b0fafda76a8d58e7c0a3945481abc9915a44665ad3` |
| WinMLServer.dll | 767,960 | `bc802b0b4bc39bc9de0a5b6dfc3976081eb7c229e8b30a00fd2f5397c779cbef` |

Extracted footprint: the runtime package's win-x64 native folder is 8 files /
49,777,192 bytes (includes `onnxruntime.dll` 24.3 MB and `DirectML.dll` 18.7 MB);
LibLlama.Core's win-x64 natives add ≈ 8.9 MB. Total on-disk budget ≈ **59 MB**,
plus the GGUF model (already downloaded by the app's models step).

## 3. Install mechanics

- **There is no installer to run.** "Install" = download both .nupkgs, verify
  SHA-256 fail-closed, extract `runtimes/win-x64/native/**` from both into ONE
  folder (e.g. `<data_dir>/winml/bin/x64/`, exactly the MXC layout), and run
  `WinMLServer.exe` from that folder. The folder must stay whole: the exe imports
  `WinMLRuntimeCore.dll`, `WinMLTokenizer.dll`, `WinMLTasks.dll` directly, and
  the server/tasks DLLs load `WinMLServer.dll`, `Microsoft.Windows.AI.MachineLearning.dll`,
  `onnxruntime.dll`, `DirectML.dll` from the same directory; LibLlama's DLLs and
  its `LlamaBackends/` subfolder likewise.
- **Admin: not required.** The exe's embedded manifest requests `asInvoker`.
  Everything lives in the per-user data dir. No PATH changes, no registry, no UAC
  (contrast the MXC host-prep step, which needed one prompt).
- **Runtime prerequisites:** for the self-contained package, none beyond the OS —
  no Windows App Runtime install (that is the framework-dependent flavor's
  requirement), and the exe links only the Universal CRT (`api-ms-win-crt-*`),
  which is inbox on Windows 10+. (Not verified on a bare machine; a missing-CRT
  failure would present as a spawn error and should be a step diagnostic.)
- **OS prerequisite:** Windows ML's current platform targets **Windows 11 24H2
  (build 26100) or later** — stated in Microsoft's Windows ML samples'
  prerequisites and in the execution-provider docs (downloadable EPs require
  24H2). The package nuspec's generic runtime floor is Windows 10 19H1 (18362),
  but the Task/Server/GGUF surface is documented and sampled only against
  24H2+. The wizard step should gate on build ≥ 26100 and report the actual
  build when it refuses.
- Nothing lands in `C:\Program Files\WindowsML` — no primary source mentions
  that path. See §5.

## 4. Server contract

Primary sources: microsoft/WindowsML `docs/api-reference/IWinMLServer.md`,
`Samples/Server/README.md`, `docs/Server/README.md`, the devblog example, and
strings/imports read from the shipped `WinMLServer.exe` itself.

- **Two hosts, one contract.** Apps can host `WinMLServer.dll` in-process via the
  COM-style `IWinMLServer` interface; the package also ships `WinMLServer.exe`,
  described by Microsoft as the development/testing host, which "loads a model
  file and serves it from the command line." Both serve the same endpoint.
- **Starting the exe:** positional model artifact path (or `--recipe <file>`),
  with flags confirmed from the binary: `--model-id`, `--port`, `--target`
  (`auto`|`cpu`|`gpu`|`npu`), `--target-preference` (`default`|`performance`|
  `efficiency`), `--concurrency`, `--context-tokens`, `--max-output-tokens`,
  `--tokenizer-source`/`--tokenizer-path`, `--mmproj`, `--draft-model`,
  `--speculative`, `--queue-capacity`, `--queue-wait-timeout-ms`,
  `--max-request-body-bytes`, `--max-image-bytes`, `--reasoning-efforts`,
  `--diagnostics`, `--print-recipe`. Microsoft's published example:
  `WinMLServer.exe model.gguf --model-id qwen2.5-0.5b --target gpu --port 8080`.
- **Stdout protocol (machine-parseable, from the binary):**
  - `WINMLSERVER_ACCESS_KEY <key>`
  - `WINMLSERVER_READY http://127.0.0.1:<port>/v1`
  - `WINMLSERVER_MODEL model=<id> composition=… context=… target=… lanes=…`
  - failures as `WINMLSERVER_STEP_FAILED step=… error=0x…` /
    `WinMLServer configuration error: …`
  A supervisor should pass `--port` explicitly (the in-process API's default
  port is 0 = pick a free port; no documented exe default was found) and parse
  the READY and ACCESS_KEY lines from captured stdout — the app already captures
  supervised-process output, so this fits.
- **Endpoints:** exactly two routes — `GET /v1/models` and
  `POST /v1/chat/completions` (streaming via SSE supported; OpenAI error shape).
  Model list entries carry the OpenAI fields plus a `winml` object
  (context window, capabilities, lane count).
- **Authentication — the critical contract point.** Every start mints a new
  random access key (64 lowercase hex chars); the host cannot choose it. Every
  request must send `Authorization: Bearer <key>` or gets **401
  `invalid_api_key`** before the body is read. Additional request gates: source
  must be loopback, `Host` must be `127.0.0.1:<port>` / `localhost:<port>` /
  `[::1]:<port>`, and any `Origin` header is rejected (403). The key dies when
  the server stops. Microsoft's guidance: hand the key to same-user clients via
  environment, never on a command line; connect directly, never through a proxy
  that would log it.
- **Model selection/serving:** the exe serves **one model artifact per process**
  under the `--model-id` it was launched with. It does not download models and
  there is no catalog step for this host — the model is a local file path
  (GGUF directly usable; the app's already-downloaded qwen3-8b GGUF is the
  input). The tokenizer defaults to the model's own. If the model declares no
  context window, launch fails unless `--context-tokens` is passed (error text
  in the binary) — the wizard should pass it from the app's model config.
  The in-process DLL host can register up to 8 model ids with multiple lanes
  each; that is not reachable through the exe.
- **Limits worth knowing** (defaults from `IWinMLServer` docs): max completion
  tokens 4096, request body 1 MiB, max active requests 4, per-model queue 8.
- **Experimental status, from primary sources:** the devblog calls the llama.cpp
  integration and the Runtime API "experimental"/"preview" throughout and warns
  the capabilities "are experimental"; the package version literally carries
  `-experimental`; the samples README states the Server, Runtime, and Task APIs
  "are new and still evolving, and they may change." (The Windows ML platform
  itself went GA in Sept 2025 — that GA does not cover this GGUF/server surface,
  which arrived Oct 7, 2026.) Contract churn between pins should be expected.

## 5. App assumptions vs. reality

| App assumption (current code) | Reality | Verdict |
|---|---|---|
| Exe at `C:\Program Files\WindowsML\WinMLServer.exe` (`manager-winml::default_server_exe_path`) | No such install location exists. The exe ships inside a NuGet package and runs from wherever the package's native folder is extracted | **Wrong — replace with `<data_dir>/winml/bin/x64/WinMLServer.exe`** (MXC pattern) |
| Probe `GET /v1/models` with no credentials, expect 200 (`ReqwestProbeTransport`, wizard `winml-register` step) | Real server returns **401** without `Authorization: Bearer <per-start key>`; the step would report "WinMLServer unhealthy: HTTP 401" against a healthy server | **Wrong — probe must carry the current key**, which the app only knows if it launched (or otherwise obtained) the server; probing an externally-started server is not possible without its key |
| Port 8090 as "the WinMLServer port" | 8090 is purely the app's own convention (spec §6.5). The server takes any `--port`; Microsoft's example uses 8080, and the API default is 0 (ephemeral) | **Harmless if the app launches the exe with `--port 8090`**; meaningless for a server started by anyone else |
| Gateway registration: model `openai/winml`, api_key placeholder `local` (`manager-gateway`, tool-runner group) | LiteLLM will send model name `winml` and key `local` upstream: the server 404s unknown model ids (`model_not_found`) and 401s the key | **Wrong twice — the deployment's model must be the id the exe was launched with (`--model-id`, i.e. the configured `winml.model_id` = qwen3-8b), and its api_key must be the live per-start key** |
| Model eligibility: `model_id` must be set, ≤14B class (qwen3-8b approved) | Compatible: the exe serves the app's GGUF file directly; qwen3-8b is in the size class the GGUF path targets | OK |
| "The app does not install or start WinMLServer" (step remediation: "start WinMLServer, then re-run winml-register") | The approved design closes exactly this gap | Superseded by the new step (below) |

### Design consequences for the wizard step (for the build)

1. **New install step mirrors install-mxc**: pins (table in §2) → download to
   `<data_dir>/winml/downloads/` → SHA-256 fail-closed → extract both packages'
   `runtimes/win-x64/native/**` whole into `<data_dir>/winml/bin/x64/` → marker
   file. Audit = marker + exe/dll hashes. No elevation anywhere.
2. **Lifecycle coupling is mandatory, not optional.** Because the key rotates
   every start and cannot be chosen, the app must: start the exe itself
   (supervised, like a backend) with the model's GGUF path, `--model-id
   <winml.model_id>`, `--port <winml.port>`, `--target` from config; parse
   `WINMLSERVER_ACCESS_KEY` / `WINMLSERVER_READY` from its stdout; then write
   the gateway config with that key as the winml deployment's api_key (held in
   memory / credential store, never in the YAML as a literal — use the same
   env-injection pattern as the OpenRouter and master keys if LiteLLM config
   allows, else regenerate + restart the gateway on every WinML restart).
   A WinML server restart therefore implies a gateway config refresh.
3. **Probe fix:** `winml-register`/health probes must send the Bearer key.
   A keyless 401 should be read as "server present, key unknown/stale," not
   "unhealthy."
4. **Gateway emission fix:** model `openai/<winml.model_id>`, api_key = live
   key (see 2). Model id and model file both come from `winml.model_id`'s
   catalog entry.
5. **GGUF GPU caveat:** with only LibLlama.Core pinned, GGUF runs on the CPU
   backend. `--target gpu` needs an additional LibLlama backend package
   (family existence confirmed by the Core nuspec; exact ids unverified).
   Ship CPU-first and treat GPU as a follow-up pin, or verify the backend
   package ids before promising `--target gpu`.
6. **OS gate:** refuse with a clear message below Windows build 26100.

## 6. Licensing / terms

- The runtime package is under Microsoft's "Microsoft Windows ML Runtime"
  license terms (the `license.txt` inside the nupkg; `requireLicenseAcceptance`
  is true on nuget.org). It is **not** MIT — the MIT license on the GitHub repo
  covers the repo's code/samples, not these binaries.
- The terms' Distributable Code section expressly permits redistributing the
  files the NuGet package binplaces with an application (self-contained
  deployment), with conditions (add significant primary functionality, pass
  protective terms to end users, indemnity). The wizard's model — download from
  Microsoft at install time, run unmodified, never bundle the binaries in the
  app's own installer — is the most conservative reading of that grant. Because
  `requireLicenseAcceptance` is set, the wizard step should surface the license
  (link/file) as part of the install, the way a NuGet restore would.
- Execution providers and the llama.cpp components carry third-party licenses
  (see `ThirdPartyNotices.WinMLServer.txt` in the package and
  https://aka.ms/winml-licenses); llama.cpp itself is MIT. Nothing found
  prohibits the download-at-install-time pattern.

## Sources (all accessed Oct 15, 2026)

- microsoft/WindowsML repo, README + releases: https://github.com/microsoft/WindowsML ,
  https://github.com/microsoft/WindowsML/releases (release v2.7.2021-experimental, Oct 7, 2026)
- Server docs: https://github.com/microsoft/WindowsML/blob/main/docs/Server/README.md ,
  https://github.com/microsoft/WindowsML/blob/main/docs/api-reference/IWinMLServer.md ,
  https://github.com/microsoft/WindowsML/blob/main/Samples/Server/README.md
- Microsoft devblog (Oct 7, 2026): https://devblogs.microsoft.com/foundry-on-windows/build-on-winml-oct-7-26/
- Microsoft Learn, Install and deploy Windows ML:
  https://learn.microsoft.com/en-us/windows/ai/new-windows-ml/distributing-your-app ;
  Windows ML execution providers (24H2 requirement):
  https://learn.microsoft.com/en-us/windows/ai/new-windows-ml/supported-execution-providers
- NuGet packages (downloaded and inspected; hashes in §2):
  https://www.nuget.org/packages/Microsoft.Windows.AI.MachineLearning/2.7.2021-experimental ,
  https://www.nuget.org/packages/Microsoft.Windows.AI.MachineLearning.LibLlama.Core/0.5.0.2022-experimental ,
  stable comparison: …/Microsoft.Windows.AI.MachineLearning/2.4.89
- Local artifacts retained under `~/workspace/.tmp-assets/winml-probe/`
  (both .nupkgs; extracted x64 natives of the runtime package in `ml2721/`).
