# Admin API — Architectural Plan

Status: approved 2026-10-16 and implemented; §9 records what
shipped. The plan text below is the approved design as written;
where the build settled or changed a point, §9 (including its
v0.1.40 addendum) is the record — code won wherever the two
disagreed. Approving this document approved the architecture;
implementation followed the phasing in §3.9.

Compiled from the four research documents in this directory, cited
throughout as **[audit]** (`capability-audit.md`), **[rest]**
(`rest-api.md`), **[auth]** (`auth-plan.md`), **[impl]**
(`implementation-plan.md`). Where they disagree, §2 names the
disagreement and gives the compiler's recommendation; §4 collects
every open decision in one list.

---

## 1. Intent

The Service Manager today is controllable only from its own desktop
UI, on the managed machine, by whoever is sitting at it. This
project adds a second front door:

- An **opt-in admin API**: an HTTP server on a dedicated admin port
  on the managed machine, off by default, enabled deliberately from
  the desktop app.
- It exposes **the app's existing capabilities** — status,
  telemetry, backends, models, gateway, placement, setup, logs,
  diagnostics — for both **read and control**, as a REST API under
  `/api/v1`. The API is a faithful projection of what the app
  already does; it invents no new behavior **[rest §1]**.
- Its first consumer is a future **Android app for simplified
  control of Prometheus or other deployments**: check GPU state
  from the couch, restart a crashed backend, apply a saved plan,
  watch a download — without walking to the workstation or opening
  remote desktop. The client is multi-deployment by design: each
  deployment has its own URL, credentials, and pinned certificate
  **[impl, Android constraints]**.
- The API carries **its own auth system**: named users, password
  logins, discrete roles assigned per user, per-device sessions,
  and script tokens. The desktop user is implicitly trusted today;
  the network caller is not, so identity and authorization are part
  of the API itself, not a wrapper around it **[auth §1]**.

Non-goals for v1, as the sources support them:

- **No headless or service operation.** The API lives in the app
  process; it is up when the app is up. Surviving logoff (a Windows
  service host) is a later project that reuses the same crates
  **[impl D1]** — see decision 13.
- **No public-Internet exposure.** The threat model is the LAN and
  the tailnet; the router is not part of the design, and
  port-forwarding the admin port would void it **[auth §6]**.
- **No general-purpose identity platform.** No OAuth server, no
  WebAuthn/passkeys, no SMS or push factors, no email recovery
  **[auth §3.6–3.7]**. One role per user in v1 **[auth §2]**.
- **No streaming requirement.** Every fact is reachable by polling;
  an event stream, if built, is additive **[rest §7.7]**.
- **No desktop-session actions.** `open_url` and `open_folder` act
  on the managed machine's interactive desktop and get no
  endpoints **[rest §7]**.
- **No new app behavior.** Capabilities that exist only inside
  workspace crates with no command (catalog management, the model
  fit check, the MXC decision point — **[audit §13.2]**) stay out
  of v1; the API mirrors the command surface.

---

## 2. Approaches

### 2.1 Server hosting

| Option | Shape | Tradeoff |
|---|---|---|
| A — inside `src-tauri` | HTTP module in the binary crate, sharing `AppState` | Zero restructuring, one process owns all state. But the code lands in the one crate Linux CI cannot compile or test — the blind spot that shipped the v0.1.1 Windows-only error — and the 6,406-line `main.rs` grows a second front door with no boundary **[impl]**. |
| B — new `manager-api` crate, hosted by the app (**recommended**) | Workspace crate owns server, routing, auth; depends on a service core (`manager-app`) extracted from `main.rs`; Tauri `setup()` starts it in-process with the same `Arc<AppState>` | Single process, single owner of supervisor/config/telemetry/journal — no split-brain. Workspace placement puts handlers, auth, and contract tests under `cargo test --workspace` on every Linux CI run. Cost: the phase-0 extraction (§3.1) is real work before the first endpoint. The crate boundary is also the seam a future service host reuses unchanged. |
| C — sidecar / Windows service | Second executable with its own `AppState` | API survives the window closing and, as a service, logoff. But two `AppState`s means two supervisors, telemetry stores, config writers, and journals pointed at one machine: process fights, config save races, per-process task/download state. Correctness requires a state-ownership redesign, and a service (session 0, SYSTEM identity) breaks the app's assumptions about Credential Manager, in-session UAC prompts, and user-owned backends **[impl]**. |

**Recommendation: B** **[impl]**. State ownership in this codebase
is single-process by design, and B is the only option that respects
that while keeping the new surface under test. C stays open *by
construction*: if logoff-survival is ever required, `manager-app` +
`manager-api` re-host into a service binary and the Tauri app
demotes to a client — a hosting change, not an API redesign.

Consequence to accept with B: when the API is enabled, the app
**closes to tray** instead of exiting, so "the app is running"
means what an admin expects **[impl]**.

### 2.2 Long-running operation model

The candidates were synchronous request/response throughout, vs
an asynchronous operation model. **[rest §3]** decides for
**asynchronous operations**, and **[impl]** concurs by inheriting
the app's existing machinery:

- Anything that can exceed ~30 s — wizard steps (budgets to
  600 s), checksum verification (~17 min for a 46 GiB GGUF),
  downloads (hours), gateway start (prisma install + generate +
  sustained-health confirmation) — returns **202 + an operation
  resource**; the client polls it.
- The operation resource *is* the app's `TaskRecord`, exposed over
  HTTP; the model's busy semantics (one wizard step at a time, one
  download per model) surface as `409 BUSY` with the running item
  named (§3.3).
- Synchronous remains for reads, config writes, and a short list of
  bounded calls (placement propose, routing test, MXC self-test,
  the quiesce wait capped at 120 s, and the backend test request
  with a documented 300 s client guidance timeout) **[rest §3.2]**.

One tension to name: **[rest §7.7]** specifies no streaming in v1,
while **[impl]** phase 4 adds an SSE event stream fed by the
`EventSink` broadcast. These compose rather than conflict — every
fact the stream carries is also reachable by GET, so the stream is
an optimization, never a contract requirement.
**Compiler's recommendation:** the v1 contract is poll-only
(phases 1–3); SSE lands in phase 4 as an additive channel, exactly
as **[impl]** sequences it, without changing **[rest]**'s contract.

### 2.3 AuthN token model

Candidates considered in **[auth §3]**:

| Model | Verdict |
|---|---|
| **Opaque bearer tokens + server-side sessions, refresh rotation** (**recommended**) | Access token 256-bit random, 60 min; refresh token 256-bit, 30 days, **rotated on every use**; presenting a rotated token is treated as theft and revokes the whole per-device session family. Server stores only SHA-256 hashes. Revocation is real because the store is the authority — and on a single-machine server the lookup is free. |
| JWT access tokens | Rejected: self-contained tokens cannot be revoked before expiry without a server-side denylist, which is the session store again with extra steps; JWT's one advantage (third-party stateless verification) does not exist here. **[impl D5]** independently recommends the same opaque model — this axis is settled by agreement. |
| Cookies | Rejected: the client is a native app, not a browser; cookie semantics buy nothing. |
| One long-lived token, no refresh | Rejected: a stolen token becomes a permanent credential; rotation + per-device revocation is what bounds a stolen phone. |
| The LiteLLM master key as API credential | Rejected: different trust domain (inference gateway vs machine administration), cannot be scoped or revoked per client, and already lives in too many places. |

Two documents describe the phone's credential differently, and
they need one reconciliation. **[auth]** models the phone as an
ordinary login: username + password on the device creates a named
per-device session. **[impl]** models pairing: the desktop shows a
QR with URL + certificate fingerprint + a **one-time enrollment
code**, and the phone exchanges the code for its credential —
never the user's password on the device.
**Compiler's recommendation:** adopt both, as one flow. The QR
enrollment code is a login ceremony, not a second credential type:
exchanging it creates a standard per-device session (access +
refresh pair, **[auth]** model) for a user and role chosen at the
desktop, where an authenticated admin is already present. After
enrollment the phone lives on refresh rotation alone; revocation,
reuse detection, and session listing behave identically for
QR-enrolled and password-created sessions. Script clients use the
separate API-token mechanism (**[auth §3.3]**: admin-issued, named,
role-bound, hash-only at rest, shown once, no refresh, no expiry by
default) — not sessions.

### 2.4 Transport and TLS

Candidates in **[auth §5]**, with **[impl]**'s stack view:

1. **Self-signed certificate + client fingerprint pinning
   (recommended).** Generated at enable time; SANs cover hostname,
   LAN IP, Tailscale IP (2-year validity, regenerable from the
   desktop UI); key material in Windows Credential Manager
   **[impl]**. The desktop Remote Access page displays the
   fingerprint and the pairing QR (`{ base_url, cert_sha256 }`);
   the Android app pins the fingerprint from the QR — the QR *is*
   the out-of-band verification channel. A changed fingerprint is
   a hard stop in the client; re-pairing happens at the desk, never
   by tapping through.
2. **Plain HTTP restricted to Tailscale only.** WireGuard encrypts
   the tailnet path, so this is defensible, and it is cheaper. It
   makes the phone depend on Tailscale being up at home, and it
   teaches the client to accept plaintext — one configuration slip
   from LAN exposure. Note the documents differ on the floor:
   **[auth]** would run a plaintext listener on the Tailscale
   interface only; **[impl]** offers plaintext on loopback only,
   as an explicit developer opt-in, and never as a LAN default.
3. **Real CA certificate** (Let's Encrypt via DNS-01): cleanest
   client story, but adds a public DNS name for a home admin port
   and an ACME client to maintain.

**Compiler's recommendation: option 1**, with **[impl]**'s floor:
no plaintext listener beyond loopback developer opt-in, on any
interface — if Pete prefers the Tailscale route for v1, it should
be option 1's pinned TLS served over the tailnet (which option 1
already does), not **[auth]**'s separate plaintext listener.
**Plain HTTP on a LAN interface is ruled out, not warned about**
**[auth §5]** — this API carries login credentials and starts
processes.

Bind and firewall, where the documents differ in emphasis:
**[auth]** recommends binding 0.0.0.0 with a Windows Firewall rule
**scoped to the local subnet and the Tailscale range
(100.64.0.0/10)**; **[impl D3]** recommends all-interfaces binding
with the rule created on enable and removed on disable, mirroring
the gateway's existing posture. **Compiler's recommendation:**
the **[auth]** scoping, on **[impl]**'s lifecycle — bind
all-interfaces, create the subnet+tailnet-scoped rule on enable,
remove it on disable. The API is more sensitive than the gateway
it mirrors; the wizard machinery already reconciles scoped rules,
so the tighter scope costs nothing.

### 2.5 Capability coverage

**The count, reconciled.** **[audit]** is ground truth: **82
commands** are registered in `tauri::generate_handler!`, each
enumerated with its implementation. **[rest]**'s introduction
says 95 — that figure counts the client wrapper layer in
`ui/src/api.ts` it worked from, whose functions do not correspond
1:1 to registrations (it includes composite helpers, and one
wrapper that names a command which does not exist; below). The
endpoint count then reconciles exactly: 82 commands − 2
desktop-only actions (`open_url`, `open_folder`) − 1 internal
bookkeeping command (`gateway_activation_complete`) = **79
command-backed endpoints**, + 10 derived endpoints (get-one
filters for backends/models/operations, gateway group CRUD over
the config patch, `GET /logs/scopes`, the diagnostics-bundle
content download, the gateway-start alias, the updates list) =
**89 endpoints** **[rest §5]**. The plan uses 82 / 89 throughout.

**Latent defect found by the audit:** `ui/src/api.ts` invokes
`"backend_test_request"`, but the registered command is
`test_backend` **[audit §2]**. The invoke cannot resolve; the
desktop's backend-test wrapper silently degrades to an error
result today. The phase-0 extraction must fix the wrapper name as
part of establishing payload parity — the API's `POST
/backends/{id}/test` wraps the real command.

Coverage positions across the documents:

- **Full mirror, including backend-only commands.** 25 of the 82
  commands are registered but never invoked by the UI **[audit
  §13.1]** — among them the whole MCP surface, WinML, updates
  policy, `update_mxc_policy`, setup state marks, and
  `gateway_test_routing`. **[rest]** specifies endpoints for all
  of them (the API invents no behavior, but it does expose
  behavior the desktop happens not to use), with one carve-out
  below. **[impl D8]** makes the parallel point for the
  extraction: full move of all 82, not a subset, so the two front
  doors can never drift.
- **Peripheral domains carved out of v1.** **[rest §6.15]**
  recommends excluding MCP, VS Code, and WinML (9 endpoints) from
  API v1 — the Android control app has no use for editor setup or
  MCP mode toggles — while still specifying them for completeness.
  The auth classes already cover them (Class X, admin-only)
  **[auth §1]**, so adding them later is additive. Decision 10.
- **Stubs stay stubs.** `updates_check_all` and `updates_apply`
  always error `Unsupported` in the app today **[audit §12]**;
  the contract reserves their paths and returns `501 UNSUPPORTED`
  **[rest §6.14]**. **[impl D7]** additionally recommends
  `updates_apply` remain local-UI-only in v1 even once the app
  implements it. Decision 10.
- **Crate-only capabilities stay out.** Fit check, catalog reads,
  editor MCP config generation, the metrics-store health, the MXC
  decision point **[audit §13.2]** have no command; per §1's
  non-goals they are not v1 surface. One exception in spirit: the
  global routing-strategy rule currently exists twice (Rust
  `resolve_global_strategy` and a TypeScript reimplementation);
  anything the API returns uses the server-side one — the API has
  exactly one implementation of every rule it serves **[audit
  §13.2]**.
- **Two honesty constraints from the audit** shape the contract.
  `task_abort` does not abort — it flips a bookkeeping record
  while the work continues **[audit §15.7]**; the operation abort
  endpoint therefore reports the operation's resulting state and
  claims no more (§3.3). And several "reads" are active probes
  (`test_backend` consumes inference; `gateway_status` probes;
  telemetry lazily starts a poller) **[audit §15.14]** — they are
  classed and documented as such, not treated as free GETs.

### 2.6 Where the documents disagree — summary

| # | Disagreement | Documents | Compiler's recommendation |
|---|---|---|---|
| 1 | **Admin port**: 47821 vs 41231 | [auth §5] vs [impl D3] | **47821** (decision 1). Both values are unassigned and collision-free against the machine's port inventory (gateway 4000, Postgres 54329, backends 8081–8085, WinML 8090); nothing technical separates them. The compiler adopts the auth plan's value because the port is part of the exposure design that document owns, and the implementation plan offers 41231 as a "suggestion" without a distinguishing argument. It is configurable either way — Pete can settle it by fiat. |
| 2 | **Master key over REST**: admin-only audited disclosure endpoint vs never mirror the value | [auth §1 Class S] + [rest §6.5] vs [impl D6] | **Never mirror the value in v1** (decision 4). The only planned consumer is the control app, which has no workflow that needs the key *value* — gateway clients are configured at the desk, where the desktop Reveal already exists. Disclosure would put the gateway's root credential (also its admin password) on the LAN surface to serve no v1 use. The contract's `secret` class and endpoint stay specified but unimplemented; presence/rotation metadata can be added additively if a workflow ever appears. |
| 3 | **First-admin bootstrap**: desktop UI *and* a loopback-only network bootstrap endpoint vs desktop-UI-only | [auth §3.5] vs [impl phase 2, D4] | **Desktop-UI-only** (decision 9). The loopback endpoint exists for headless installs; v1 has no headless mode — the API is hosted by the desktop app, so a local trusted UI always exists. One enrollment path is easier to audit than two. |
| 4 | **Phone credential**: password login creating a session vs QR enrollment code exchanged for a device token | [auth §3.2] vs [impl, Android] | **Unified**: QR enrollment creates a standard per-device session (§2.3). |
| 5 | **Plaintext floor**: Tailscale-only plaintext listener vs loopback-only developer plaintext | [auth §5] vs [impl, Stack] | **Loopback-only** (§2.4). Pinned TLS serves the tailnet too; no separate plaintext listener. |
| 6 | **Firewall scope**: subnet + tailnet scoped rule vs gateway-style all-interfaces rule lifecycle | [auth §5] vs [impl D3] | **Scoped rule, [impl] lifecycle** (§2.4; decision 2). |
| 7 | **URL prefix**: `/api/v1/…` with status as discovery vs `/v1/…` with a `/v1/server` identity document | [rest §2.1] vs [impl, Android] | **`/api/v1`**, the contract document's prefix, with the identity document at `GET /api/v1/server` (§3.8). |
| 8 | **Streaming**: no streaming in v1 vs SSE in phase 4 | [rest §7.7] vs [impl phase 4] | **Poll-only contract; SSE additive in phase 4** (§2.2). |
| 9 | **Self-test class**: `mxc_run_self_test` classed L (lifecycle) vs read | [auth §1] vs [rest §6.13] | **Read-class behavior.** The audit confirms it is pure policy evaluation — nothing executed, no traffic, no state **[audit §11]**. It requires configurator+ anyway under the matrix (MXC reads sit in S), so the practical exposure is identical; the class label should describe the effect, and the effect is a read. |

Two classification systems also need one mapping, since **[rest]**
annotates endpoints with five classes (read / control / config /
destructive / secret) while **[auth]** authorizes against six
(R / S / L / C / D / X) plus user administration and API settings.
**The [auth] taxonomy governs authorization** — it is the finer
split, and its S class separates ordinary reads from config reads
and from disclosure, which is where the security decisions live.
The **[rest]** annotations map onto it as: read → R (or S where
[auth] classes the read sensitive), control → L, config → C,
destructive → D, secret → S-disclosure. MXC policy/mode writes,
VS Code writes, and MCP mode changes — plain `config` in [rest] —
are **X** in [auth] and admin-only; the matrix in §3.5 is the
single source of truth where the two annotations differ.

---

## 3. Architectural specifics

### 3.1 The extraction: `manager-app` (service core)

Everything in §3.2+ rests on one restructuring **[impl, Reuse
strategy]**, because today `src-tauri` is a binary crate excluded
from the workspace and nothing can import from it:

- `AppState`, its task/download registries and lazy accessors,
  `activation.rs`, and `gateway_stats.rs` move into a new library
  crate, **`manager-app`**. `AppState` is already Tauri-free (built
  by `AppState::new(config, config_path, data_dir)`); the move is
  mechanical.
- Each of the 82 command bodies becomes a framework-free operation:
  `op_xxx(&AppState, args…) -> Result<Value, ApiError>`. Bodies
  already receive everything through `AppState` and arguments.
- The 7 commands that emit Tauri events take an additional
  `&dyn EventSink`. The Tauri sink forwards to `AppHandle::emit`
  (identical UI behavior); the API sink publishes to an in-process
  broadcast channel (the phase-4 SSE source; until then it may
  record/drop).
- Ops return the **typed `ApiError`** the bodies already produce
  internally. The Tauri adapter keeps flattening to `String`
  exactly as today (zero UI behavior change); the REST layer maps
  variants to status codes (§3.3) — and sanitizes, once, in one
  place: today's error strings embed argv and paths that must not
  cross the network verbatim.
- Tauri handlers shrink to adapters (extract state → call op →
  flatten). Nothing below the ops layer changes: supervisor,
  wizard `SystemOps`, config save, telemetry store, gateway YAML
  generation, and activation journaling are called exactly as
  today. **API mutations go through the same config-save and
  `record_change` paths as UI mutations**, so a phone change and a
  desk change are indistinguishable to the activation plan, the
  drift check, and cancel/restore.
- Parity is proven, not assumed: golden payload tests captured
  before the move, plus contract tests that run one operation
  through both the Tauri adapter and the REST handler and compare
  (§3.9, phase 0).

**The operation registry (Pete's drift-prevention requirement, as
implemented in phase 0).** Every capability is declared exactly
once, in `manager_app::registry`'s `for_each_operation!` macro: one
entry per operation carrying its canonical name (the Tauri command
name), its capability class (§3.5), its handler, its typed
parameter list, and its REST route (`None` only for the explicit
exclusions — desktop-only actions, the master-key value, the raw
config patch, and the peripheral MCP/VS Code/WinML surface). From
that single declaration, code generation produces:

- the `Op` enum, the `OP_SPECS` table, and `dispatch()` in
  manager-app — an exhaustive `match` with no wildcard arm, so a
  registered operation without a dispatch arm does not compile;
- the `#[tauri::command]` adapters and the `generate_handler!`
  list in src-tauri;
- the axum router in manager-api (phase 1), which registers each
  spec's route with an authorization guard driven by the spec's
  class through `role_allows` — itself an exhaustive `match` over
  the class enum with no wildcard arm, so a new class cannot ship
  without decided grants.

Adding an operation is one registry line; the build then fails at
every unhandled site until it is wired on all surfaces. Contract
tests are the second line of defense: the registry enumerates
itself (82 operations, unique names, unique routes, the §3.5
matrix row by row), the frontend's invoked command names are
checked against the registry (the `backend_test_request` defect
class), and phase 1 adds a registry-vs-live-route-table test in
manager-api.

### 3.2 Hosting: `manager-api`

- New workspace crate **`manager-api`**: **axum** on the existing
  tokio runtime (extractor/middleware model maps onto per-route
  role guards as tower layers; handler tests run in-process via
  `tower::ServiceExt::oneshot`, matching the repo's hermetic test
  style). JSON is `serde_json` — already the boundary format in
  both directions, so REST and UI payloads are the same values from
  the same functions.
- The Tauri `setup()` hook starts the server when the API is
  enabled, handing it the same `Arc<AppState>` the UI uses. One
  process, one supervisor, one config writer, one journal.
- TLS in-process via **rustls** (the stack is already rustls-based;
  no OpenSSL enters the build). The certificate is generated on
  first enable; its private key is stored in Windows Credential
  Manager beside the other managed secrets **[impl]**.
- Configuration is a new `admin_api` section in `AppConfig`
  (`enabled: bool`, default false; `port: u16`), following the
  existing per-section pattern. Enabling/disabling reconciles the
  listener and its firewall rule on the start-gateway precedent
  (one elevation prompt when the rule state changes).
- Middleware, outermost first: request-id + tracing → body size
  limit → timeout → auth (token → principal) → **role guard per
  route group** → audit logging of mutations with the acting user
  id **[impl]**. Deny by default: a route with no class/role
  mapping returns 403 to everyone, including admin, until classed
  **[auth §2]**; the role→class table ships as data in the server
  crate, unit-tested against the route table so an unclassed route
  fails the test suite.

### 3.3 Endpoint model

From **[rest]**, the contract the Android client is built against:

- **Base**: `http://<host>:<admin-port>/api/v1` (§2.6, item 7 for
  the prefix reconciliation). JSON only, except the generated
  gateway config (`text/yaml`) and bundle download
  (`application/zip`). Request bodies are closed objects — unknown
  fields are rejected `INVALID_INPUT`, because the app's patch
  structs are closed and a typo'd field must not silently no-op.
  Field names are the app's snake_case payload names; the desktop's
  camelCase invoke arguments never cross the wire.
- **Error envelope**, one shape for every error:
  `{ "error": { "code", "kind", "message", "details" } }`, where
  `kind` is the app's `ApiError` variant, `code` its stable
  SCREAMING_SNAKE form plus REST-layer codes (`BUSY`,
  `OPERATION_FAILED`, and the auth codes), and `message` is the
  app's string, displayed by clients but never branched on.
  Mapping: validation → 400 `INVALID_INPUT`; absent resource →
  404 `NOT_FOUND`; singleton busy → **409 `BUSY`**; config
  validation → 422 `CONFIG`; app-deferred capability → 501
  `UNSUPPORTED`; everything else → 500 by kind.
- **409 BUSY semantics**: the app serializes on singleton locks
  rather than queueing (one wizard step, one download per model,
  one plan apply). A duplicate POST returns 409 with `details`
  naming the running item and its elapsed seconds — the same
  information the desktop shows — never a silent wait **[rest
  §3, §6.7; audit §15.8]**.
- **Operations**: work over ~30 s returns **202** with the
  operation resource (`id`, `kind`, `status`
  running/done/failed/aborted, `detail`, `created_ms`,
  `started_ms`, `finished_ms`, `error`); `detail` carries the
  outcome the synchronous command would have returned (a wizard
  step's outcome, including `manual_command` on failure). Poll
  guidance: 1 s for 10 s, then 5 s. Progress lives where the app
  keeps it (download byte progress on the model resource), so the
  operation is a completion handle, not the progress channel.
  `POST /operations/{id}/abort` maps to the app's task abort with
  its real semantics stated plainly: it is bookkeeping-level
  best-effort per kind, and the response reports the resulting
  state (§2.5).
- **Collections** return the app's envelope key, complete and
  unpaged (operator-scale lists). The one true paged query is logs,
  keeping `get_logs`' exact semantics (offset/limit, filters,
  `total_matching`, `scopes`).
- **Unknown is never fabricated**: fields the app omits when
  unmeasured (per-process VRAM under WDDM, gateway statistics
  before a metrics window exists) stay omitted in API responses
  **[rest §1]**.

### 3.4 The auth system

From **[auth §3–4]**, with the §2.6 reconciliations applied:

- **Passwords**: argon2id, PHC string format, parameters embedded
  per hash (m = 64 MiB, t = 3, p = 1). Policy: minimum 12
  characters, no composition rules, bundled common-password
  blocklist, no forced rotation (NIST SP 800-63B shape).
  Verification runs the same code path for unknown usernames
  (dummy hash) so timing does not enumerate users; argon2id's
  ~250 ms verify cost is itself an online-guessing throttle.
- **Sessions**: opaque tokens (§2.3). Access 60 min, refresh
  30 days with rotation on every use and reuse-detection revoking
  the session family. Server stores SHA-256 hashes only. Sessions
  are **per device**, named by the client at creation ("Pixel —
  Android app"); a user lists and revokes their own, an admin
  revokes anyone's; logout revokes server-side. Password change
  (current password required) invalidates the user's other
  sessions; admin reset issues a must-change temporary password
  and revokes all sessions; the desktop app's local reset is the
  recovery path of last resort — there is no email recovery,
  because there is no mail infrastructure and none will be grown
  for this.
- **API tokens** (scripts): admin-issued, named, bound to a role
  (standing practice: `viewer`/`operator`, never admin), shown
  once, hash-only at rest, no refresh, no expiry by default
  (optional expiry at creation), individually revocable,
  last-used recorded, and distinguished in the audit log
  (`auth_type: api_token`).
- **Brute-force controls**: per-account — 5 failed logins in
  15 minutes locks the account 15 minutes, further failures extend
  exponentially (15 → 30 → 60, capped), success resets; per-IP
  rate limit on `/auth/login` as a second layer (the account limit
  is primary — LAN/Tailscale source IPs are a weak signal). One
  identical "invalid credentials" error for unknown user, wrong
  password, and locked account; lock state visible only in the
  audit log and to admins.
- **TOTP**: implemented in v1 as an **optional** per-user second
  factor (QR enrollment, secret sealed in the user store — §3.4
  store note — 8 single-use recovery codes shown once, stored
  hashed, ±1 time-step acceptance). When enabled, login returns a
  `totp_required` challenge and tokens are issued only after the
  code verifies. Whether admin accounts must enable it is
  decision 6.
- **User store**: a dedicated file, **`admin-users.json`**, in the
  app data dir, owned by a module in the server crate —
  **deliberately not a field on `AppConfig`**. `update_config`
  writes AppConfig wholesale; users stored there would be
  rewritable by any configurator through a configuration endpoint
  — privilege escalation by construction. Separation makes the
  exclusion structural: the generic config endpoints never
  serialize the user store, and the diagnostics-bundle builder
  must exclude `admin-users.json` explicitly, alongside the
  Credential Manager material. Format: versioned JSON, atomic
  write-temp + rename (the config store's existing pattern),
  loaded at API startup, written on every mutation. Contents:
  users (id, username, PHC hash, role, disabled and must-change
  flags, TOTP state, recovery-code hashes, timestamps), sessions
  (id, user, device name, token hashes, created/last-seen/
  expires), API tokens (id, user, name, role, hash, timestamps,
  optional expiry). Everything is hashes except TOTP secrets,
  which are inherently reversible; those are sealed with a
  machine-local key held in Windows Credential Manager, so the
  file alone does not yield them. Administration operations
  (**[auth §4.2]**) are admin-only: create, list (never hashes or
  secrets), role change / disable / enable, password reset,
  delete — with two guards: the **last enabled admin** cannot be
  deleted or disabled, and no one deletes themselves. Role
  changes and disables revoke the user's sessions immediately;
  token validation always joins the user record, nothing cached
  past a short TTL.
- **First-admin bootstrap**: the API ships disabled. The desktop
  app's Remote Access settings page creates the first admin user
  locally (username + password written straight to the user
  store); enabling the API requires at least one admin to exist.
  If the user store is deleted, the API returns to enrollment mode
  and the desktop UI says so. No default credentials, no factory
  password, and (per §2.6 item 3) no network bootstrap path in v1.
- **Audit log**: every mutation and every auth event appends one
  JSONL record to `logs/admin-api-audit.jsonl` (timestamp, actor,
  auth type, source IP, action = route + class, target id, result,
  request id). Passwords, tokens, and the master key are never
  written. Reads are not audited except the Class S disclosure
  endpoints (master-key reveal if ever implemented, bundle export,
  launch command), which are audited like mutations. No endpoint
  truncates or edits the log; it is readable read-only at
  `GET /admin/audit` (admin), newest first, paginated.

### 3.5 Roles and the capability matrix

Four roles, exactly one per user in v1, assigned as named bundles
fixed in code — never ad-hoc per-user grants **[auth §2]**. The
classes are **[auth]**'s (§2.6 for the mapping from [rest]'s
annotations):

| Class | viewer | operator | configurator | admin |
|---|---|---|---|---|
| R — reads | ● | ● | ● | ● |
| S — config reads (config family, MXC policy read) | ● | ● | ● | ● |
| S — disclosure (master key, diagnostics bundle, launch command) | — | — | — | ● |
| L — lifecycle (start/stop/restart, plan apply, downloads, verify) | — | ● | ● | ● |
| C — configuration edits | — | — | ● | ● |
| D — destructive / install (deletes, plan cancel, wizard steps) | — | — | — | ● |
| X — security policy & credential writes (MXC policy/mode, VS Code, MCP) | — | — | — | ● |
| User administration | — | — | — | ● |
| API settings (enable, port, bind, TLS) | — | — | — | ● |

The load-bearing splits **[auth §2]**: read-only by default (the
first phone credential is `viewer` and can change nothing);
**lifecycle is separate from configuration** — an operator can
restart a crashed backend at 2 AM but cannot change its flags,
placement, or routes, and most real remote use is lifecycle while
most damage is configuration; **destructive is separate again** —
deleting a 50 GiB model or running an installer step is never a
side effect of holding the run-things role; **MXC policy is
admin-only** because the sandbox constrains every other tool the
system runs — a remote session that could set it to Disabled would
own the machine; and **the phone's daily account should be
`operator`, not `admin`** — a stolen phone's residual risk is
bounded by one access-token lifetime *of the phone's role*
**[auth §6]**. Whether this matrix stands as drawn is decision 5.

### 3.6 Activation apply: the one server-side composition

Every endpoint is a thin wrapper over one operation — except one.
**Apply activation plan** is composed today in the desktop UI
(`applyActivationPlan`): for each id in `backend_restart_ids` →
restart backend; then, if `gateway_pending`: (quiesce wait in
Drain mode) → gateway stop → run the `start-gateway` wizard step →
mark gateway scope complete **[audit §14; rest §6.6]**. The REST
API exposes it as a single operation,
`POST /api/v1/activation-plan/apply` with `{ "mode": "drain" |
"now" }`, and **the server owns the composition** — re-implemented
once, server-side, over the same ops the UI calls, so the phone
never replays a fragile multi-call sequence and the desktop and
the API cannot diverge on ordering or partial failure.
`gateway_activation_complete` is not a client endpoint: it is
bookkeeping the server performs after a successful apply or
gateway (re)start. The primitives with standalone meaning
(quiesce-wait, gateway stop/start, backend restart) remain
individually exposed. Gateway **start** itself has no app command
— it is the wizard's `start-gateway` step — so
`POST /api/v1/gateway/start` is that step's run under a domain
name, inheriting its prisma-pin enforcement, `prisma generate`,
database probe, watchdog-flag spawn, sustained-health
confirmation, busy semantics, and its failure surface (the
operation's detail carries the step outcome, including
`manual_command`) **[rest §6.5]**. Wizard steps that need UAC
elevation prompt on the **server's console**: a remote caller
cannot see or answer the prompt, and with nobody at the desk the
step blocks until its budget expires and fails the operation —
a property of the underlying capability, stated in the contract,
not a defect of the mapping **[rest §7.3]**.

### 3.7 Transport and exposure

- **Opt-in, off by default.** The API does not listen until
  enabled in the desktop app's Remote Access page, and enabling
  requires an admin user to exist (§3.4). While enabled, the
  desktop UI persistently shows listening state, bind address,
  port (default per decision 1), and certificate fingerprint — a
  listening admin port is never a surprise **[auth §5]**.
- **Bind**: all interfaces, with the Windows Firewall rule scoped
  to the local subnet + Tailscale range (100.64.0.0/10), created
  on enable and removed on disable (§2.4). Loopback-only binding
  remains available for tunnel-only setups.
- **TLS**: self-signed + QR fingerprint pinning (§2.4); the pinned
  listener serves LAN and tailnet alike; no plaintext beyond
  loopback developer opt-in.
- **Threat model in brief** **[auth §6]**: the LAN attacker
  (compromised IoT device, guest, malware on another home machine)
  is answered by pinned TLS, argon2id cost + lockouts, firewall
  scoping, and the audit log; the stolen phone by refresh rotation
  with reuse detection, per-device revocation, short access
  lifetimes, and the operator-not-admin daily account; the rogue
  read-only user by deny-by-default classification enforced in
  middleware before any handler runs, with denials audited and
  indistinguishable in shape from unknown routes; token leakage by
  hash-only storage, header-only transport (tokens never in URLs),
  one-time display, and per-token/per-session revocation. An
  attacker with interactive access to the Windows machine is out
  of scope — they already own everything the API protects, which
  is precisely why local reset (§3.4) is a feature.

### 3.8 Android consumption constraints

Forward-looking — the app is a later project — but these
constraints shape the API now **[impl, Android constraints;
auth §5]**:

- **QR enrollment**: the desktop shows a QR encoding
  `{ base_url, cert_sha256, enrollment_code }`. The phone scans,
  pins the fingerprint, and exchanges the one-time code for its
  per-device session (§2.3). The QR flow doubles as proof of
  local presence at enrollment. mDNS/Bonjour discovery is a later
  enhancement, not a v1 dependency.
- **Identity document**: `GET /api/v1/server` returns the
  deployment's identity — deployment id, app version, API
  version, enabled feature set — so one client build can
  feature-detect across deployments at different versions
  (Prometheus today, other deployments later). The teach-the-client
  probe `GET /api/v1/status` **[rest §2.1]** remains the
  authenticated status resource; `/server` carries identity only.
- **Per-device, revocable credentials**: each phone holds its own
  session, bound to a user and role; devices are listed and
  revoked from the desktop UI and via the session endpoints. No
  shared secrets, and the user's password is never stored on the
  device. On-device storage (Android Keystore for the refresh
  token and pinned fingerprint, access token in memory only) is
  the client's responsibility; token format, rotation, and expiry
  are the API's.
- **Flaky-network tolerance**: mutations that start work return
  operation ids safe to re-poll after a timeout; reads are plain
  GETs with no session affinity; the phase-4 event stream is
  resumable-by-repoll — a phone on lossy Wi-Fi degrades to polling
  instead of breaking. Versioning is additive-only within `/v1`;
  timestamps stay in the app's conventions (epoch ms, RFC 3339
  strings where the app already uses them); ids are the app's
  existing string ids — no new identifier scheme to translate.

### 3.9 Implementation phasing

From **[impl]**, with the **[auth]** requirements pinned to the
phases that make them true. Each phase ends green on its gates and
leaves the app shippable; the API stays default-off until phase 5,
and **nothing listens beyond loopback until users, roles, and TLS
all exist** — that ordering is the plan's core safety property.

| Phase | Content | Gates (summary) |
|---|---|---|
| **0 — Ops-layer extraction** | §3.1: `manager-app` crate, `EventSink`, typed errors, Tauri adapters; fix the `backend_test_request` wrapper defect (§2.5) | Workspace tests; `src-tauri` check + clippy; UI build + smoke; golden payload parity |
| **1 — Server skeleton + reads** | `manager-api`: axum app, `admin_api` config, listener lifecycle, TLS with generated self-signed cert, identity/status endpoints, the read-only surface. **Loopback only** — the interim static-token idea is explicitly *not* shipped **[impl]** | `oneshot` handler tests over a fake-backed ops layer; loopback live-socket smoke; **redaction tests** proving secret-bearing values appear in no response |
| **2 — Auth and users** | User store, argon2id login/logout, session + refresh rotation, API tokens, role-guard middleware per route group, first-admin bootstrap from the desktop UI, **LAN bind + firewall reconciliation lands here** — the phase that makes the port safe to expose. Brute-force lockout (§3.4) must land in this phase, not later: it is a precondition of LAN exposure, with rate-limit polish finishing in phase 5 | Per-role allow/deny matrix tests over every route; throttling, revocation, hash-parameter tests; phase-1 redaction suite still green |
| **3 — Mutations and lifecycle** | Backend lifecycle and CRUD, config patches, placement, offload, activation apply/drain/cancel (§3.6), gateway stop/start via the start-gateway step (409 BUSY with running-step name + elapsed, mirroring the UI). Audit logging of every mutation | End-to-end over a fake-backed deployment (pair → login → mutate → observe; journal entry carries the API user as author); UI/API concurrency serialization test |
| **4 — Long-running operations** | Downloads, wizard steps beyond start-gateway, updates check — via the task registry: 202 + task id, polling endpoint, SSE stream from the `EventSink` broadcast (backend status, download progress, wizard progress, task progress — the four event kinds the UI receives today) | Task lifecycle incl. abort; SSE reconnect/resume; full-download soak against the fake ops layer |
| **5 — Hardening and enablement** | Rate limiting and lockout policy finish, request limits, token and certificate rotation, audit-log review surface in the desktop UI, **close-to-tray when the API is enabled**, negative-test sweep (expired/revoked/forged tokens, role-escalation attempts, oversized bodies, malformed JSON), docs reconciled against the shipped surface, the default-off → opt-in enablement flow in Settings | Full workspace suite; Windows CI; install-test on the release build; manual on-machine pass driving the API from a second machine on the LAN |

Note one sequencing consequence of the reconciliations: phase 2's
enabling of LAN bind is also where the Remote Access settings page
(desktop) must exist — it is the bootstrap surface, the enable
switch, and the pairing-QR display in one place **[auth §3.5,
§5]**.

---

## 4. Decision points for Pete

Consolidated and deduplicated from **[auth §7]**, **[impl
D1–D8]**, and **[rest §8]**. Each carries the compiler's
recommendation; approving the plan approves the recommendations
except where you override them here.

1. **Admin port number and enablement persistence.**
   Options: **47821** [auth] or **41231** [impl] — both unassigned
   and collision-free (§2.6, item 1); the port is configurable in
   either case. Separately: persistent enablement vs a
   desktop-side confirmation at every app start.
   **Recommendation: 47821, persistent.** The listening state is
   shown persistently in the desktop UI; a per-start confirmation
   trains clicking-through without adding a real check.
2. **Bind and firewall posture.**
   Options: all-interfaces bind with the firewall rule **scoped
   to the local subnet + Tailscale range** [auth], or the
   gateway-style all-interfaces rule created on enable / removed
   on disable [impl].
   **Recommendation: scoped rule on the [impl] lifecycle** (§2.4).
3. **TLS.**
   Options: **self-signed + QR fingerprint pinning**, Tailscale-
   only plaintext, or a real CA certificate (Let's Encrypt
   DNS-01) [auth §5].
   **Recommendation: self-signed + pinning**, with no plaintext
   listener beyond loopback developer opt-in (§2.4).
4. **The LiteLLM master key over REST.**
   Options: an admin-only, audit-logged disclosure endpoint,
   as classed in [auth] and specified in [rest §6.5]; or never
   mirror the value — presence/rotation metadata only [impl D6].
   **Recommendation: never mirror the value in v1** (§2.6,
   item 2). The desktop Reveal remains the disclosure path; the
   endpoint stays specified-but-unimplemented so adding it later
   is additive.
5. **The role matrix as drawn** (§3.5).
   Two sub-questions [auth §7.3]: does `operator` apply
   activation plans (it executes changes a configurator saved —
   the plan says yes, Class L), and is the phone's daily account
   `operator` with a separate, rarely used `admin` (the plan
   says yes)?
   **Recommendation: adopt the matrix as drawn, both
   sub-questions yes.**
6. **TOTP posture.**
   Options: optional per user in v1, or required for `admin`
   accounts from day one [auth §3.7].
   **Recommendation: optional in v1**, enabled on Pete's admin
   account by choice; making it mandatory for admin is a one-line
   policy change later.
7. **API token issuance.**
   Options: admin-issued only, or any user may mint tokens for
   themselves up to their own role [auth §3.3].
   **Recommendation: admin-issued only in v1.** Self-minting is
   additive later; issuance audit is simpler while the user count
   is one household.
8. **Session lifetimes.**
   Options: access 60 min / refresh 30 days as drawn, or a
   shorter refresh (7 days) for admin-role sessions [auth §7.6].
   **Recommendation: as drawn for all roles in v1.** Rotation
   with reuse detection already bounds a stolen refresh token;
   a per-role lifetime split is tuning, addable without a
   contract change.
9. **First-admin bootstrap path.**
   Options: desktop-UI creation only [impl], or desktop UI plus
   a loopback-only network bootstrap endpoint while zero users
   exist [auth §3.5].
   **Recommendation: desktop-UI only** (§2.6, item 3) — v1 has no
   headless mode, so the endpoint solves a problem that does not
   exist yet.
10. **Peripheral domains in v1.**
    Options: include or exclude the MCP, VS Code, and WinML
    endpoints (9, specified in [rest §6.15], classed X in
    [auth]); and within updates: expose the policy writes
    (pin / auto-update — plain config edits) while
    `updates_check_all` / `updates_apply` remain app-side stubs,
    with `updates_apply` staying local-UI-only even once
    implemented [impl D7].
    **Recommendation: exclude the peripheral trio from v1;
    include updates policy writes; `updates_apply` local-only.**
    All exclusions are additive to reverse.
11. **The broad raw config patch in v1.**
    `PATCH /api/v1/config` mirrors `update_config`: one endpoint
    that can merge any field of `AppConfig`, including gateway
    bind and ports [audit §1]. The domain endpoints cover the
    same ground with narrower blast radius [rest §8.4].
    **Recommendation: exclude from v1.** No Android workflow
    needs arbitrary config merge; the user-store separation
    (§3.4) already removes its worst hazard, and the endpoint
    can be added additively if a real need appears.
12. **Remote wizard / install-class operations.**
    `run_wizard_step` installs software, reconciles firewall
    rules with UAC prompts on the server console, and runs for
    minutes [audit §3]. Options: expose it in v1 at Class D
    (admin only), accepting that steps needing elevation fail
    unattended [rest §7.3]; or keep install-class steps
    local-only in v1. Gateway start is *not* part of this
    question — it rides the same step but is core control (§3.6)
    and is in v1 either way.
    **Recommendation: expose at Class D, admin only**, with the
    elevation limitation stated in the client wherever a step is
    offered.
13. **Logoff survival.**
    Options: v1 in-process with close-to-tray (API up while the
    app process is up and the user is logged in), or scope the
    Windows-service host now [impl D1].
    **Recommendation: in-process + close-to-tray for v1**; the
    service host is a separate later project that re-hosts the
    same crates (§2.1). Open question this leaves for you: must
    phone control work while nobody is logged in to Prometheus?
    If yes, this decision flips and the phasing grows.
14. **Extraction scope.**
    Options: the full 82-command phase-0 move, or a partial move
    of only the commands v1 endpoints need [impl D8].
    **Recommendation: full extraction.** Partial is faster to
    the first endpoint and leaves two ops implementations to
    drift — the failure mode the extraction exists to prevent;
    the move is mechanical and golden-tested.
15. **Operation retention.**
    Options: operations retained for the process lifetime,
    mirroring the app's task list, or a bounded history (e.g.
    last 100) stated in the contract [rest §8.3].
    **Recommendation: process lifetime in v1.** It matches the
    app's semantics, the volumes are operator-scale, and a bound
    can be introduced as server behavior without breaking the
    client contract.

---

*Sources: `capability-audit.md` (82 commands, classes, awkward
fits), `rest-api.md` (89 endpoints, error envelope, operation
model), `auth-plan.md` (roles, tokens, user store, transport),
`implementation-plan.md` (hosting, extraction, phasing). Detail
lives there; this document is the synthesis to approve.*

## 9. As-built notes (October 17, 2026, branch feature/admin-api)

What shipped, where the build settled questions the plan left
open, and the deliberate deviations. Code won wherever it and the
plan disagreed; this section is the record.

**Surface totals.** 104 route records: 65 registry-generated op
routes, 15 derived/composite routes, 24 auth/admin routes. Every
endpoint documented in rest-api.md section 6 is implemented
except two deliberate exclusions: `GET /gateway/master-key`
(decision 4 — the op is routeless; the key never crosses REST in
v1) and raw `PATCH /config` (decision 11). `POST
/updates/components/{component_id}/apply` is routed and returns
501 UNSUPPORTED from the app's own stub (decision 10).

**Addendum — v0.1.40 (October 18, 2026).** The totals above were
written before three post-plan operations landed; the current
registry (`crates/manager-app/src/registry.rs`) is the source of
truth:

- **108 operations** in total (the §3.1 contract test's "82
  operations" is the plan-era count): **66 with REST routes**
  (50 sync 200, 10 async 202, 2 created 201, 4 body-wrapped sync)
  and **42 with route `None`**, each an explicit exclusion listed
  in rest-api.md §6.16. The live surface is therefore **105 route
  records**: 66 registry + 15 derived + 24 auth/admin.
- The +1 routed operation over the totals above is
  `backend_set_mtp` (`PUT /api/v1/backends/{id}/mtp`, class C,
  tracker #53). The other growth is routeless: `mcp_start_preflight`
  (#44) and `backend_add_preview` (#49, desktop-only) joined the
  exclusions, and `backend_add` gained its optional launch
  parameters (`n_ctx`, `n_batch`, `offload_layers`,
  `offload_override`, `gpu_indices`) under its existing route.
- The peripheral operations (§6.15 of rest-api.md — MCP, VS Code,
  WinML) are likewise unimplemented over REST per decision 10;
  the "every endpoint implemented" sentence above covers the v1
  committed surface only.
- `GET /api/v1/events` (SSE) is an API-layer derived route, not a
  registry operation; the audit-log read path is
  `GET /api/v1/admin/audit` (§3.4's `GET /admin/audit` omits the
  prefix). Both `POST /updates/check` and
  `POST /updates/components/{component_id}/apply` return 501
  UNSUPPORTED from the app's stubs.

**Registry drift mechanism.** As specified in section 3.1:
`for_each_operation!` in manager-app is the single declaration;
the Tauri adapters, the handler list, the axum router, and the
authorization table are all generated or derived from it, with
exhaustive matching and contract tests on both sides (route
table vs registry; frontend invoke scan vs registry plus the
allowlisted desktop-local commands, which must exist in the
shell source).

**Composites** (model PATCH, gateway group CRUD, gateway start,
activation apply, gateway-config bind_host split) inherit their
authorization class from the operations they compose, looked up
in the registry at table construction — with the one declared
exception: the activation apply is class L (decision 5) although
it internally runs the admin-class start-gateway step.
`POST /gateway/start` inherits the step's class D. Composite
read-modify-write sequences are serialized by a per-context lock.

**Asynchronous endpoints.** AsyncOp routes, gateway start, and
the activation apply answer 202 with an API-side operation record
(TaskRecord shape), retained for the process lifetime and merged
into `GET /operations`. Abort is bookkeeping with the app's
semantics: the record flips to aborted; the underlying work is
not cancellable and its completion never overwrites the abort.

**Events.** `GET /api/v1/events` (SSE) carries the app's
operation events with process-global ids. Live-only: nothing is
replayed on reconnect; ids let a client detect gaps.

**Deviations.**
- TLS private key custody: a key file under
  `<data_dir>/admin-api/tls/` rather than Windows Credential
  Manager. The in-process server reads it at startup before any
  UI exists; the directory is per-user app data alongside
  `admin-users.json`. Moving custody is future hardening.
- QR rendering: the desktop pairing surface shows the payload
  (`base_url`, `cert_sha256`, enrollment code) as copyable values;
  no QR library was installable in the build environment. The
  payload format is the QR content, so rendering is additive
  later.
- Certificate SAN set: hostname + primary LAN IP + Tailscale IP
  (when the CLI reports one) + loopback, regenerated when the set
  changes at enable time; rotation is a desktop action that
  restarts the server.
- Test-environment note: workspace tests run on a Linux VM
  without the Windows desktop stack; the Tauri shell is
  type-checked against a stub harness, and the UI is
  pattern-verified only. On-machine Windows validation (real
  firewall prompts, tray behavior, LAN reachability) remains for
  the first install.

