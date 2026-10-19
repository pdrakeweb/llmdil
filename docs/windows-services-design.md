# Windows Services supervision — design note

Branch: `feature/windows-services` (from v0.1.32, commit 99ee351).
Status: increment 1 implemented (see "Increment 1 status" at the end)
and merged to main; shipped in releases from v0.1.36, present in
v0.1.40. The original gate — do not land until Pete has tested it on
Prometheus — was superseded by the merge; what remains gated is
**reliance**: session-0 GPU validation (§4a) is still an on-machine
item, so service mode ships available but not yet validated for GPU
backends. The deferred list at the end is current as of v0.1.40:
no installer bundling of `llm-service-host.exe` (no `externalBin`
entry in `src-tauri/tauri.conf.json`), PostgreSQL is still started
process-managed via `pg_ctl` before the gateway service, start mode
is still Manual (`ServiceStartMode::Manual` in
`crates/manager-services`), and there is no boot-start.
Pete's directive (2026-10-17): every backend and the gateway should be
registered as Windows system services; the Service Manager starts/stops
those services; when configuration changes, the service definition is
updated as needed. The feature is independently testable and must NOT
land on main until Pete has tested it on Prometheus.

## 1. Service host: a native Rust shim in this repo

A Windows service's binary must speak the Service Control Manager (SCM)
protocol (`StartServiceCtrlDispatcher`). Neither `llama-server.exe` nor
the gateway's `pythonw.exe -c …` bootstrap does, so registering the raw
executables with `sc.exe` fails at start with error 1053. A wrapper is
mandatory. Options:

- **WinSW** — mature, per-service XML config, built-in log redirection
  and failure actions. Cost: an external binary to download, version,
  and ship per service; its XML becomes a second definition format this
  codebase doesn't control; .NET/native variants add installer weight.
- **NSSM** — same shape as WinSW, effectively unmaintained.
- **A native shim in-repo (chosen)** — one small binary,
  `llm-service-host.exe`, built from `crates/manager-services` with
  hand-rolled `windows-sys` FFI, the same style the codebase already
  uses for PDH, Credential Manager, and elevation. Every managed
  service's ImagePath is this shim plus a pointer to a definition file;
  the shim registers the control handler, spawns the real child,
  redirects its stdio to the definition's log files, reports status to
  SCM, and terminates the child's whole process tree on stop (the child
  is assigned to a kill-on-close job object at spawn). One binary,
  every service, no external dependency, and the definition format is
  ours (JSON, versioned, unit-testable on Linux CI).

SCM management from the app uses the same `windows-sys` surface
(`OpenSCManagerW`, `CreateServiceW`, `StartServiceW`, `ControlService`,
`QueryServiceStatusEx`, …) — no new crates beyond the `windows-sys`
version the workspace already carries.

### The definition-file indirection (the load-bearing decision)

A service's registered ImagePath never changes after registration:

```
"<app>\llm-service-host.exe" run --definition "<data_dir>\services\<service-name>.json"
```

Everything that varies — program, args, env, working directory, log
paths — lives in that JSON definition file, which the app owns and may
rewrite at any time **without elevation** (it is a plain file in the
app data dir). Consequences:

- "Update the service definition when configuration changes" is, in
  the common case, just regenerating a JSON file. No SCM call, no UAC
  prompt, no service re-registration.
- SCM mutations are needed only for: create/delete (explicit,
  elevated, rare), start/stop (granted to the operator account, see
  §5), and changes to account or failure policy (elevated, rare).
- The shim reads the definition at service start. A running service
  keeps the definition it started with; the new definition takes
  effect at its next start — exactly the Activation Plan's existing
  semantics for process supervision ("stopped processes take changes
  at next start; running ones restart to apply").
- The definition format is pure data, so `spec → definition` is a pure
  function with unit tests that run on Linux CI (where the real SCM
  code is `cfg(windows)`-gated).

## 2. Per-service definitions

### Naming

- Backend `<id>` → service name `llm-backend-<id>`
  (display name `LLM Backend <id>`, description names the model file
  and port).
- Gateway → service name `llm-gateway` (display name
  `LLM Gateway (LiteLLM)`).

Names are stable across definition rewrites; the backend id is the
same id the supervisor and config already use.

### Command line and environment — from the same spec generation

Definitions are generated from the exact objects process supervision
spawns today; there is no second flag-construction path:

- Backends: from `backend_spec()`'s `ProcessSpec`
  (now `crates/manager-app/src/lib.rs`; it lived in
  `src-tauri/src/main.rs` when this note was written) — program (`<data_dir>\llamacpp\llama-server.exe`),
  args (model, port, context, batch, tensor split remapped onto placed
  GPUs, raw-flag filtering), env (`CUDA_VISIBLE_DEVICES` with GPU
  UUIDs), port. `manager_services::backend_definition(&ProcessSpec)`
  converts it 1:1.
- Gateway: from the start-gateway step's `PlannedCommand`
  (`spawn_command` + `spawn_command_with_env` in
  `crates/manager-wizard/src/steps.rs`) — argv is the venv
  `pythonw.exe -c <bootstrap> --config <data_dir>\gateway\config.yaml
  --host <bind> --port <port>`; env is transformed per §4 (secrets
  become credential references, never literals).

### Working directory

Backends: none today (the supervisor's `ProcessSpec.workdir` is
`None`); the definition inherits that. Gateway: the PlannedCommand's
cwd (unset today). Definitions carry the field so a future spec change
flows through.

### Logging — the Logs view keeps working

- Gateway: unchanged. The definition's stdout/stderr paths are the
  same files the detached spawn already writes
  (`<data_dir>\logs\gateway.stdout.log` / `gateway.stderr.log`), and
  the shim appends to them. Everything that tails those files (step
  failure diagnostics, the diagnostics bundle) works as before.
- Backends: today the supervisor captures the child's pipes into an
  in-memory ring — impossible for a process the app didn't spawn.
  Under service supervision the shim redirects the child's stdout and
  stderr to `<data_dir>\logs\backend-<id>.stdout.log` and
  `backend-<id>.stderr.log`. **Deferred in increment 1:** teaching the
  Logs view's backend scopes to read those files (today they read the
  supervisor rings, which stay empty for service-supervised backends).
  The files exist and are in the diagnostics bundle from day one; the
  Logs-view wiring is increment 2 (see status list).

### Failure / restart policy

SCM recovery actions on every managed service: restart after 10 s on
first failure, restart after 30 s on second, restart after 60 s on
subsequent failures, failure counter resets after 1 hour. This is
deliberately weaker than the process supervisor's crash-loop backoff
(the supervisor tracks attempts and gives up); SCM has no give-up
state, so SCM restart is unconditional in increment 1. A flapping
service is visible as Running→Stopped churn in the Backends view
through the SCM status merge; backoff tuning is a field-test
question.

### Start mode: Manual

All managed services register with **Manual** start. Reasons:

- The app's lifecycle model is "the manager starts what should run":
  enabled backends at the operator's intent, the gateway through the
  start-gateway step / activation apply. `Automatic` would race that
  model at boot (services up before the app, ports held before
  adoption logic runs, an `enabled=false` edit away from a fight).
- `disabled` in this app means "not part of the running set", not
  "uninstall". A disabled backend keeps its registration (Manual,
  stopped); the app simply never starts it, and Start All / launch
  flows skip it exactly as in process mode.
- Boot persistence is a legitimate future want (LLM stack survives a
  reboot without opening the app) and is then a per-service opt-in:
  flip that service to Automatic (delayed) — a definition-level change
  the sync path can add without restructuring.

## 3. Definition sync and the Activation Plan

Regeneration triggers (mode = service only; process mode never
touches definitions):

- Backend config save paths: add, apply-config (flags), update-flags,
  set-offload, placement apply (which writes through the same save
  machinery), delete (→ the registration is removed by the explicit
  unregister action; delete also attempts an SCM delete when elevated
  rights allow, and reports if it could not — see status list).
- Gateway config changes: the gateway definition is regenerated at
  every start-gateway step run (it is cheap and the step already
  rebuilds the full spawn environment, including which secrets exist),
  and on gateway config save.
- Mode switch process → service: nothing is auto-registered.
  Registration is an explicit per-service action in Settings (§6), so
  the elevation moment is deliberate and the feature is testable one
  service at a time.

Composition with the Activation Plan is by construction: a plan entry
already means "saved config differs from live state; apply = make live
match saved". Under service supervision, apply already routes backend
restarts through the start/stop commands and the gateway through the
start-gateway step — both of which are mode-aware after increment 1 —
and definition regeneration has already happened at save time (for
backends) or happens inside the step (gateway). A running service that
is restarted therefore comes up on the new definition; a stopped one
takes it at next start. No plan-format changes.

## 4. The two hard problems

### (a) Session-0 GPU access — TOP on-machine validation item

Services run in session 0. NVIDIA's own CUDA Installation Guide for
Windows states, of the TCC driver model, that TCC "allows the use of
CUDA from within processes running as Windows services, which is not
possible for WDDM devices"
([CUDA Installation Guide, Windows](https://docs.nvidia.com/cuda/archive/11.2.1/pdf/CUDA_Installation_Guide_Windows.pdf)).
NVIDIA staff later reported that drivers from roughly the r352 series
(2015–2016, CUDA 7.5 era) worked around the limitation and WDDM
devices became usable from session 0 / RDP, without the installation
guide's wording being withdrawn
([NVIDIA developer forum](https://forums.developer.nvidia.com/t/we-are-able-to-runing-cuda-service-with-wddm/41461)).
The two sources disagree across a decade of driver releases; neither
settles Prometheus, whose mix is exactly the interesting case:

- **Tesla P100s — TCC driver model** (no display outputs): documented
  as service-safe. Expected to work.
- **RTX A4000 — WDDM driver model** (the display GPU): officially
  undocumented territory. Workstation-class (Quadro lineage) WDDM
  cards are the ones the forum reports concern, but "reports from
  2016" is not a guarantee for the current driver on this machine.

**This must be settled empirically before service mode is trusted, and
it gates the feature**: if the A4000 is unreachable from session 0,
service supervision is viable only for P100-placed backends (mixed
mode, §6) or not at all.

Validation procedure (on Prometheus, in order):

1. Baseline (process mode): start `planner` (A4000-only). Record
   `nvidia-smi` showing the llama-server pid holding VRAM on the
   A4000, and a successful completion through the gateway.
2. Switch the app to service mode, register only the `planner`
   service, start it from the Backends view. Pass criteria, all of:
   - the service reaches Running and stays up 60 s;
   - the shim's backend log (`logs\backend-planner.stderr.log`)
     shows ggml-cuda device initialization and model load, with no
     "no CUDA-capable devices" / device-enumeration failure;
   - `nvidia-smi` lists the llama-server pid (the shim's child) with
     the model resident on the A4000;
   - the app telemetry attributes VRAM to that pid as before;
   - a completion request through the gateway returns tokens.
3. Repeat 1–2 for a P100-placed backend (`coder`): expected to pass
   per the TCC documentation; a failure here invalidates the shim,
   not the concept.
4. Split placement (a backend spanning A4000 + P100, e.g.
   `coder-nvidia`'s configured 50/50): start as a service; both GPUs
   must show residency. A failure isolated to the A4000 leg confirms
   the WDDM hypothesis with the split as the witness.
5. Record driver version + outcomes in this note's status section
   before any wider rollout.

Failure handling if WDDM fails validation: per-service mode (§6)
keeps A4000-placed backends on process supervision and P100 backends
on services; the app must not silently fall back — the mode is
explicit per service in Settings.

### (b) Secrets: run the services as Pete

The gateway needs two secrets at start: the LiteLLM master key
(Credential Manager target `local-llm-service-manager/litellm-master-key`)
and the Postgres password (target
`local-llm-service-manager/postgres-password`, composed into
`DATABASE_URL` as `postgresql://postgres:<pw>@127.0.0.1:<port>/litellm`),
plus the OpenRouter key when the tier is enabled (target = the tier's
configured `cred_ref`). All three live in the **interactive user's**
Credential Manager vault. A service running as `LocalSystem` has a
different vault and cannot read them; copying them into the service
definition would put live secrets into a JSON file and break the
app's "secrets only in Credential Manager" rule.

Options:

- **Services run as Pete's user account (chosen).**
...[truncated 11456 chars]  The account already owns the credentials, the model files, and
  the data dir; the gateway definition carries only credential
  *references*, and the shim — running as that account — resolves
  them at start. SCM stores the account password as an LSA secret;
  the account needs the "Log on as a service" right, granted during
  the elevated registration. Costs: the password must be re-entered
  when it changes (registration is re-run; the Settings register
  action covers this), and services stop being able to start if the
  password drifts until then. Acceptable for a single-operator
  workstation.
- **Move the secrets to a machine store** (DPAPI machine scope or a
  SYSTEM-owned vault) and run services as SYSTEM: a migration of the
  app's whole secrets model, breaking the invariant that only Pete's
  session can read the master key, for a benefit (no password
  upkeep) that matters on multi-user machines, which this is not.
- **Hybrid** (gateway as Pete, backends as SYSTEM): backends hold no
  secrets today, so this works — but it doubles the account matrix
  for a marginal gain and forecloses per-backend secrets (a future
  authenticated model source) without a re-registration.

**Recommendation: all managed services run as Pete's account in
v1.** The shim resolves credential references at start; a missing
master-key credential fails the gateway start loudly (a gateway
without its key must not come up half-configured), while a missing
Postgres password simply omits `DATABASE_URL` — the same no-database
behavior as process mode without Postgres.

One scoping note: in service mode the start-gateway step still
ensures the app-managed PostgreSQL server is running (process-managed
via `pg_ctl`, as today) before it starts the gateway service, because
the gateway migrates against it at startup. Making Postgres itself a
managed service is a named follow-up, not increment 1.

## 5. Elevation

Registration and unregistration need an administrator token; nothing
else in the steady state does. The flow follows the app's existing
pattern (the wizard's firewall reconciliation, via
`SystemOps::run_elevated` → `ShellExecuteExW` "runas", bounded wait,
decline = a surfaced warning with a manual path — here, the Settings
register action itself):

1. Settings → Supervision, Register on a service row: the app writes
   the current definition, stages the account password in a
   transient credential (target
   `local-llm-service-manager/service-account-password`), and runs
   `llm-service-host register --definition <path>` elevated — one
   UAC prompt.
2. The helper (elevated token, same user) reads and deletes the
   transient credential, creates the service with the account +
   password (SCM stores the password as an LSA secret; the account
   receives "Log on as a service"), sets description + failure
   actions, and sets the service DACL: SYSTEM and Administrators
   full; **the operator's own SID: query, start, stop** — so
   day-to-day lifecycle from the app is unelevated, while
   create/delete/change-config remain admin-only. The helper appends
   its outcome to `<data_dir>/logs/service-manage.log` (an elevated
   process's stderr is invisible to the app) and exits non-zero on
   failure; the app surfaces the exit code and clears the staged
   credential on every path.
3. Unregister is the same shape: `llm-service-host unregister
   --name <name>`, elevated.

## 6. Coexistence: mode setting, per-service in effect

`supervision.mode` in config (`process` | `service`, default
`process`) is the global switch; registration is the per-service
opt-in. Routing rule for every lifecycle call: **service mode AND
the service is registered → SCM; otherwise → the process supervisor,
unchanged.** Consequences:

- Process mode is bit-for-bit the current behavior; nothing about it
  changes when the feature ships.
- Mixed operation is not just allowed, it is the migration path:
  register `coder` (TCC P100), validate, then register the rest;
  A4000 backends can stay on process supervision if §4(a) validation
  fails on WDDM.
- Switching the mode back to `process` stops nothing and unregisters
  nothing; registered services simply stop being routed to (and
  should be stopped from the Backends view first — the app does not
  reach across modes to stop a service it is not currently routing).
- Unregistering returns that backend/gateway to process supervision
  at the next start. Definition files are left in place (harmless,
  and re-registering reuses the path).

## Increment 1 status (2026-10-17)

Implemented in this branch:

- `manager-config`: `supervision.mode` (`process` default) with
  serde-default backfill; round-trip tests.
- `crates/manager-services`: definition types and pure builders
  (backend from the supervisor's `ProcessSpec`; gateway from a
  launch spec with secrets as credential references / a composable
  `DATABASE_URL`), definition file I/O, env resolution with the
  missing-secret rules above, the `ServiceControl` trait, the
  Windows SCM implementation (register with description, failure
  actions, and the operator-SID DACL; start/stop/query mapped onto
  the supervisor's state model), and `llm-service-host` (SCM host:
  control handler, credential resolution, stdio to the definition's
  log files, pid file, kill-on-close job object; plus the elevated
  `register`/`unregister` helper modes). Unit tests for definition
  generation run on Linux.
- App routing (`src-tauri`; the app glue has since moved to
  `crates/manager-app`): backend start/stop/restart route to the
  SCM when mode = service and the service is registered;
  `list_backends` merges SCM state for registered services;
  definitions regenerate on backend add / apply-config / update-flags
  / set-offload / placement apply, on every service-mode
  start-gateway run, and at registration; `gateway_stop` is
  mode-aware; the start-gateway step's spawn seam starts the service
  instead of a detached process in service mode, after stopping the
  previous gateway through the SCM (not the pid file).
- UI: Settings → Supervision card (mode toggle, shim presence,
  service account, per-service registered/state with Register /
  Unregister); Dashboard "Start All Services" (separate addition,
  mode-aware by construction — it calls the same commands);
  launch-routing fix (`setup_completed` reconciliation — separate
  addition, see git history).
- Gates: workspace `cargo check`/`cargo test` (493 tests),
  workspace clippy (0 warnings), `cargo check` inside `src-tauri`
  (passes; 1 pre-existing warning, none new), UI build + smoke all
  pass. Caveat: the Windows-target check of the full dependency
  graph cannot run on this Linux VM (`ring`'s build script needs
  MSVC), so the SCM/shim FFI code was type-checked for
  `x86_64-pc-windows-msvc` via a stand-in harness with the crate's
  own sources, and the first full Windows build + run is Pete's
  on-machine step.

Deferred (explicitly):

- **Session-0 GPU validation (§4a)** — on-machine, gates any real
  reliance on service mode for GPU backends.
- **Logs view backend scopes reading the service log files** — the
  files are written (`logs/backend-<id>.{stdout,stderr}.log`) and
  ship in the diagnostics bundle; the Logs view still reads the
  supervisor rings for backends in increment 1.
- **Graceful drain parity** — a service-mode stop terminates the
  host's job object (whole tree at once); the supervisor's drain
  window (wait for in-flight requests) has no service equivalent
  yet.
- **PostgreSQL as a managed service** — the start-gateway step still
  starts it process-managed before the gateway service (see §4b).
- **Installer bundling of `llm-service-host.exe`** — test builds
  place the cargo-built binary beside the app executable; the
  Settings card shows the expected path and whether it is present.
- Automatic (boot-start) services, per-service mode overrides in
  config, and crash-loop backoff beyond the SCM recovery actions.
