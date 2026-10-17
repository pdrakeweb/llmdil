# LLMdil Android App — Implementation Plan

Status: plan for Pete's review, 2026-10-17. No code written.
Companion to `PLAN.md` (the admin API plan, approved 2026-10-17
with the compiler's recommendations, plus Pete's override: the API
binds all interfaces — any device on the LAN may reach it; no
port-forwarding is his internet boundary). This document plans the
API's first consumer: the Android control app.

Process: this plan follows Pete's `android-app-development` skill
(v0.6.0, `~/workspace/custom-skills/android-app-development/`).
The skill's lifecycle governs the app project itself — Phase 0
intake → Phase 1 `APP_SPEC.md` → Phase 1b design of record →
Phase 2 implementation plan approved **before any code** → build,
test-scenario, audit, beta, handoff phases. This document is the
input to those phases, not a substitute for them: most intake
questions are already answered by the API documents cited
throughout, and §8 collects the ones that are genuinely Pete's to
decide. Audience calibration: Pete is the developer (expert
level) — the skill's Q0 is answered by his role; questions are
kept to real decisions.

---

## 1. Scope

The app is a remote control for one or more LLMdil
deployments (Prometheus first). It does everything the API's
`/api/v1` contract (`rest-api.md`, 89 endpoints) exposes to its
role, and nothing else — the API invents no behavior, and neither
does the app. What the API excludes from v1, the app also excludes:
no MCP/VS Code/WinML screens (`PLAN.md` decision 10), no raw
config patch (decision 11), and **no master-key display** — the
value is never mirrored over REST in v1 (decision 4), so no screen
shows it.

Control surface, mapped to `rest-api.md` §5 endpoint groups:

| App area | Endpoint groups | Notes |
|---|---|---|
| Deployment status | Status (1), Telemetry (4) | `GET /status`, `GET /server` identity, GPU telemetry |
| Backends | Backends (14), Placement (2) | Lifecycle, config view/edit, flags, offload, test request |
| Gateway | Gateway (13) | Status, groups, routing test/recent, start/stop |
| Activation plan | Activation plan (4) | Pending changes, apply (drain/now), quiesce, cancel |
| Models | Models (11) | Download progress + pause/resume/cancel, verify, add/delete |
| Operations | Operations (3) | Every 202 operation: poll, outcome, abort |
| Logs | Logs (2) | Paged query with the contract's exact filters |
| Setup | Wizard / setup (11) | Step list, run/retry/skip; Class D, admin only, UAC caveat shown |
| Diagnostics | Diagnostics (2) | Create bundle, download/share the zip |
| Settings | Config / settings (2, read + domain patches only) | Raw `PATCH /config` excluded per decision 11 |

Role gating is the API's (`auth-plan.md` §2 matrix: viewer /
operator / configurator / admin over classes R/S/L/C/D/X). The app
renders only the controls the signed-in role may use; the server
remains the authority, and a 403 is surfaced as a plain message —
never a dead control (skill house rule: a control that does
nothing with no explanation is the worst failure mode).

## 2. Enrollment and auth

Per `PLAN.md` §2.3/§3.4/§3.8 and `auth-plan.md` §3.2/§5,
unified as one flow:

1. **Pairing.** The desktop Remote Access page shows a QR encoding
   `{ base_url, cert_sha256, enrollment_code }`. The app scans it
   (manual entry of the same three values is the fallback),
   **pins the certificate fingerprint**, and exchanges the
   one-time enrollment code for a standard per-device session —
   access token (60 min) + refresh token (30 days), for the user
   and role the desktop chose. The user's password is never typed
   into or stored on the phone.
2. **Token storage on device.** Refresh token and pinned
   fingerprint in Android Keystore-backed encrypted storage;
   access token in memory only (`auth-plan.md` §5). Deployment
   records (name, base URL, fingerprint, session) are stored per
   deployment; credentials and fingerprints are **excluded from
   Android auto-backup** (`dataExtractionRules` /
   `fullBackupContent`) — backup eligibility is decided
   deliberately per the skill's storage rules, never defaulted.
3. **Refresh.** Rotation on every use, single-flight (concurrent
   requests share one refresh). The presented token is replaced
   atomically with the returned one.
4. **Revocation and reuse detection.** Presenting an
   already-rotated refresh token revokes the whole session family
   server-side. On the client, a reuse/revocation response is
   terminal for that deployment's session: local tokens are
   wiped, the deployment shows "session revoked — re-pair at the
   desktop," and there is no silent retry and no re-pairing from
   the phone. The same state covers desktop-side revocation and
   role change/disable (which revoke sessions server-side).
5. **Fingerprint change is a hard stop.** A presented certificate
   whose SHA-256 does not equal the pinned fingerprint fails
   closed with no bypass affordance in the UI; re-pairing happens
   at the desktop, never by tapping through (`auth-plan.md` §5).
6. **TOTP.** The API supports optional per-user TOTP for password
   logins. The app v1 offers **no password login** — enrollment
   only — so no TOTP entry is needed in the app (see §8, decision
   5).

Named invariants for this app (skill house rule — stated once
here, repeated verbatim in every later audit prompt):

- **"A certificate fingerprint mismatch is a hard stop: the app
  never offers a bypass, and no request is sent over an unpinned
  or mismatched connection."**
- **"The app never displays a value it did not receive: unmeasured
  data is omitted, stale data is labeled with its age, and nothing
  is ever zero-filled."** (This is desktop design rule 12 carried
  to the client, and the wire contract's rule 4.)

## 3. Screens

Inventory, each mapped to its endpoint group. One deployment is
active at a time; a deployment switcher lives in Settings and in
the Status screen's header.

| # | Screen | Reads | Actions (role class) |
|---|---|---|---|
| 1 | Pairing (QR scan + manual entry) | — | Enrollment exchange |
| 2 | Status | `/status`, `/server`, `/gateway/status`, `/telemetry/gpus` | Deployment switch |
| 3 | Backends | `/backends` | — (rows navigate to 4) |
| 4 | Backend detail | `/backends/{id}`, `/config`, `/launch-command` (S-disclosure, admin) | Start/stop/restart (L), test (L), config patch + preview-diff (C), flags/offload (C), delete (D) |
| 5 | Placement | `/placement/propose` | Apply (C) — from backend/model context, not a top-level screen |
| 6 | Gateway | `/gateway/status`, `/gateway/config`, `/routing/recent` | Start (L, operation), stop (L), group CRUD (C/D), routing test (R) |
| 7 | Activation | `/activation-plan` | Apply drain/now (L, operation), quiesce-wait (L), cancel (D) |
| 8 | Models | `/models` | Download + pause/resume (L, operation), cancel (destructive class per contract), verify (L, operation), add (C), rename/enable (C), delete (D) |
| 9 | Operations | `/operations` | Abort (L) |
| 10 | Logs | `/logs`, `/logs/scopes` | Filter/paging controls only |
| 11 | Setup | `/setup/steps`, `/setup/status`, `/setup/prerequisites` | Run/retry (D, operation), skip/mark-manual (C), complete (C) |
| 12 | Diagnostics | — | Create bundle (operation), download + share zip |
| 13 | Settings | `/auth/me`, own sessions | Sign out (revokes server-side), revoke own sessions, change own password, deployment management (add/remove/re-pair) |

Deferred past v1: user administration and the audit-log viewer
(admin-only API surface; the desktop is the admin surface — §8,
decision 6). MXC screens are excluded with the peripheral domains.

### Design principles applied

`docs/design-principles.md` governs the app UI as it governs the
desktop UI; the mappings that bite:

- **Status colors are the same semantics** (rule 11): green
  running/enabled/adopted, gray disabled/neutral, light red
  stopped/failed, amber starting/stopping. Backend `state` values
  map 1:1; a state chip appears only for non-default information.
- **Label: Value everywhere** (rule 9); explanation lives behind
  info affordances, not inline prose (rule 5).
- **Unknown is not zero** (rule 12): the API omits unmeasured
  fields (per-process VRAM under WDDM, stats before a metrics
  window); the app omits the row — see invariant 2 in §2.
- **Multi-number data is a visualization** (rules 3, 8): GPU
  screens lead with per-GPU VRAM/utilization bars; backend memory
  (GPU RAM / system RAM / KV) renders as segmented bars, not
  number lists.
- **Compact over long** (rules 1, 2, 4): backend detail is one
  screen with tabs (Status / Config / Logs), not stacked sections;
  summary first, detail on demand.
- **Button form** (rule 15): icon-only for safe secondary actions
  (copy URL, refresh); words + icon for lifecycle-critical and
  destructive actions (Start, Restart, Apply Changes, Delete) and
  for each surface's standard next action.
- **Number formatting** (rule 14): ≤1 decimal, trailing `.0`
  dropped, no decimals ≥100.
- Phase 1b of the app project produces `DESIGN_TOKENS.md` and
  mockups (including empty, longest-label, and error states) and
  names a design of record before the plan is written, per the
  skill. Semantic tokens are defined alongside Material roles —
  the status colors above are semantic tokens, not ad-hoc hex.

## 4. Architecture

Per the skill's bootstrap defaults (`bootstrapping.md`), set from
the first commit:

**Version pins** (skill `platform-currency.md` §1, verified
2026-09-04 at the skill's writing — **re-verify at scaffold time**
per the skill; if the date is stale by then, re-check before
scaffolding): AGP 9.4.0, Gradle 9.6.0, JDK 17, SDK Build Tools
36.0.0, compileSdk 37, targetSdk 36, minSdk 28 (see §8, decision
2). All versions in one Gradle version catalog. AGP 9 notes that
bite: Kotlin is built in (no Kotlin Android plugin), `kapt` is
incompatible — Hilt runs on **KSP**.

**Modules** (dependency edges as stated; the pure-domain rule is
load-bearing — the skill's audits flag any `android.*` /
`androidx.*` import in `:domain` as Critical):

- `:domain` — pure Kotlin, no Android imports. Domain models
  (Backend, Model, GatewayStatus, Operation, …), repository
  interfaces, use cases, the role/capability model the UI gates
  on.
- `:network` — the wire layer, depending only on `:domain`'s
  interfaces. OkHttp + kotlinx.serialization DTOs that mirror the
  contract field-for-field (snake_case payload names), the error
  envelope parser (`code` is branched on; `message` is display
  only), and the TLS stack (§below). DTOs are **not** domain
  models: the wire gets its own types so a server-side rename
  cannot silently change the protocol (skill sync rule), and
  contract tests (§7) pin the mapping.
- `:data` — repository implementations over `:network`, the
  session/token store (Keystore-backed), the deployment store,
  refresh single-flight and rotation handling, offline/stale
  bookkeeping (every cached value carries its fetch time).
- `:app` — Compose UI, Hilt DI, Navigation 3, ViewModels exposing
  `StateFlow` UI state. Edge-to-edge insets handled from the first
  screen (mandatory at targetSdk 36); back handling on
  `BackHandler` / `OnBackPressedDispatcher`, never
  `onBackPressed()`.

**Networking and TLS pinning.** The QR's `cert_sha256` is the
SHA-256 fingerprint of the server's self-signed certificate. The
client installs a trust manager that computes the SHA-256 of the
presented certificate and compares it to the pinned value for
that deployment; hostname verification is superseded by the
fingerprint equality (that is the TOFU model the QR's out-of-band
channel authorizes). Mismatch, absence, or an unpinned deployment
fails closed (invariant 1). HTTP logging is redacted by
construction — `Authorization` headers never appear at any log
level, and production logging stays at BASIC (skill cloud-access
rules). Response reads on error paths are bounded
(`peekBody` cap) when building error objects.

**State management.** One ViewModel per screen; repositories are
the only network callers; polling lives in the repository layer
with the contract's cadence (operations: 1 s for 10 s, then 5 s;
`rest-api.md` §3.1) so screens never own timers. SSE (API phase 4)
slots into the same repository flows as an accelerator later;
polling remains the correctness path.

**Offline and unreachable behavior.** Prometheus being off, asleep,
or off-LAN is a normal state, not an error path:

- Every screen distinguishes **live**, **stale** (last-known data
  with its age shown), and **unreachable** (no data yet). Cached
  values are never rendered as live; control actions are disabled
  with the reason visible while unreachable — not tappable
  no-ops.
- Mutations are never queued for later replay. A control action
  either reaches the server and returns its operation id, or
  fails loudly in place. (Replaying a queued "restart" hours
  later is a hazard, not a convenience.)
- A timeout on a mutation is ambiguous by nature; the recovery is
  the contract's: re-poll operations/state and reconcile, never
  blind-retry the POST (409 BUSY is the server's answer to a
  duplicate, and the app treats it as "already running — here's
  the operation," not an error).

**Permissions** (classified per the skill's doctrine; recorded in
the app spec's permission table):

| Permission | Class | Why / denial behavior |
|---|---|---|
| Camera | Enhancing | QR pairing only, requested in context from the Pair flow; denial leaves manual entry fully working |
| Notifications | Enhancing | Operation-completion notices (§8, decision 3); denial leaves in-app polling untouched |
| Internet / network state | Normal | Not runtime-requested |
| Biometric | — (API, not a permission) | Optional gate on refresh-token use (§8, decision 4) |

No other runtime permissions. Deny-everything is a test scenario
(§7), and every denial leaves its workflow completing with a
visible reason and a working inline retry that branches correctly
on permanent denial.

## 5. Phasing

Two phase ladders interlock: the API's phases 0–5 (`PLAN.md`
§3.9) and the app skill's phases 0–7. The app project runs its
own Phase 0–2 first (intake playback from these documents, spec,
design of record, plan approval) — that work needs nothing from
the API implementation and starts now.

| App milestone | Content | Depends on |
|---|---|---|
| **M0 — Scaffold + contract fakes** | Repo scaffolded on the pins; modules as §4; DTO layer written against `rest-api.md`; MockWebServer fake implementing the documented contract (enrollment exchange, reads, 202 operations, error envelope, 409 BUSY) | Nothing. Starts as soon as the app plan/spec gates pass |
| **M1 — Read-only app vs. fake** | Pairing UX (scan + manual), Status, Backends, Gateway, Models, Logs screens read-only against the fake; offline/stale states; contract tests green | M0 |
| **M2 — First usable milestone: paired read-only against the real API** | Enrollment against the desktop QR, pinned TLS on the LAN, live reads (status, backends, gateway, GPUs, models, logs) with a `viewer`/`operator` session | **API phase 2** (auth, LAN bind, pairing QR). API phase 1 is loopback-only, so nothing on the phone can be tested end-to-end before phase 2 lands |
| **M3 — Control** | Lifecycle (start/stop/restart, gateway start/stop), activation apply/cancel, config patches with preview-diff, placement apply — with role gating | **API phase 3** |
| **M4 — Operations complete** | Downloads with progress (model-resource polling), wizard steps, diagnostics bundle download/share; SSE client as an accelerator if API phase 4's stream exists | **API phase 4** |
| **M5 — Beta on hardware** | Skill Phase 5 audit + Phase 6 beta: signed release build on Pete's Pixel via `verify-install.sh` / `verify-artifact.sh` gates; hardening behaviors (rotation, revocation, rotation/reuse states) verified against the real server | **API phase 5** enablement; real hardware |

What can be scaffolded now, against the documented contract
alone: everything in M0–M1, plus the M3/M4 screens' UI against
the fake. What cannot be tested before the API's auth phase is
live: the real enrollment exchange, pinning against a real
certificate, refresh rotation and reuse detection, role enforcement
as the server applies it, and every mutation. Those are M2+
acceptance items, named here so nobody reports them as app bugs
during M1.

## 6. Testing

Per the skill's Phase 4 (agent-executable scenarios; zero FAILs
to pass the gate) and `testing-and-bugs.md`:

- **Harness first**: `tests/README.md` naming the rigs — (a)
  emulator + MockWebServer fake, (b) emulator + real API on the
  LAN (API phase 2+), (c) real Pixel + real API. What each rig can
  and cannot prove is written down; a scenario needing a rig that
  doesn't exist yet is BLOCKED with the rig named, never passed on
  a weaker rig. LAN/real-rig scenarios stay in the Markdown ADB
  format — the emulator's networking is not the LAN.
- **Journeys** (`*.journey.xml`) for feature behavior against the
  fake: pairing (manual entry), status rendering incl. omitted
  unmeasured fields, backend lifecycle happy path, operation
  polling to completion, activation apply flow.
- **Contract tests** (JVM): golden payloads taken from
  `rest-api.md` examples parse into the DTOs and map to domain
  models field-for-field; the error envelope maps by `code`, never
  by `message`. These run in CI on every change — they are the
  drift alarm between this client and the contract document.
- **Fault injection** against a scriptable fake (the
  `panel-remote` pattern): server refuses, hangs past timeout,
  starves (slow drip), vanishes mid-poll, returns malformed JSON,
  returns 409 with the busy payload, rotates-then-revokes the
  refresh family, presents a wrong-fingerprint certificate. Each
  has a scenario asserting the user-visible outcome in §4's terms
  (stale labeled, hard stop, no dead controls).
- **Permission scenarios as ADB tests** (Journeys auto-grant, so
  denial paths would pass for the wrong reason): camera denied
  once / permanently (inline retry must deep-link to Settings when
  spent), notifications denied, deny-everything cold start driving
  the main flow.
- **Empty and first-run**: fresh install shows the pairing screen
  and nothing else — no fabricated status, no zeros (invariant 2).
  Screenshot-checked.
- **Gates**: every implementation pass ends with a real build and
  `verify-install.sh` exit 0; release candidates pass
  `verify-artifact.sh` (v2+ signature, ABIs, targetSdk). Every
  fixed silent failure gets a regression scenario — a silent
  failure that regresses is invisible again.

## 7. Release posture

Personal, non-distributed build (the skill's personal-APK framing
— confirmed here as a decision for Pete in §8, not assumed):
signed release APK, sideloaded via `adb` / direct install on
household devices; version single-sourced from the git tag with
`versionCode` derived; tag-triggered CI build per the skill's
`releasing.md`. No credentials are baked in — the app holds only
per-device session tokens it enrolled for, which is why the
personal-APK credential exception is not needed here.

## 8. Open decisions for Pete

Each with the plan's recommendation; approving this plan approves
the recommendations except where overridden.

1. **Distribution.** (a) Direct APK sideload — GitHub release on
   the app repo, installed on household devices (ADB sideloading
   is unaffected by developer verification); (b) Play limited
   distribution account (no ID, no fee, up to 20 devices);
   (c) full Play (targetSdk policy deadlines bind, review
   overhead for a household tool). **Recommendation: (a)** — the
   audience is Pete's household, and it matches how the desktop
   app is already delivered.
2. **Minimum Android version.** The skill's floor is minSdk 28
   (Android 9, 2018 devices). Pete's devices are current Pixels;
   raising the floor buys nothing this app uses — Keystore,
   biometric, and Compose all run at 28. **Recommendation:
   minSdk 28** per the skill, targetSdk 36 / compileSdk 37.
3. **Operation completion notification.** (a) Polling only —
   the v1 contract is poll-only, cadence per §4; (b) add the SSE
   client once API phase 4 ships the stream; (c) push (FCM) —
   requires server-side push infrastructure the API does not
   have and the plan does not build. **Recommendation: (a) for
   v1, (b) as an M4 accelerator; not (c).** Background polling
   with a foreground service is explicitly not proposed — the
   app notifies only while open in v1.
4. **Biometric gate on token use.** Gate refresh-token use behind
   biometric / device-credential unlock (bounds the
   stolen-unlocked-phone residual in `auth-plan.md` §6) or leave
   tokens ungated. **Recommendation: gate it**, with device
   credential as fallback; the gate fires on session resume after
   the access token has expired, not on every screen.
5. **Password login in the app.** Enrollment-only (the unified
   flow in `PLAN.md` §2.3; password never on the device, TOTP not
   needed in the app) vs also offering username/password login as
   a fallback (adds the TOTP challenge UI and a password-handling
   surface). **Recommendation: enrollment-only in v1**; password
   login is additive later if a deployment ever needs it.
6. **Admin screens in the app.** User administration, audit log,
   and API settings are API surface (admin role) with no app
   screens in v1; own-session management is in Settings (§3).
   **Recommendation: defer full admin to a later milestone** —
   the desktop is the admin surface, and the phone's daily
   account is `operator` by design (`PLAN.md` decision 5).
7. **App project home and identity.** New repo
   `pdrakeweb/llmdil-android`, applicationId
   `com.ourdrakefamily.llmdil`, app name "LLMdil".
   The repo split follows the skill's extraction hygiene
   (independent release cadence from the manager). The
   applicationId is cheap only before the first install —
   changing it later means a new app. **Recommendation: as
   stated.**
8. **Tablet form factor.** Phone-first; no orientation lock, so a
   tablet runs the phone layouts unoptimized, vs adaptive layouts
   in v1. **Recommendation: phone-first**, adaptive layouts as a
   later design-phase item if a tablet becomes a real control
   surface.

---

*Sources: `PLAN.md` (approved architecture and decisions),
`rest-api.md` (89-endpoint contract), `auth-plan.md` (roles,
tokens, enrollment, storage), `implementation-plan.md` (API
phasing), `docs/design-principles.md` (UI rules), and the
`android-app-development` skill — `SKILL.md`, `references/
lifecycle.md`, `references/bootstrapping.md`, `references/
permissions-storage-cloud.md`, `references/platform-currency.md`.*
