# Admin API — AuthN/AuthZ Plan (design draft)

Status: design draft for Pete's review, 2026-10-16. No code written.
Scope: authentication, authorization, user management, and transport
exposure for the planned admin REST API (admin port on the managed
machine, consumed first by an Android client). Endpoint-by-endpoint
REST documentation lives in the companion API doc; this plan defines
who may call what, and how callers prove who they are.

Grounding facts from the current app (v0.1.32):

- The capability surface is the ~82 Tauri commands in
  `src-tauri/src/main.rs`, mirrored by `ui/src/api.ts`. There is no
  actor concept today: the local desktop user is implicitly trusted,
  and every command runs with the app's full authority.
- Real secrets (LiteLLM master key, Postgres password) live in
  Windows Credential Manager, never in the config JSON. The config
  file itself is operator-editable JSON in the app data dir.
- One command already discloses a credential to any local caller:
  `gateway_master_key` returns the master key in plaintext (the
  Gateway view's Reveal button). Over a network API that command
  changes category — see Class S below.

## 1. Capability inventory, by class

Authorization is assigned per class, not per endpoint, so the matrix
stays auditable as endpoints are added. Every endpoint belongs to
exactly one class; a new endpoint cannot ship until it is classed.

### Class R — reads (non-sensitive)

Status and observation that reveals no secret and changes nothing:
`get_status`, `list_backends`, `get_gpu_telemetry`,
`gateway_status`, `gateway_recent_requests`, gateway statistics,
`list_models`, `get_logs` (app/backend/gateway logs — operational,
may incidentally contain paths and model names, no credentials by
construction), `tasks_list`, `placement_propose` (a dry-run
calculation), `gateway_preview_yaml`, `gateway_test_routing`,
`gateway_activation_plan`, `backend_preview_diff`,
`setup_status` and the other setup reads, `winml_status`,
`mxc_get_activity_report`, `updates_check_all`, `mcp_list`.

### Class S — sensitive reads (credential-adjacent)

Reads whose payload is, or can carry, a secret or a full system
export:

- `gateway_master_key` — returns the LiteLLM master key in
  plaintext. The key is also the LiteLLM admin password; disclosure
  is total gateway compromise.
- `diagnostics_export_bundle` — the bundle contains raw,
  unredacted process logs (the Settings UI says so verbatim).
- `backend_get_launch_command` — the spawn command line; the
  gateway/backend environment is assembled at spawn from Credential
  Manager material, so a rendered command line must be treated as
  potentially secret-bearing until proven otherwise.
- `get_config` / `get_gateway_config` / `backend_get_config` /
  `get_mxc_policy` — no secrets by construction (secrets are in
  Credential Manager), but a full configuration read is the complete
  system map: paths, topology, policy. Grouped here, not in R, so a
  read-only role handed to a third party does not silently include
  it. (The per-role split of S is in the matrix — viewer gets the
  operational config reads, not the three disclosure items above.)

### Class L — lifecycle control

Mutations that run or stop things but do not change saved
configuration: `start_backend`, `stop_backend`, `restart_backend`,
`test_backend`, `gateway_stop`, `gateway_quiesce_wait`,
`gateway_activation_complete` / plan apply (executes already-saved
changes), `download_model`, `model_pause_download`,
`model_resume_download`, `cancel_download`, `task_abort`,
`model_verify_checksum`, `telemetry_reset_peaks`,
`telemetry_save_snapshot`.

### Class C — configuration edits

Writes to saved configuration that take effect at apply/next start:
`update_config`, `update_gateway_config`, `backend_add`,
`backend_apply_config`, `backend_update_flags`,
`backend_set_offload`, `model_add`, `model_assign`,
`model_set_enabled`, `model_set_short_name`, `placement_apply`,
`gateway_set_bind_host`, `telemetry_set_interval`,
`updates_pin`, `updates_set_auto`, `setup_mark_complete` and the
other setup state marks.

### Class D — destructive and install-level operations

Irreversible, expensive, or code-changing operations:
`backend_delete`, `model_delete` (deletes a downloaded GGUF —
tens of GiB), `gateway_activation_cancel` (restores a prior
configuration snapshot, discarding the pending one),
`updates_apply` (replaces installed component binaries),
`run_wizard_step` / `setup_retry_step` (installs software: venv,
Postgres, Prisma — long-running, machine-changing),
`winml_register` / `winml_unregister`, `task_abort` is L, not here.

### Class X — security-policy and credential operations

Operations that change the security posture itself, or write
credentials to new places: `update_mxc_policy`, `mxc_set_mode`
(the MXC sandbox decides what tool execution is allowed —
weakening it remotely must not be a mid-tier capability),
`mxc_propose_tightening` is a read (R), `mxc_run_self_test` is L.
`vscode_write_config` (writes the master key into editor config
files on disk), `vscode_install_extensions`, `mcp_set_mode`,
`mcp_test_connection` (MCP server entries can carry tokens).
Plus everything in §4: user administration and this API's own
settings (port, bind, TLS, token issuance).

## 2. Roles

Four discrete roles. A user holds exactly one role in v1 (a later
version may allow a set of roles, interpreted as the union — the
matrix is written so union semantics stay safe). Roles are
cumulative in capability but are assigned as named bundles, never
as ad-hoc per-user grants: what a role can do is fixed in code,
reviewable in one table, and identical for every holder.

| Role | One-line scope |
|---|---|
| `viewer` | Watch: status, telemetry, logs, statistics. Change nothing. |
| `operator` | Run the system day to day: start/stop/restart backends and gateway, apply saved plans, run downloads. Cannot change any saved setting. |
| `configurator` | Shape the system: all configuration edits (backends, models, gateway, placement, flags) on top of operator. Cannot delete, install, disclose credentials, or touch security policy. |
| `admin` | Everything, including destructive operations, credential disclosure, MXC policy, and user administration. |

### Capability matrix

Grant = ●, deny = —.

| Class | viewer | operator | configurator | admin |
|---|---|---|---|---|
| R — reads | ● | ● | ● | ● |
| S — config reads (get_config family, MXC policy read) | ● | ● | ● | ● |
| S — disclosure (master key, diagnostics bundle, launch command) | — | — | — | ● |
| L — lifecycle | — | ● | ● | ● |
| C — configuration edits | — | — | ● | ● |
| D — destructive / install | — | — | — | ● |
| X — security policy & credential writes | — | — | — | ● |
| User administration (§4) | — | — | — | ● |
| API settings (port/bind/TLS enable) | — | — | — | ● |

Design notes on the split:

- **Read-only by default.** The first credential Pete issues to a
  phone for "check the GPUs from the couch" is `viewer`, and it
  cannot start, stop, edit, or disclose anything.
- **Lifecycle is separate from configuration.** `operator` can
  restart a crashed backend at 2 AM from a phone but cannot change
  its flags, move its placement, or repoint a route. Most real
  remote-control use is lifecycle; most damage is configuration.
- **Destructive is separate again.** Deleting a 50 GiB model or
  running an installer step is never a side effect of holding the
  "run things" role.
- **Credential disclosure is admin-only and singular.** Exactly
  three endpoints disclose, all in one row of the matrix, all
  audited at a higher level (§4).
- **MXC policy is admin-only.** The sandbox mode is the control
  that constrains every other tool the system runs; a remote
  session that could set it to Disabled would own the machine.
- Deny by default at the router: an endpoint with no class/role
  mapping returns 403 to everyone, including admin, until classed.

Enforcement model: each route declares its class; middleware maps
the caller's role to the class set and rejects before the handler
runs. The role→class table above is the single source of truth and
ships as a data table in the server crate, unit-tested against the
route table so an unclassed route fails the build's test suite.

## 3. AuthN design

### 3.1 Passwords

- Username + password login. Passwords hashed with **argon2id**
  (PHC string format, parameters embedded per hash: m = 64 MiB,
  t = 3, p = 1 — interactive-login cost on the workstation class
  this runs on, revisable without invalidating old hashes).
  Verification through the same code path for existing and
  non-existing usernames (a dummy hash) so login timing does not
  enumerate users.
- Password policy: minimum 12 characters, no composition rules,
  checked against a small bundled blocklist of common passwords
  (NIST SP 800-63B shape). No forced rotation.
- Rejected: bcrypt (acceptable, but argon2id's memory hardness is
  the current consensus default and the Rust `argon2` crate is
  maintained); SHA-family fast hashes (never acceptable for
  passwords); storing passwords encrypted instead of hashed (the
  server never needs the plaintext).

### 3.2 Sessions and tokens (the Android client's model)

Recommended: **opaque bearer tokens with a server-side session
store**, in two parts.

- **Access token**: 256-bit random, opaque, presented as
  `Authorization: Bearer …`. Lifetime 60 minutes. The server stores
  only its SHA-256 hash, so a leak of the session store does not
  leak usable tokens.
- **Refresh token**: 256-bit random, opaque, lifetime 30 days,
  **rotated on every use** — each refresh returns a new refresh
  token and invalidates the presented one. Presenting an already-
  rotated token is treated as theft: the whole session family for
  that device is revoked and the event is audited. This is the
  standard OAuth 2.0 refresh-rotation pattern, implemented
  server-side without standing up an OAuth server.
- **Sessions are per device.** Each login creates a named session
  ("Pixel — Android app", set by the client at login). A user can
  list their sessions and revoke any one of them; an admin can
  revoke any user's. Logout revokes the session server-side —
  closing the app is not a logout.
- Rejected: **JWT access tokens** — self-contained tokens cannot
  be revoked before expiry without reintroducing a server-side
  denylist, which is the session store again with extra steps; on
  a single-machine server the store lookup is free. JWT's one real
  advantage (stateless verification by third parties) does not
  exist here. Rejected: **cookies** — the client is a native app,
  not a browser; cookie semantics buy nothing and complicate CSRF
  posture if a web client appears later. Rejected: **one
  long-lived token, no refresh** — a stolen token is then a
  permanent credential; rotation + revocation is the control that
  bounds a stolen phone (§6).

### 3.3 Per-client API tokens (scripts and automation)

Separate from sessions, for Pete's own scripts (a cron health
check, a shell alias):

- Created by an admin (or by a user for themselves, if Pete wants
  that later — v1: admin-issued), named, bound to a **role** —
  usually a lesser one than the creator's; a token can never exceed
  the issuing admin's own ceiling is unnecessary complexity, the
  control is that tokens are admin-issued and role-scoped.
- Shown exactly once at creation; the server stores only the
  SHA-256 hash. Bearer-presented like an access token but **no
  refresh, no expiry by default** (optional expiry at creation),
  revocable individually, last-used timestamp recorded. Clearly
  distinguished in the audit log (`auth_type: api_token`).
- Rejected: reusing the LiteLLM master key as the API credential.
  It is a different trust domain (inference gateway vs machine
  administration), it cannot be scoped or revoked per client, and
  it already lives in too many places (editor configs, clients).

### 3.4 Brute-force resistance

- Per-account throttling: 5 failed logins in 15 minutes locks the
  account for 15 minutes; further failures while locked extend the
  lock exponentially (15 → 30 → 60 min, capped). Successful login
  resets the counter. Lockouts are audited.
- Per-source-IP rate limit on `/auth/login` as a second layer —
  secondary because on a LAN/Tailscale the source IP is a weak
  signal (NAT, shared tailnet), so the account limit is primary.
- Identical error for unknown user, wrong password, and locked
  account ("invalid credentials"), with lock state revealed only
  through the audit log and to admins — no user enumeration, no
  lockout oracle.
- Argon2id's cost is itself a throttle: ~250 ms per verify caps
  online guessing regardless of limits above.

### 3.5 First-admin bootstrap

The dangerous moment in every auth system is enrollment before
any user exists. Rules:

- The API ships **disabled** (§5). Enabling it in the desktop app
  when no users exist starts enrollment mode.
- In enrollment mode, `POST /auth/bootstrap` is accepted **only
  from loopback**, and only while zero users exist; it creates the
  first `admin` user and immediately closes. A remote bootstrap
  attempt is refused and audited.
- The intended path is simpler and preferred: the desktop app's
  Remote Access settings page (local, trusted UI) has "Create
  first admin user" — username + password fields, written straight
  to the user store. The loopback endpoint exists so a headless
  install is not locked out; both paths are equivalent in effect.
- No default credentials, no factory password, no "admin/admin"
  first boot. If the user store is deleted, the API returns to
  enrollment mode and says so in the desktop UI.

### 3.6 Password change and reset

- **Change**: any signed-in user, requires the current password;
  invalidates all their sessions except the current one (standard
  practice — a password change after a suspected compromise kicks
  the other device out).
- **Reset by admin**: an admin sets a temporary password for a
  user; the account is flagged must-change, all sessions revoked,
  and the temporary password works only to reach the change-
  password endpoint. Audited.
- **Local reset**: if every admin is locked out, the desktop app
  (local access = physical access = already root on this machine)
  offers password reset for any account. This is the recovery path
  of last resort; it is why no email recovery exists — the system
  has no mail infrastructure and a home server should not grow one
  for this. Rejected: security questions (weak, and the answers
  live in the same house as the machine).

### 3.7 TOTP (optional second factor)

Recommendation: **implement TOTP as an optional per-user second
factor in v1, not required.** Enrollment: user scans a QR (secret
generated server-side, stored sealed in the user store — see §4
note on store protection), 8 single-use recovery codes shown once
and stored hashed. When enabled for a user, login returns a
`totp_required` challenge; the access/refresh pair is issued only
after the code verifies. Server accepts ±1 time step.

Suggested posture: Pete enables it on `admin` accounts; `viewer`
accounts on the home LAN do not need it. Making it mandatory for
admin is a one-line policy change later — listed as an open
decision (§7). Rejected for v1: WebAuthn/passkeys (the right
long-term answer, but Android client + Windows server pairing UX
is a project of its own; TOTP gets 90% of the value now),
SMS codes (no telephony path, SIM-swap class issues), push
approval (needs a notification service the app does not have —
and per the current codebase, notifications are a dead config
field with no delivery mechanism at all).

## 4. User management

### 4.1 Storage — a dedicated user store, deliberately not AppConfig

Recommendation: a **separate store file** (`admin-users.json`) in
the app data dir, managed by a dedicated module in the server
crate — **not** a field on `AppConfig`. Reasons:

- `update_config` (Class C) writes AppConfig wholesale. If users
  lived there, any configurator could rewrite the user list or a
  password hash through a configuration endpoint — privilege
  escalation by construction. The generic config endpoints must
  never serialize the user store; separation makes that structural
  rather than a field-filtering promise.
- Password hashes, TOTP secrets, and session hashes need different
  handling from operator configuration: never in diagnostics
  bundles, never in config previews, never in the audit log.
  (Action item for the API implementation: the diagnostics bundle
  builder must exclude `admin-users.json` explicitly — the user
  store joins the Credential Manager material on the bundle's
  exclusion list.)
- File format: versioned JSON, atomic write (write-temp + rename,
  the pattern the config store already uses), loaded at API
  startup, written on every mutation. Contents: users (id,
  username, password PHC hash, role, disabled flag, must-change
  flag, TOTP secret + enabled flag, recovery-code hashes,
  created/last-login timestamps), sessions (id, user, device name,
  token hashes, created/last-seen/expires), api tokens (id, user,
  name, role, token hash, created/last-used/optional expiry).
- TOTP secrets are the one item that is stored reversibly (they
  must be, to verify codes). Mitigation: they are sealed with a
  machine-local key held in Windows Credential Manager alongside
  the existing secrets, so the store file alone does not yield
  them. Everything else in the file is hashes only.
- Rejected: SQLite — the app has one database already (the
  gateway's Postgres, which belongs to LiteLLM, not the manager),
  and a second store for a handful of users buys transactions the
  atomic-file pattern already provides at this scale. Rejected:
  Windows Credential Manager per user — it stores secrets for the
  OS account, not an application user database; it stays the home
  of machine secrets (master key, Postgres password, TOTP sealing
  key), not of user records.

### 4.2 Operations (admin-only, Class: user administration)

- `POST /admin/users` — create (username, initial password, role).
- `GET /admin/users` — list (never hashes, never secrets; role,
  disabled, last login only).
- `PATCH /admin/users/{id}` — change role, disable/enable.
- `POST /admin/users/{id}/reset-password` — §3.6.
- `DELETE /admin/users/{id}` — delete. Guards: cannot delete or
  disable the **last enabled admin** (the system must always keep
  a recoverable admin path short of local reset); cannot delete
  yourself.
- Role assignment is a property of the user record, set only
  through these endpoints. Changing a role revokes the user's
  existing sessions (a demotion must take effect now, not at
  token expiry).
- Disabling a user: blocks login immediately, revokes all
  sessions and refresh families; API tokens of the user stop
  working (checked against the user record per request — token
  validation always joins the user store, nothing is cached past a
  short TTL).
- Self-service (any role): `GET /auth/me` (own record, own
  sessions), change own password, manage own TOTP, revoke own
  sessions, list own API tokens (creation stays admin-issued, §3.3).

### 4.3 Audit trail

Every mutation through the API — and every auth event — appends
one JSONL record to `logs/admin-api-audit.jsonl` in the app data
dir (same directory the wizard log and diagnostics bundle already
use), with: timestamp, actor (user id + username, or `bootstrap` /
`local-ui`), auth type (session / api_token / bootstrap), source
IP, action (route + class), target (backend id, user id, …),
result (success / denied / error), and request id. Passwords,
tokens, and the master key are never written — actions are logged
by name, payloads by target id only.

- Reads are not audited except Class S disclosure endpoints
  (master key reveal, bundle export, launch command) — those three
  are audited like mutations.
- The log is append-only from the API's perspective: no endpoint
  truncates or edits it; rotation is by file size with dated
  rollovers, matching the app's existing log handling.
- Exposed read-only at `GET /admin/audit` (admin only), newest
  first, paginated — same shape the Logs view already uses.

## 5. Transport and exposure

- **Opt-in, off by default.** The admin API does not listen until
  Pete enables it in the desktop app (Remote Access page). Enabling
  requires at least one admin user to exist (§3.5). The desktop UI
  shows the listening state, bind address, port (proposed default
  **47821**, unassigned range, distinct from the gateway's 4000),
  and cert fingerprint persistently while enabled — a listening
  admin port must never be a surprise.
- **Bind choices** (operator picks at enable time): loopback only
  (useful solely with a tunnel — included for completeness),
  **LAN + Tailscale (recommended)**: bind 0.0.0.0 on the admin port
  with a Windows Firewall rule scoped to the local subnet and the
  Tailscale range (100.64.0.0/10) — the wizard already manages a
  scoped firewall rule for the gateway, so this follows existing
  precedent; or a single specified interface address for stricter
  setups.
- **TLS — decision with a recommendation.** Options:
  1. **Self-signed certificate + client pinning (recommended).**
     At enable time the server generates a self-signed cert
     (SANs: machine hostname, LAN IP, Tailscale IP, `prometheus`
     mDNS-style alias where available; 2-year validity, regenerated
     on demand from the desktop UI). The desktop Remote Access page
     shows the cert fingerprint and a QR code encoding
     `{ base_url, cert_sha256 }`; the Android app scans it at
     pairing and pins the fingerprint (TOFU with an out-of-band
     check — the QR is the out-of-band channel). A changed
     fingerprint is a hard stop in the client, with re-pairing done
     from the desktop screen again, never by tapping through.
  2. Plain HTTP **restricted to Tailscale only**: WireGuard
     encrypts the tailnet path, so HTTP inside it is defensible.
     The server would refuse LAN-interface plaintext entirely.
     Cheaper to build, but it makes the phone dependent on
     Tailscale being up at home, and it teaches the client to
     accept plaintext — one config slip away from LAN exposure.
  3. Real CA certificate (Let's Encrypt via DNS-01 on a family
     domain): cleanest client story, but adds a public DNS name
     for a home admin port and an ACME client to maintain.
  Recommendation: **option 1**, with option 2's Tailscale path
  accepted as a consequence (the pinned TLS listener also serves
  the Tailscale interface — no separate plaintext listener).
  **Plain HTTP on a LAN interface is not offered at all** —
  credentials and bearer tokens on a broadcast network with other
  people's devices on it is the one configuration this plan rules
  out rather than warns about.
- **Client storage (for the Android phase):** refresh token and
  pinned fingerprint in Keystore-backed encrypted storage, access
  token in memory only; biometric unlock gating the refresh
  token's use is an app-side decision for that phase, noted here
  because it determines how bad a stolen unlocked phone is (§6).
- Versioning: the API is `/v1/…`; the pairing QR carries the base
  URL so a future version can live beside it.

## 6. Threat model (brief)

Assets: control of a workstation that runs local LLM backends and
holds the LiteLLM master key; the user store; the audit trail's
integrity. Attacker positions and the controls that answer them:

- **LAN attacker** (compromised IoT device, guest on the network,
  malware on another home machine). Sees the admin port, can
  attempt logins and replay anything sniffable. Controls: TLS with
  pinning (nothing sniffable is usable), argon2id + lockouts
  (online guessing is slow and noisy), audit log (attempts are
  recorded), firewall scoping (the port is not reachable from
  outside the LAN/tailnet at all — the router is not part of this
  design and port-forwarding it would void the model).
- **Stolen phone.** The attacker holds a live refresh token.
  Controls: refresh rotation with reuse detection (the moment the
  real user or the thief refreshes, the other chain dies and the
  event is audited), per-device session revocation from the
  desktop UI or another session ("sign out the Pixel"), short
  access-token lifetime, and the recommendation that the Android
  app gate token use behind biometric unlock. Residual risk: up to
  one access-token lifetime (60 min) of the phone's role — a
  reason the phone's everyday account should be `operator`, not
  `admin`.
- **Rogue read-only user attempting mutation.** A `viewer`
  credential (perhaps shared casually — "check the dashboard")
  tries lifecycle or config calls, by curiosity or because the
  credential leaked. Controls: deny-by-default route classification
  with the role check in middleware before any handler code, 403s
  audited with actor identity, and no capability discovery via
  error differences (a denied route and an unknown route are
  indistinguishable in shape). The matrix is the control: viewer
  holds no mutation class at all.
- **Token leakage** (token pasted into a log, a screenshot, a
  backup). Controls: hashes-only storage server-side (a leaked
  store file yields no usable tokens), one-time display for API
  tokens, tokens never in URLs (header only, so they stay out of
  request logs), the audit log recording actions but never token
  values, and per-token/per-session revocation as the incident
  response. A leaked **API token** is bounded by its role — issue
  scripts `viewer`/`operator` tokens, never admin, as standing
  practice.
- Out of scope, stated plainly: an attacker with interactive
  access to the Windows machine itself (they already own
  everything the API protects — local reset, §3.6, is a feature
  under exactly this assumption), and compromise of the desktop
  app's own process.

## 7. Open decision points for Pete

1. **TLS option**: self-signed + QR pairing/pinning (recommended)
   vs Tailscale-only plaintext for v1.
2. **TOTP posture**: optional per user (recommended for v1) vs
   required for `admin` from day one.
3. **Roles**: the four-role matrix as drawn — in particular, is
   `operator` allowed to apply activation plans (it executes
   configurator-saved changes; this plan says yes, Class L), and
   should the phone's daily account be `operator` with a separate
   `admin` used rarely (recommended)?
4. **API tokens**: admin-issued only (recommended v1) vs users may
   mint tokens for themselves up to their own role.
5. **Admin port default** (47821) and whether enabling the API
   also requires a desktop-side confirmation each app start
   (recommended: no — persistent setting, visibly shown).
6. **Session lifetimes**: 60 min access / 30 day refresh as drawn,
   vs shorter refresh (7 days) for admin-role sessions.

---

Companion documents in this series: the capability audit and REST
API documentation, and the implementation plan (which sequences
user store → auth middleware → routes → pairing flow → Android
client) build directly on the classes and matrix defined here.
