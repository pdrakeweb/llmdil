# Prometheus operator tools

Scripts Pete runs interactively on the Prometheus workstation
(Dell Precision 7865) alongside LLMdil. They are measurement and
snapshot tools for repeatable hardware/configuration experiments —
currently the Phase 0 baseline set. Both scripts are read-only except
for the result files they write, need no admin rights, and run on
Windows PowerShell 5.1 and PowerShell 7.

Why PowerShell: these are operator scripts run by hand on a Windows
machine, next to the app. PowerShell is native there, needs no
install, and talks to WMI/CIM and nvidia-smi directly. LLMdil itself
stays Rust; these scripts are not part of the app.

## Files

| File | Purpose |
|---|---|
| `Invoke-ModelBaseline.ps1` | Benchmarks the planner and/or coder model over a fixed prompt set and records server-reported timings (prompt tok/s, generation tok/s, draft acceptance when speculative decoding is active). |
| `Get-SystemSnapshot.ps1` | One-shot capture of llama-server version, GPU state, DIMM inventory, commit charge, and pagefile. |
| `prompts/` | The benchmark prompt sets: `planner-*.txt` (3 prompts) and `coder-*.txt` (3 prompts). Prompts are fixed on purpose — changing a prompt invalidates comparisons against earlier runs. |

`Invoke-ModelBaseline.ps1` behaviour notes:

- Sends one warm-up request per target (discarded), then `-Runs`
  (default 3) measured runs per prompt.
- Default target URL is the gateway, `http://127.0.0.1:4000/v1`, with
  model names `planner` and `coder`. Gateway model names are gateway
  group names; `-PlannerModel` / `-CoderModel` select which group a
  target measures (for example `-PlannerModel planner-mtp` for the
  Planner-MTP backend's group). Pass a backend's own base URL in
  `-BaseUrl` to measure it directly, bypassing the gateway.
- Default sampling is production: no temperature or seed is sent.
  `-Deterministic` sends temperature 0 and seed 42 (the parity pass).
- Results land in `./baseline-results` (override with `-OutDir`): a
  per-run CSV and a summary JSON with medians per target and prompt.
  `-Label` names the arm; use the labels below so runs stay
  comparable across sessions.
- Coder thermal guard: the P100 previously hit 81 C idle and fell off
  the bus. The script reads the P100 temperature before and after
  every coder run and aborts the coder target at 80 C (override with
  `-AbortTempC`, disable with `-NoThermalGuard`). Partial results are
  still written; the script exits with code 2 when the guard fires.
- Model output is never executed. This is a timings measurement only.

## Examples

Run from the LLMdil repo root. The gateway or the named backend must
already be running; the script says so and stops if it cannot reach
the endpoint.

Baseline capture (run before any change, and after any change):

```powershell
.\tools\prometheus\Get-SystemSnapshot.ps1 -OutFile .\baseline-results\snapshot.json
```

Pre-MTP planner baseline, through the gateway:

```powershell
.\tools\prometheus\Invoke-ModelBaseline.ps1 -Target Planner -Label base-file
```

The same baseline direct to the planner backend (how the 2026-10-17
baseline of 21.8 tok/s was measured; replace the port with the planner
backend's own port from LLMdil):

```powershell
.\tools\prometheus\Invoke-ModelBaseline.ps1 -Target Planner -Label base-file -BaseUrl http://127.0.0.1:<planner-port>/v1
```

Coder baseline:

```powershell
.\tools\prometheus\Invoke-ModelBaseline.ps1 -Target Coder -Label coder-baseline
```

Both targets in one run:

```powershell
.\tools\prometheus\Invoke-ModelBaseline.ps1 -Target Both -Label phase0-baseline
```

MTP A/B arms (per `docs/mtp-enablement-plan.md`): the base arm is
the existing `planner` backend; every MTP arm is the separate
`planner-mtp` backend serving the Planner-MTP model, with its raw
flags edited between arms. Never run both backends at once — stop
one before starting the other (their weights alone exceed the
A4000's 16 GiB). The script only measures and labels.

Through the gateway — this needs a gateway group named
`planner-mtp` with member `planner-mtp` (gateway model names are
group names; a backend gets none by itself):

```powershell
# Base arm: the planner backend as it is
.\tools\prometheus\Invoke-ModelBaseline.ps1 -Target Planner -Label base-file

# Arm: planner-mtp, raw flags "--parallel 1" only (no speculative flags)
.\tools\prometheus\Invoke-ModelBaseline.ps1 -Target Planner -PlannerModel planner-mtp -Label mtp-nospec

# Arms: planner-mtp, raw flags "--parallel 1 --spec-type draft-mtp --spec-draft-n-max 2", then 3, then 5
.\tools\prometheus\Invoke-ModelBaseline.ps1 -Target Planner -PlannerModel planner-mtp -Label mtp-nmax2
.\tools\prometheus\Invoke-ModelBaseline.ps1 -Target Planner -PlannerModel planner-mtp -Label mtp-nmax3
.\tools\prometheus\Invoke-ModelBaseline.ps1 -Target Planner -PlannerModel planner-mtp -Label mtp-nmax5

# Deterministic parity pass for the arm under consideration
.\tools\prometheus\Invoke-ModelBaseline.ps1 -Target Planner -PlannerModel planner-mtp -Label mtp-nmax2-det -Deterministic
```

Direct to the backend under test (bypasses the gateway; use each
backend's own port from LLMdil):

```powershell
# Base arm, direct to the planner backend
.\tools\prometheus\Invoke-ModelBaseline.ps1 -Target Planner -Label base-file -BaseUrl http://127.0.0.1:<planner-port>/v1

# MTP arms, direct to the planner-mtp backend (model name is sent as given; a direct llama-server ignores it)
.\tools\prometheus\Invoke-ModelBaseline.ps1 -Target Planner -PlannerModel planner-mtp -Label mtp-nospec -BaseUrl http://127.0.0.1:<planner-mtp-port>/v1
.\tools\prometheus\Invoke-ModelBaseline.ps1 -Target Planner -PlannerModel planner-mtp -Label mtp-nmax2 -BaseUrl http://127.0.0.1:<planner-mtp-port>/v1
```

Re-running after any change: use the same command and a label naming
the change. The summary JSON records the label, URL, model, sampling
mode, and timestamp, so runs from different days stay comparable.

## Where these scripts live

They live in this repo, under `tools/`, and version with the app —
not in a separate repository. Reason: the scripts encode LLMdil and
llama.cpp specifics (gateway group names, speculative-decoding flag
names, timing field names) that change when the app and its pinned
llama.cpp change; keeping them in the same repo keeps them in step.

Noted future step, not implemented: bundle `tools/` into the Tauri
installer as resources, so the scripts are installed alongside the
LLMdil executable and Pete does not need a repo checkout on the
workstation to run them.
