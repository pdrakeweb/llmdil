# Admin REST API — Implementation Plan

How the admin API gets built: where the server lives, what it is built
on, how it reuses the existing command implementations instead of
duplicating them, and the order the work lands in. Companion documents:
`rest-api.md` (endpoint surface) and `auth-plan.md` (roles, users,
login). This plan is based on the codebase as of v0.1.32.

**Status: implemented.** Phases 0–5 below all landed (Option B:
`manager-app` extraction + `manager-api` hosted in-process);
PLAN.md §9 is the as-built record. Points where the build settled
differently from this draft, for readers using it as a reference:
the admin port is **47821** (D3's 41231 suggestion was not taken;
`AdminApiConfig` default in `manager-config`), bound
0.0.0.0 with the firewall rule scoped to `localsubnet` +
`100.64.0.0/10`; the identity document is
`GET /api/v1/server` (not `/v1/server`); enrollment exchanges the
one-time code for a standard per-device **session** (access +
rotating refresh), not a standalone device token; and the shipped
registry has grown past the 82 commands this plan counts — 108
operations as of v0.1.40 (66 routed, 42 explicit exclusions;
rest-api.md §6.16).

## Codebase facts the plan rests on

- **All domain logic already lives in workspace crates.** Supervisor
  (`manager-supervisor`, 2,643 lines), wizard + steps
  (`manager-wizard`, 13,674), config (`manager-config`), telemetry
  (`manager-telemetry`), gateway config generation
  (`manager-gateway`), models (`manager-models`), MXC
  (`manager-mxc`) — none of it depends on Tauri.
- **The Tauri layer is one binary file.** `src-tauri/src/main.rs` is
  6,406 lines holding `AppState`, all 82 `#[tauri::command]` handlers,
  and the glue helpers they share (`backend_spec`, preflights,
  activation wiring), plus two sibling modules that are already
  framework-free files: `activation.rs` (the scoped activation
  journal) and `gateway_stats.rs` (the LiteLLM metrics sampler).
  src-tauri is a **binary crate excluded from the workspace** — no
  other crate can import anything from it today.
- **Commands are thin over `AppState`.** The pattern is uniform:
  `async fn cmd(state: State<'_, AppState>, args) -> Result<Value, String>`,
  where the body calls lazy accessors on `AppState`
  (`supervisor()`, `telemetry()`, `wizard(mode)`, `wizard_ctx(mode)`)
  and shared helpers. Tauri coupling inside the bodies is limited to
  two things: the `State` extractor (mechanical) and
  `AppHandle::emit` in 7 commands (backend status, telemetry update,
  wizard progress, download progress). `AppState` itself stores no
  `AppHandle` — it is constructed with
  `AppState::new(config, config_path, data_dir)` and handed to
  `app.manage(...)`.
- **Errors are already structured internally, flattened at the
  boundary.** Command bodies use a typed `ApiError` (variants:
  Config, Supervisor, Wizard, Telemetry, Gateway, Models, Mxc, Mcp,
  WinMl, NotFound, InvalidInput), then flatten to `String` because
  Tauri invoke wants a string. The REST layer can consume the typed
  form the UI throws away.
- **State ownership is single-process by design.** One `Supervisor`
  behind one `Mutex`; one telemetry store with a single-writer poll
  loop; config saved through `manager_config::save` (validate, atomic
  write, `.bak` rotation) and installed in memory only on success;
  one activation journal file under the data dir. Two processes
  owning these concurrently would diverge or corrupt — this fact
  decides the hosting question below.
- **The app is not headless today.** No tray icon, no
  minimize/close-to-tray, no autostart, no single-instance guard.
  Closing the window exits the process and everything it supervises'
  control plane with it. The installer is NSIS `currentUser`; there
  is no Windows service and no service identity.
- **Secrets live in Windows Credential Manager** via raw
  `CredReadW`/`CredWriteW` FFI in `manager-wizard/src/ops.rs`
  (gateway master key, Postgres password), under the interactive
  user's account. One existing command, `gateway_master_key`,
  returns the master key value itself to the UI.
- **Test culture is hermetic.** 473 tests across the workspace
  crates, built on injectable seams — the wizard's `SystemOps` trait
  with `FakeSystemOps` recording every effect. CI runs
  `cargo test --workspace` on Linux; src-tauri compiles only on
  Windows (checked inside `src-tauri/` per the standing rule), which
  makes workspace placement of new code a test-coverage decision,
  not just an organizational one.
- **No HTTP server exists anywhere in the codebase today.**
  `tokio` (full) and `reqwest` (rustls) are already workspace
  dependencies; no web framework, no password-hashing crate
  (`pbkdf2` appears only as a transitive dependency).

## Where the server lives

### Option A — Inside the Tauri binary, no new crate

Add an HTTP server module under `src-tauri/src/`, spawned from the
existing `setup()` hook alongside the gateway-metrics poller, sharing
the managed `AppState`.

- Pro: zero restructuring; one process owns all state; ships in the
  existing installer unchanged.
- Con: the code lands in the one crate that cannot be tested on
  Linux CI and does not compile on the dev VM. Every handler test,
  auth test, and contract test would run only on the Windows CI —
  the exact blind spot that let a Windows-only compile error ship
  in v0.1.1. The 6,406-line binary file grows a second front door
  with no crate boundary to hold it.

### Option B — New workspace crate `manager-api`, hosted by the app (recommended)

A new workspace crate owns the HTTP server, routing, auth middleware,
and REST handlers. It depends on a **service core** extracted from
`main.rs` (phase 0 below): `AppState`, the command bodies recast as
framework-free operations, `activation.rs`, and `gateway_stats.rs`,
moved into a library crate (working name `manager-app`). `src-tauri`
shrinks to the Tauri adapter: window setup, invoke-handler
registrations that forward to the same operations, and the
`EventSink` implementation that forwards to `AppHandle::emit`.
The Tauri `setup()` hook starts `manager-api`'s server on the
configured admin port when the API is enabled, handing it the same
`Arc<AppState>` the UI uses.

- Pro: single process, single owner of Supervisor/config/telemetry/
  journal — no split-brain. The server and the ops layer are
  workspace crates, so `cargo test --workspace` covers them on every
  Linux CI run. The crate boundary is also the seam a future
  sidecar or service reuses unchanged (option C becomes an
  additional host for the same crates, not a rewrite).
- Con: the phase-0 extraction is real work before the first
  endpoint exists, and it touches every command signature in
  `main.rs` (mechanically — see Reuse strategy).

### Option C — Separate sidecar / service binary

A second executable (its own `[[bin]]` in `manager-api` or a sibling
crate) that loads the same config and runs its own `AppState`.

- Pro: API availability is independent of the desktop UI — survives
  the window closing, can in principle run as a Windows service and
  survive logoff.
- Con: **two `AppState`s means two Supervisors, two telemetry
  stores, two config writers, two activation journals** pointed at
  one machine. The supervisor adopts already-running backends by
  PID/port probing, so a second supervisor would fight the first
  over the same processes; config saves would race (last writer
  wins, `.bak` churn); `wizard_step_running` and the task/download
  registries are per-process memory. Making this correct is a
  state-ownership redesign (a real daemon the UI also talks to),
  not an incremental step.
- Con (service form): a Windows service runs in session 0, by
  default as SYSTEM. The managed stack assumes the interactive
  user: Credential Manager secrets are the user's, the elevated
  firewall step prompts in-session, backends run as the user. A
  service host forces new decisions on identity, secret storage,
  and elevation that have nothing to do with the API itself.

**Recommendation: Option B now; Option C stays open by
construction.** If logoff-survival later becomes a requirement, the
move is to re-host `manager-app` + `manager-api` in a service
binary and demote the Tauri app to a pure client of the same ops
layer — a hosting change, not an API redesign. That path only
exists if phase 0 is done honestly.

### Availability consequence to decide, not discover

Under option B the API is up exactly when the app process is up.
Today closing the window kills the process. When the API is
enabled, the app should therefore **close to tray** (tray icon,
window hidden, process — and API — alive; explicit Exit in the tray
menu). This is a small, contained change to `main.rs` window
handling plus a tray icon, and it makes "the app is running" mean
what an admin expects. Running while logged off is explicitly out
of scope for v1 (see decision D1).

## Stack

- **Framework: `axum`** on the existing tokio runtime. It is the
  tokio-native router, its extractor/middleware model maps cleanly
  onto the auth design (per-route role guards as tower layers), and
  handler tests run in-process via `tower::ServiceExt::oneshot` —
  no socket needed, matching the repo's hermetic test style.
  Alternatives considered: `actix-web` (own runtime model, heavier,
  no benefit here), `warp` (filter composition ages poorly at this
  surface size), hand-rolled `hyper` (unjustifiable plumbing for
  ~80 operations).
- **JSON: `serde_json`** — already the boundary format in both
  directions (commands take/return `Value`), so REST payloads and
  UI payloads are the same values from the same functions.
- **TLS: in-process rustls** (`axum-server` with rustls, or
  `tokio-rustls` directly). The stack is already rustls-based
  (`reqwest` rustls feature); no OpenSSL dependency enters the
  build. On first enable, the app generates a self-signed
  certificate for the machine, stores the key in Credential Manager
  beside the other managed secrets, and exposes the certificate
  fingerprint in the desktop UI for client pinning (see Android
  constraints). Plain HTTP is offered only on loopback and only as
  an explicit developer opt-in — never as the LAN default: this API
  starts processes and mutates configuration, and it carries login
  credentials.
- **Middleware** (tower layers, outermost first): request-id +
  tracing, body size limit, timeout, auth (session/device token →
  principal), role guard per route group, audit logging of
  mutations to the app log with the acting user id.
- **Configuration**: a new `admin_api` section in `AppConfig`
  (`enabled: bool` default false, `port: u16`, bind derived from
  enabled + the same all-interfaces posture as the gateway),
  following the existing per-section config pattern
  (`postgres`, `notifications`, …). Enabling/disabling the listener
  and reconciling its Windows firewall rule follows the established
  precedent of the start-gateway step's `netsh advfirewall` rule
  management (one elevation prompt when the rule state changes).

## Reuse strategy

The API must not become a second implementation of the app's
operations. The seams, as they exist in the code today:

1. **Operations extraction (phase 0).** Move `AppState` and its
   `impl` (task registry, download registry, lazy supervisor/
   telemetry/wizard accessors), `activation.rs`, and
   `gateway_stats.rs` into the `manager-app` lib crate. Each
   command body becomes a free async function
   `op_xxx(&AppState, args…) -> Result<Value, ApiError>` in that
   crate. The move is mechanical: bodies already receive everything
   they need through `AppState` and arguments.
2. **The `EventSink` seam.** The 7 commands that call
   `app.emit(...)` take an additional `&dyn EventSink` parameter in
   their ops form. Tauri's sink forwards to `AppHandle::emit`
   (identical UI behavior). The API's sink publishes to an
   in-process broadcast channel, which phase 4 exposes as an
   event stream; until then it can simply record/drop. Progress
   semantics (wizard steps, downloads) are otherwise unchanged —
   they already report through the task registry and the
   `wizard_step_running` slot, both of which live in `AppState`.
3. **Typed errors at the ops layer.** Ops functions return the
   existing `ApiError` instead of the flattened `String`. The Tauri
   adapter keeps flattening exactly as today (zero UI behavior
   change); the REST layer maps variants to status codes
   (NotFound → 404, InvalidInput → 400, Config validation → 422,
   busy/conflict states → 409, the rest → 500 with a sanitized
   message). This also fixes a latent API hazard: today's error
   strings embed internal detail (argv, paths) that must not cross
   the network verbatim — the mapping layer is where sanitization
   lives, once.
4. **Tauri handlers become adapters.** Each of the 82 registrations
   becomes: extract `State`, call the ops function, flatten the
   error. One line of logic per command, verified by the existing
   UI smoke test plus the unchanged `main.rs` unit tests moving
   with the code they test.
5. **Nothing below the ops layer changes.** Supervisor, wizard
   steps (already behind the `SystemOps` trait with
   `FakeSystemOps` for tests), config load/save, telemetry store,
   gateway YAML generation, activation journaling — all called by
   the ops layer exactly as the commands call them today. In
   particular, **API mutations go through the same config-save and
   `activation::record_change` paths as UI mutations**, so a change
   made from a phone and a change made at the desk are
   indistinguishable to the activation plan, the drift check, and
   cancel/restore.
6. **Existing patterns the API inherits rather than invents:**
   long work is already modeled — the task registry
   (`record_task`/`finish_task`, `tasks_list`, `task_abort`) for
   downloads and updates, and busy signaling with a named running
   step for the wizard. The REST surface for long operations is
   "start → 202 + task id → poll task (or event stream)", a direct
   projection of what `AppState` already tracks.

## Phasing

Each phase ends green on its own gates and leaves the app
shippable; the API stays default-off until phase 5.

### Phase 0 — Ops-layer extraction (no new surface)

Move state + command bodies to `manager-app`; introduce
`EventSink` and typed-error returns; Tauri handlers become
adapters. **Gates:** `cargo test --workspace`; `cargo check` +
clippy inside `src-tauri/`; UI build + smoke; a payload-parity
check — for a representative command set, the adapter returns
byte-identical JSON to the pre-extraction handler (golden tests
captured before the move).

### Phase 1 — Server skeleton + read-only endpoints

`manager-api` crate: axum app, config section + listener lifecycle
(start/stop with the app when enabled), TLS with generated
self-signed cert, health/version/capabilities endpoints, and the
read-only surface (status, config view with secrets redacted,
backends, telemetry, gateway status + stats, logs query, models,
activation plan). Interim access control: loopback-only plus the
deployment's first static admin token is **not** shipped — phase 1
binds loopback only, LAN exposure arrives with real auth in
phase 2. **Gates:** handler tests via `oneshot` against an ops
layer backed by `FakeSystemOps`; a live-socket loopback smoke
test; redaction tests proving secret-bearing values (master key,
credential blobs) appear in no response.

### Phase 2 — Auth and users

User store (new `manager-api`-owned persistence under the data
dir — user records with argon2id password hashes; `argon2` is a
new dependency), login/logout, opaque session tokens (hashed at
rest, revocable, expiring), device tokens for paired clients,
role-guard middleware wired per route group, first-admin bootstrap
**from the desktop UI only** (creating the first user is a local
act; there is never an unauthenticated network setup path).
Enabling LAN bind + firewall reconciliation lands here, since
this is the phase that makes the port safe to expose.
**Gates:** auth middleware tests (per-role allow/deny matrix over
every route), login throttling test, token revocation test,
password-hash parameter test; the phase-1 redaction suite still
green.

### Phase 3 — Mutations and lifecycle

Backend start/stop/restart, config patches, backend/model CRUD,
placement apply, offload, activation apply/drain/cancel, gateway
stop and start (start goes through the existing start-gateway
wizard step, inheriting its prisma/watchdog handling and its
busy semantics — a concurrent step yields 409 with the running
step's name and elapsed time, mirroring the UI's busy payload).
Audit logging of every mutation. **Gates:** end-to-end tests over
a fake-backed deployment (pair → login → mutate → observe state →
activation journal contains the entry with the API user as
author); concurrency test proving UI-shaped and API-shaped calls
serialize on the supervisor lock without deadlock.

### Phase 4 — Long-running operations

Downloads (start/pause/resume/cancel), wizard steps beyond
start-gateway, update check/apply — all via the task registry:
202 + task id, task polling endpoint, and an SSE event stream fed
by the `EventSink` broadcast (backend status, download progress,
wizard progress, task progress — the same four event kinds the UI
receives today). **Gates:** task lifecycle tests including abort;
SSE reconnect/resume test; a soak test driving a full model
download through the API against the fake ops layer.

### Phase 5 — Hardening and enablement

Login rate limiting and lockout policy, request limits, token and
certificate rotation, audit-log review surface in the desktop UI,
close-to-tray behavior when the API is enabled, negative-test
sweep (expired/revoked/forged tokens, role escalation attempts,
oversized bodies, malformed JSON), docs (`rest-api.md`,
`auth-plan.md` reconciled against the shipped surface), and the
default-off → opt-in enablement flow in Settings. **Gates:** the
full workspace suite, Windows CI, install-test on the release
build, and a manual on-machine pass (Prometheus) driving the API
from a second machine on the LAN.

### Test strategy (applies across phases)

Mirror the repo's existing patterns rather than adding new ones:
hermetic fakes over live resources (`FakeSystemOps` precedent);
unit tests beside the code (the crates carry 33–161 tests each);
handler tests without sockets (`oneshot`); contract tests that run
one operation through both the Tauri adapter and the REST handler
and compare payloads, so the two front doors cannot drift; golden
payload tests for the phase-0 move; and per-phase live smokes kept
as small as the repo's existing UI smoke.

## Android consumption — constraints on the API design

Forward-looking only; the app build is a later project. What the
API must provide so that client is buildable at all:

- **Discovery without a network protocol.** v1: the desktop UI
  shows the admin URL and a QR code containing URL + certificate
  fingerprint + a one-time enrollment code. The phone scans,
  pins the fingerprint, and exchanges the code for its device
  token. mDNS/Bonjour advertisement is a later enhancement, not a
  v1 dependency — Windows mDNS from Rust is an avoidable risk, and
  the QR flow is also how a user proves physical/local presence at
  enrollment time.
- **Per-device credentials.** Enrollment mints a distinct,
  revocable device token per phone (bound to a user and its
  roles), never a shared secret and never the user's password
  stored on the device. The server lists/revokes devices in the
  desktop UI. On-device storage (Android Keystore) is the client's
  problem; token format and lifetime are the API's: opaque,
  rotatable, with expiry the client can survive by re-pairing
  rather than by holding a refresh credential forever.
- **Multi-deployment by design.** Pete's stated case is "Prometheus
  (or other deployments)" — the client will hold several
  deployments, each with its own URL, pinned fingerprint, and
  device token. The API therefore exposes a `GET /v1/server`
  identity/capabilities document (deployment id, app version, API
  version, enabled feature set) so one client build can
  feature-detect across deployments at different versions.
- **Versioning.** All routes under `/v1/`; additive changes only
  within a version. Timestamps in epoch ms (the app's existing
  convention in logs/downloads), ids are the app's existing string
  ids — no new identifier scheme for the client to translate.
- **Flaky-network tolerance.** Mutations that start work return
  task ids (safe to re-poll after a timeout); read endpoints are
  plain GETs with no session affinity; the event stream is
  resumable-by-repoll — SSE is an optimization, and every fact it
  carries is also reachable by GET, so a phone on lossy Wi-Fi
  degrades to polling instead of breaking.

## Risks and decision points

- **D1 — Availability when the UI is closed / user logged off.**
  v1 (option B + close-to-tray) covers "window closed" but not
  "logged off". A service host covers logoff at the cost of the
  session-0 identity redesign. **Recommendation:** ship v1
  in-process with close-to-tray; treat logoff-survival as a
  separate project that re-hosts the same crates. Pete's call on
  whether phone control must work while nobody is logged in.
- **D2 — TLS posture on the LAN.** Self-signed + fingerprint
  pinning via QR (recommended) vs plain HTTP (rejected above) vs
  a locally-issued CA the phone must install (more moving parts,
  no gain for a single-operator deployment). Pete confirms the
  pinning UX is acceptable.
- **D3 — Admin port.** Needs a fixed default (suggestion: 41231,
  chosen to avoid the gateway's 4000, Postgres 54329, and backend
  8081–8085 range) and is configurable. Bind posture when enabled:
  all interfaces + firewall rule (mirrors the gateway's model)
  vs LAN-subnet-scoped rule. **Recommendation:** all interfaces
  with the firewall rule created only on enable, removed on
  disable — the precedent the start-gateway step already set.
- **D4 — First-admin bootstrap.** Desktop-UI-only creation
  (recommended) vs first-login-claims-admin on the network
  (rejected: any LAN neighbor could claim a fresh deployment
  before Pete does).
- **D5 — Session model.** Opaque server-side tokens with a token
  table (revocable, listable, recommended) vs JWTs (stateless,
  but revocation and per-device control — both required by the
  enrollment design — fight the model).
- **D6 — The master-key endpoint.** `gateway_master_key` returns
  the live LiteLLM master key to the local UI. Mirroring it over
  REST would put the gateway's root credential on the LAN
  surface. **Recommendation:** do not mirror it; the API exposes
  key *presence/rotation* operations only, under the highest role,
  if at all. Pete's call if a client workflow genuinely needs the
  value.
- **D7 — Remote wizard/install operations.** The API can in
  principle run install-class steps (downloads of toolchains,
  Postgres install) that spawn elevated, long operations.
  **Recommendation:** phase 4 includes them only behind the
  highest role, and `updates_apply` (runs installers) stays
  local-UI-only in v1. Pete's call.
- **D8 — Extraction scope.** The honest phase-0 move (all 82
  commands) vs a partial move of only the endpoints v1 needs.
  Partial is faster to first endpoint but leaves two ops
  implementations to drift — the failure mode this plan exists to
  avoid. **Recommendation:** full extraction; it is mechanical and
  golden-tested.
- **Standing risk — LAN attack surface.** Enabling this port puts
  a process-spawning, credential-adjacent control plane on the
  network. The mitigations are structural in this plan
  (default-off, auth before exposure, TLS, audit log, redaction
  tests, no secret-valued endpoints), but the residual risk is
  real and is the reason phases 1–2 are ordered as they are:
  nothing listens beyond loopback until users, roles, and TLS all
  exist.

## Summary

Host a new workspace crate, `manager-api`, inside the existing app
process, on top of a `manager-app` service core extracted from
`main.rs` — one process keeps owning the supervisor, config, and
journal, while the workspace placement keeps the new surface under
Linux CI test coverage. Build order: extract the ops layer (0),
read-only server on loopback (1), auth + users + LAN exposure (2),
mutations (3), long-running operations (4), hardening (5). The
decisions Pete owns: logoff-survival scope (D1), TLS pinning
acceptance (D2), port and bind posture (D3), and whether any
secret-valued or installer-class operation is ever reachable
remotely (D6, D7).
