# Code review — v0.1.60 (deep review)

Scope: the whole tree at `main` = `b410986` (v0.1.60, Nov 6, 2026) — 18 crates under `crates/`, `src-tauri/`, and `ui/src` — reviewed by risk in five areas: (1) security surfaces, (2) persistence and state, (3) process lifecycle, (4) parsers / arithmetic on untrusted input / FFI, (5) the UI ↔ Rust contract. Method: per-area full-file reads by parallel reviewers, then every Blocker and Major re-read against the code by the coordinator before listing; each fixed finding carries a regression test that was run red (before) and green (after) where the code is reachable on Linux. Line references are to the base commit `b410986` unless a fix commit is named. Branch: `review/v0.1.60-deep-review`.

Two findings were handed in with the brief (F1, F2); both were verified and fixed.

## Summary

| Severity | Count | Fixed in this PR | Deferred / needs decision |
|---|---|---|---|
| Blocker | 1 | 1 | 0 |
| Major | 25 | 21 (2 of them partially) | 4 |
| Minor | 30 | 12 | 18 |
| Nit | 12 | 2 | 10 |
| **Total** | **68** | **36** | **32** |

Per area: security 9 Major (incl. F1) / 8 Minor / 4 Nit · persistence 1 Blocker / 5 Major (incl. F2) / 6 Minor / 1 Nit · process lifecycle 4 Major / 7 Minor / 2 Nit · parsers & FFI 2 Major / 6 Minor / 3 Nit · UI contract 5 Major / 3 Minor · coordinator 2 Nit.

### Top 5 findings

1. **Blocker — Lock-order deadlock between `download_model` and `list_models` wedges the whole app.** `crates/manager-app/src/lib.rs:4002-4058` vs `:3805-3806`. `op_download_model` held `downloads` while waiting for `config.write()`; `op_list_models` (polled every 2 s by the Models view during a download) takes `config.read()` then `downloads`. Clicking Download on a second model during a backend start deadlocks both, and tokio's writer-preferring `RwLock` then parks every later `config.read()`. Fixed in this PR: `2a9171c`.
2. **Major — Unauthenticated `HEAD` requests run every GET handler of the Admin API.** `crates/manager-api/src/auth_routes.rs:216-218` (with `resolve_template` `:163-166`). The class table lists only GET rows, so HEAD resolved to no class and the middleware passed it through with no token check while axum ran the GET handler — admin user list, audit log, launch commands and diagnostics bundle content answered status and headers to anyone who could reach the port. Fixed in this PR: `ecc7aaf`.
3. **Major — The gateway venv installed whatever LiteLLM PyPI served that day (F1).** `crates/manager-wizard/src/steps.rs:3702-3713` (`litellm_spec`), default config with no pin. LiteLLM 1.107.0 was published malicious on Oct 21, 2026; any fresh install or venv rebuild could pull a compromised release. Fixed in this PR: `905d3f8` (default pin 1.105.1, load-time backfill, wizard fallback).
4. **Major — A corrupt `config.json` was silently replaced by defaults and then destroyed (F2).** `src-tauri/src/main.rs:165-173` → first save in `ensure_catalog_models` (`crates/manager-app/src/lib.rs:1486`). The shell used `load` (never surfacing a `.bak` fallback), ran on defaults with an invisible `eprintln!` when both files failed, and startup seeding then rotated the corrupt primary over `.bak`. Fixed in this PR: `3ecf855`.
5. **Major — TOTP guessing never locks an account; the lockout reset happened at the password stage.** `crates/manager-api/src/auth.rs:339`. `record_success` ran as soon as the password verified, before the second factor, so an attacker holding the password could alternate login / one TOTP guess indefinitely below the 5-failure threshold (only the per-IP throttle remained). Fixed in this PR: `22f90b7`.

Runner-up Majors worth triage next: the Admin API TLS listener blocked by one idle TCP connection (SEC-07, fixed), service-mode child stdio never captured (PL-M1, fixed, gate-verified only), every PostgreSQL start sitting out its 60 s budget (PL-M3, fixed), four UI calls sending snake_case keys Tauri silently dropped (UI-1..4, fixed), `must_change_password` never enforced (SEC-03, needs decision), and SCM recovery actions that can never fire (PL-M2, needs decision).

### Verification status

- Local (Linux): `cargo test --workspace` green; `cargo clippy --workspace --all-targets -- -D warnings` green on Linux and, for the crates touched, also cross-checked for `x86_64-pc-windows-gnu`; `ui`: `npm ci && npm run build && npm run smoke` green; `cargo doc --no-deps` clean for the crates whose docs this PR touched.
- `cargo fmt --all -- --check` **fails on `main` already**: the tree carries pre-existing rustfmt drift in ~60 files (see D-2). Every hunk this PR adds was checked to be rustfmt-clean; the drift itself was not reformatted (a mechanical whole-tree reformat would bury the review diff).
- `cfg(windows)` code (service host, SCM, NVML/PDH/DXGI, tasklist image checks) and `src-tauri` are gate-verified only — the Prometheus gates run them natively.

---

## Area 1 — Security surfaces

### Major

**F1. LiteLLM installed unpinned by default.** `crates/manager-wizard/src/steps.rs:3702-3713`; `crates/manager-config/src/lib.rs` `default_config` (`updates.components` empty). `litellm_spec()` emitted bare `litellm[proxy]` unless `updates.components.litellm.pinned_version` was set, and nothing set it. Scenario: fresh install, or any start-gateway run that recreates the venv (wrong Python version, `--clear`) on or after Oct 21, 2026 → `uv pip install litellm[proxy]` resolves to the newest release, including the malicious 1.107.0. **Fixed in this PR: `905d3f8`** — `DEFAULT_LITELLM_PIN = "1.105.1"` in `default_config`; `migrate()` backfills the pin only when the litellm component is absent or unpinned (explicit operator pins untouched); `litellm_spec` falls back to the same pin so no path emits a bare requirement. Tests: `default_config_pins_litellm`, `migrate_backfills_litellm_pin_only_when_unpinned`, `litellm_spec_is_always_pinned`. Behaviour note: "Unpin" for LiteLLM now re-pins to the default on the next load (the existing settings round-trip test was updated to assert this and to keep unpin→None coverage on `llamacpp`). Hash-locked requirements: **Deferred** (see Deferred list).

**SEC-01. HEAD bypasses authentication.** `crates/manager-api/src/auth_routes.rs:216-218`. Scenario: `HEAD /api/v1/admin/users` with no token → 200 and the handler runs; `HEAD /diagnostics/bundles/{id}/content` reads the whole zip and returns `Content-Length` (existence/size oracle, DoS). The module doc promises deny-by-default. **Fixed in this PR: `ecc7aaf`** — HEAD resolves as GET for the class lookup. Test: `head_requests_get_the_get_guard` (every GET route → 401 for HEAD without a token; viewer 403 / admin 200 on an admin route); verified red before the fix.

**SEC-02. Password stage reset the account lockout.** `crates/manager-api/src/auth.rs:339`, `:387`. Scenario above (Top 5 #5). **Fixed in this PR: `22f90b7`** — the counter resets only when tokens are issued; `complete_totp_login` refuses while locked. Tests: `totp_guessing_locks_the_account`, `totp_success_resets_and_lock_blocks_second_stage`.

**SEC-03. `must_change_password` is never enforced.** Set at `auth.rs:707` (admin reset), read nowhere (`login :352`, `validate_bearer :541-589`, middleware). Scenario: an admin resets a password; the temporary password logs in to a full 30-day session at the user's role and is never forced to change — auth-plan §3.6 says it "works only to reach the change-password endpoint". **Needs Pete's decision**: the fix adds a new error code (`PASSWORD_CHANGE_REQUIRED`, 403) to the API contract and changes what CLI/UI clients must handle after a reset.

**SEC-04. API tokens could rotate the owner's password or enroll a second factor.** `auth_routes.rs:52-57` (self-service routes `Authenticated`), `:656-719`; `auth.rs:661-693, 940-946`. Scenario: an operator-role API token issued for user `admin` leaks; its holder enrolls TOTP on the admin account (persistent lockout of the real admin) or brute-forces the password through `/auth/password` with no lockout accounting, then sets a new one. **Fixed in this PR (partially): `22f90b7`** — the four self-service endpoints refuse `AuthType::ApiToken` (403), and step-up password failures on change-password / TOTP-disable / TOTP re-enrollment count toward the account lockout and are refused while locked. Tests: `api_tokens_cannot_use_account_self_service`, `change_password_guesses_lock_the_account`. Remaining part — requiring the password step-up for *first* TOTP enrollment — **Needs Pete's decision** (changes the UI/CLI enrollment flow).

**SEC-05. Unauthenticated memory and disk growth via the login username.** `auth_routes.rs:428-433` (no length cap), `auth.rs:274, 313` (lockout map keyed by attacker text, never pruned), `audit.rs:45-80` (raw actor, no rotation, whole-file reads). Scenario: anyone reaching the port posts failed logins with distinct ~1 MB usernames; each adds a permanent ~1 MB key and a ~1 MB audit line. **Fixed in this PR: `22f90b7`** — usernames over 64 chars (the `create_user` limit) are refused before any per-account bookkeeping (same argon2 cost, same answer), idle lockout and per-IP entries are pruned, the audit actor is capped at 64 chars. Test: `oversized_usernames_do_not_grow_the_lockout_map`. Audit-log rotation: **Deferred** (policy choice, see below).

**SEC-06. Argon2 ran inline on the shared async workers.** `auth.rs:323-329, 673, 942`; `auth_routes.rs:702-711` (also under the user-store read lock). Scenario: the API shares the app's tokio runtime (one worker per CPU); a handful of concurrent unauthenticated logins (64 MiB, t=3 each) stall supervision, Tauri commands and every other request. **Fixed in this PR: `22f90b7`** — `verify_password_async` runs on the blocking pool behind a global 2-permit semaphore; the TOTP-disable check verifies outside the store lock. No timing-based regression test (would be flaky); covered by the auth suites passing through the new path.

**SEC-07. One idle TCP connection blocked every TLS handshake.** `crates/manager-api/src/tls.rs:194-209`. axum accepts one connection at a time and `TlsListener::accept` awaited the handshake inline with no timeout. Scenario: a client opens TCP to :47821 and sends nothing → no other client can connect until it goes away. **Fixed in this PR: `293a887`** — handshakes run as `JoinSet` tasks with a 10 s timeout (≤ 64 in flight); `accept` yields whichever completes first. Test: `idle_connection_does_not_block_other_handshakes` (verified red against the old inline accept).

**SEC-08. Certificate silently regenerated whenever the LAN/Tailscale IP changes.** `tls.rs:65-82` (`ensure_cert` regenerates on any SAN difference), `src-tauri/src/main.rs:362-407` (SANs from the IPs at that moment, every start). Scenario: the app autostarts before Tailscale is up, or DHCP moves the LAN IP → new key and fingerprint → every paired CLI/app client fails pinning and must re-pair; pinned clients do not check hostnames, so SAN freshness buys nothing. **Needs Pete's decision**: the fix (load existing material at start; refresh SANs only via the explicit Regenerate action) changes the certificate lifecycle posture.

### Minor

**SEC-09. CLI accepted `http://` base URLs.** `crates/manager-cli/src/client.rs:127-141`. The pin lives in the TLS config, so `LLMDIL_BASE_URL=http://…` sent the password/token in cleartext with no pin. **Fixed in this PR: `73cf6fc`** — `https://` required, `https_only(true)`, redirects disabled. Test: `api_client_requires_https_base`.

**SEC-10. Log-shipping redaction missed Python-repr / single-quoted values and escaped quotes.** `crates/manager-logship/src/redact.rs:363-418`. `{'password': 'hunter2hunter2'}`, `api_key='abc…'` (LiteLLM's debug format) and `"token": "ab\"cdefgh"` passed through (partly) unredacted. **Fixed in this PR: `023a59b`**. Test: `python_repr_and_escaped_quotes_redact`.

**SEC-11. Firewall drift check accepted widened, duplicated or disabled rules.** `crates/manager-api/src/firewall.rs:103-127`. `RemoteIP: LocalSubnet,100.64.0.0/10,Any`, a second same-named rule, or `Enabled: No` all read `Ok`, so `ensure()` never repaired them. **Fixed in this PR: `5e7d941`** — exact scope set, single rule, enabled. Tests: `status_drifted_when_scope_widened`, `…_when_disabled`, `…_when_rule_duplicated`. (Classifier is pure; the netsh path is Windows-only.)

**SEC-12. Refresh-token rotation check-then-act race.** `auth.rs:467-523`. Two concurrent refreshes with the same token could both rotate. **Fixed in this PR: `f23dec1`** — re-check under the write lock; a mismatch is reuse and revokes the family. Test: `concurrent_refresh_rotates_at_most_once` (can expose, not force, the interleaving).

**SEC-13. Audit coverage gaps.** `auth_routes.rs:471-485` (`handle_totp` writes no entry — TOTP logins and guesses unrecorded), `auth.rs:277-282` (lock events not audited), `src-tauri/src/main.rs:572-713` (desktop `admin_api_*` user/cert commands not audited). **Deferred**: the desktop actor/auth_type values are an owner choice; the TOTP entry is cheap and belongs with it.

**SEC-14. Every API-token request rewrites `admin-users.json`.** `auth.rs:617-629` (no staleness throttle, unlike sessions); retired refresh hashes and dead sessions never pruned. **Deferred**: performance/wear, not correctness.

**SEC-15. TOTP codes replayable within the ±1-step window.** `auth.rs:1111-1119`. **Deferred** (hardening; small, but adds per-user state worth designing with SEC-03).

**SEC-16. Log-streaming endpoint may be plain `http://`.** `crates/manager-logship/src/shipper.rs:437-445`; no scheme validation on `log_streaming.endpoint`. **Decided (Nov 12, 2026): refuse.** Fixed after this review: plain `http://` is accepted only for a loopback endpoint; the shipper does not start otherwise, and says why on the Log receiver card (`crates/manager-logship/src/endpoint.rs`, docs/log-streaming.md "Endpoint policy"). The config value is still accepted and saved. Tests: `non_loopback_http_is_refused`, `non_loopback_http_endpoint_is_refused`, `saved_settings_apply_live_and_a_plain_http_endpoint_is_refused`.

### Nits

- **SEC-N1** `crates/manager-wizard/src/lib.rs:157-175`: `generate_master_key` draws from SipHash `RandomState`, not a CSPRNG. Deferred (prefer `getrandom` with a new dependency decision).
- **SEC-N2** `auth.rs:1124-1125`: recovery/enrollment code character 7 has 16, not 32, possible values (UUID v4 version nibble): 39 bits instead of 40. Deferred.
- **SEC-N3** `tls.rs:77-79`: `server.key` is plaintext PEM under `%APPDATA%`; the module doc's "phase 2 moves it to Credential Manager" never happened. Deferred (doc truth-up belongs with the decision).
- **SEC-N4** `redact.rs:262`: `sk-` matched without a word boundary (`task-scheduler1` over-redacts). Deferred (over-redaction is the safe failure).

### Reviewed and found clean

Token/hash comparisons by SHA-256 lookup (no timing oracle), argon2id parameters and dummy-hash timing parity for unknown users, error parity (locked/disabled/unknown/wrong all `InvalidCredentials`), case-insensitive lockout key, 15→30→60 min escalation, expiry checks on every validate/refresh, session revocation on role/disable changes, last-admin guard, API-token role cap at operator, pending-TOTP promotion only after verification, single-use recovery codes, all 123 route templates (no overlapping templates; percent-encoded and trailing-slash paths fail closed), argv-vector netsh/cmdkey/sc invocation (no shell injection), bundle content by server-assigned id (no traversal), CLI pin (raw 32-byte SHA-256 of the leaf, CertificateVerify signature checked), service definitions carry Credential Manager references only (secrets resolved in the shim), plaintext non-loopback bind refused.

---

## Area 2 — Persistence and state

### Blocker

**PS-1. Download / list-models deadlock.** See Top 5 #1. **Fixed in this PR: `2a9171c`** — the `downloads` guard is dropped right after the insert. Test: `download_model_releases_downloads_lock_before_config_write` (parks the op on the supervisor lock, takes a config read guard, asserts `downloads` frees; verified red before the fix).

### Major

**F2. Startup config fallback invisible and destructive.** See Top 5 #4. **Fixed in this PR: `3ecf855`** — `manager_config::load_for_startup` (on `load_detailed`) copies an unreadable primary (`.bak` path) or both files (defaults path) to `config.json.corrupt-<unix-ms>` / `config.json.bak.corrupt-<unix-ms>` before anything can save; `src-tauri` uses it and records the outcome on `AppState`; `get_status` gains an additive `config_load` field (no registry/route change) and the sidebar footer shows an error row for `backup` / `defaults` with details in the tooltip. Tests: `startup_*` and `preserve_unreadable_*` in manager-config (incl. two saves after the fallback not touching the preserved copies), `get_status_reports_config_load_outcome`. The shell wiring is gate-verified.

**PS-2. Lost updates: clone under read lock, save, install later; `save()` itself unsynchronized.** `crates/manager-app/src/optimize.rs:1623-1653` (`op_optimize_save_purpose`), `bench.rs:216-233`, `optimize_experiments.rs:101-118`; `crates/manager-config/src/lib.rs:2328-2341` (fixed temp name, no lock). Scenario: the operator saves a purpose text while the download watcher stamps `verified_at` → the stale clone overwrites the stamp in memory and on disk, and the start preflight refuses the backend until a multi-GB re-verify. **Fixed in this PR: `fb8ec4c`** (write lock held across clone-save-install at all three sites) and **`76fd0ac`** (process-wide save lock). Test: `concurrent_saves_leave_a_whole_file`; the lost-update interleaving itself is not deterministically reproducible.

**PS-3. `reload_config_from_disk` installed stale or `.bak` configs.** `crates/manager-app/src/lib.rs:1196-1200`, run after every wizard step and audit. It read the file outside the lock (reverting an edit that saved meanwhile) and accepted `load()`'s silent `.bak` fallback (the primary is briefly absent mid-rotation). **Fixed in this PR: `fb8ec4c`** — write lock first, `load_detailed`, backup-only loads ignored. Test: `reload_from_disk_ignores_backup_fallback`.

**PS-4. Wizard steps write `config.json` without AppState's lock, and save a stale snapshot over an unloadable file.** `crates/manager-wizard/src/steps.rs:1650-1697` (placement) and `:2135-2149` (`stamp_verified_at`): load-modify-save outside `AppState.config`, with `Err(_) if ctx.data_dir.is_dir() => ctx.config.clone()` saving the step's minutes-old snapshot over a config that would not load. With PS-3/PS-7 fixed the window is narrower and F2 preserves startup-time corruption, but a file that becomes unreadable mid-run is still overwritten. **Needs Pete's decision**: the clean fix changes the wizard ↔ app contract (steps return config edits for the app to apply under its lock, or receive a save callback).

**PS-5. `mcp_set_exposure` panicked for every server outside the built-in two.** `crates/manager-app/src/lib.rs:16162` (`.expect`), `:1242-1254` (`mcp_servers()` built from `default_servers()` only), stale comments `:419-421`, `:1123-1125`. Scenario: changing the exposure of the curated `brave_search` (seeded into every config) persisted the change and then panicked the command. **Fixed in this PR (partially): `c579fbe`** — no panic: leaving Local has nothing to stop, entering Local returns an explicit error; stale comments corrected. Test: `mcp_set_exposure_on_curated_server_does_not_panic`. Remaining — deriving local-process specs from `AppConfig.mcp` (so curated/operator servers and operator-edited commands can run locally, and `mcp_delete_server`/the MCP test op stop returning NotFound for them) — **Needs Pete's decision** on the exposure → mode mapping.

### Minor

**PS-6. Phantom activation-journal entries from `update_gateway_config`.** `lib.rs:4650-4666` journaled before `validate()` (the other 17 sites validate first). **Fixed in this PR: `65f19a3`**. Test: `invalid_gateway_patch_journals_nothing` (verified red before). Rolling the journal back when a *save* fails after journaling: **Needs Pete's decision** (touches the code-review 4.3 record-before-save contract).

**PS-7. `save()` could leave no `config.json` and could destroy `.bak`.** `manager-config/src/lib.rs:1609-1622, 2334-2341`. The primary was renamed to `.bak` before the new file was written; any write failure left no primary; the remove-then-rename helper could lose `.bak`; no fsync. **Fixed in this PR: `76fd0ac`** — write+fsync temp, copy primary to `.bak` via temp+rename, rename temp over primary. Test: `failed_save_keeps_the_primary_and_bak` (temp path made a directory).

**PS-8. In-place mutation before a save that may fail.** `lib.rs:15948-15957` (tool probe), `:8754-8766` (verify checksum), `:4128-4149` (download watcher, `let _ = save`), `ensure_catalog_models :1329-1486`. Memory and disk diverge on a failed save. **Deferred** (each site is a small clone-save-install conversion; none loses data with PS-3 fixed).

**PS-9. Corrupt activation journal silently reset.** `crates/manager-app/src/activation.rs:152-157`. A parse error loads `Default` and the next write erases standing offload overrides. **Deferred** (same preservation pattern as F2 is the likely fix).

**PS-10. Activation cancel ordering and guard.** `lib.rs:8079-8105` saves the restored config before clearing the journal; holds `config.write()` across the supervisor lock and a ~2.5 s gateway probe; background writes (`verified_at`, `setup_completed`) make cancel permanently refused at `activation.rs:659`. **Needs Pete's decision** (which fields cancel compares and restores).

**PS-11. Config write lock held while waiting on the supervisor.** `record_gateway_change` / `record_backend_change` under `config.write()` call `backend_start_times` (supervisor lock), which a backend start can hold for ~10 s; every status/list poll queues behind. No deadlock (no supervisor→config inversion exists). **Deferred**.

### Nit

**PS-12.** `verifying_downloads.lock().unwrap()` at `lib.rs:2185`, `bench.rs:1317, 1381` panics on a poisoned lock unlike everywhere else. Deferred.

### Reviewed and found clean

`migrate()` (explicit-`mcp` verbatim restore, curated marker from raw JSON, newer-version refusal), `validate()` collecting all errors, the activation `JOURNAL_LOCK` (poison recovery, temp+rename, no `.await` under it), no std lock held across `.await`, ~35 other mutators already holding the write lock across save and install-on-success, wizard `try_lock`, no supervisor→config lock inversion.

---

## Area 3 — Process lifecycle

### Major

**PL-M1. Service mode never captured child output.** `crates/manager-services/src/bin/llm-service-host.rs:607-637, 696-707`. The log and NUL handles were created with a null `SECURITY_ATTRIBUTES` (not inheritable) and passed with `STARTF_USESTDHANDLES` + `bInheritHandles = TRUE`, so the child got handle values that did not exist in its process: every service-mode backend/gateway/receiver log stayed empty (start-failure tails showed nothing), and a handle the child later opened could alias the dead value and receive its stdout. **Fixed in this PR: `c41e921`** (`bInheritHandle = TRUE`). Windows-only: cross-checked with clippy for `x86_64-pc-windows-gnu`; behaviour gate-verified only.

**PL-M2. SCM recovery actions can never fire; deliberate stops recorded as errors.** `llm-service-host.rs:519-544` (always reports `SERVICE_STOPPED` with the child's exit code; stop uses `TerminateJobObject(job, 1)`), `crates/manager-services/src/scm.rs:404-427` (failure actions set, `SERVICE_CONFIG_FAILURE_ACTIONS_FLAG` never set). Windows runs recovery only when the process dies without reporting STOPPED, so a crashed llama-server/LiteLLM/log receiver is never restarted — contradicting the receiver design ("SCM recovery replaces the retired VM-side watchdog") — and every operator stop shows Win32 exit 1. Setting the flag alone would make every deliberate stop restart. **Needs Pete's decision**: whether backends should auto-restart under SCM at all (process mode's crash restart is not wired either — PL-m5 — and the GPU-unavailable no-retry rule would be bypassed).

**PL-M3. Every PostgreSQL start sat out its full 60 s budget.** `crates/manager-wizard/src/ops.rs:638` (`wait_with_output` waits for pipe EOF), `steps.rs:2529, 2809-2840`. `pg_ctl --no-wait start` hands its pipe write ends to `cmd /C postgres` and the postmaster, so the run waited until the budget expired, `taskkill`ed the already-exited `pg_ctl`, logged "timed out … and was stopped" and skipped the pid-grace check (the `steps.rs:2558-2563` comment describes exactly this). Linux shares the root cause for any backgrounded grandchild. **Fixed in this PR: `7951e71`** — wait on the child, drain pipes on reader tasks, 2 s grace after exit, tree-kill-on-timeout unchanged. Test: `run_command_returns_on_child_exit_when_grandchild_holds_pipes`.

**PL-M4. Stopping the gateway could `taskkill /T /F` an unrelated program.** `ops.rs:283-299` (`stop_gateway`), `steps.rs:4230-4248` (`stop_previous_gateway`): only `process_alive(pid)` was checked before a tree kill of the pid in `gateway.pid`. Scenario: reboot → the detached gateway is gone, the pid file stays, Windows reuses the pid → "Stop gateway" or the next start kills that program and its tree. **Fixed in this PR: `7b3986c`** — a live pid must be `python.exe` (`SystemOps::process_is_gateway`, the postmaster path's image-check pattern), otherwise the pid file is treated as stale. Tests: `start_gateway_does_not_kill_a_recycled_gateway_pid`, `start_gateway_stops_previous_gateway_with_matching_image` (FakeSystemOps); the real image check is Windows-only (gate-verified).

### Minor

**PL-m1. Job object attached after the child already runs; failures ignored.** `llm-service-host.rs:701-748`. A launcher (`venv\Scripts\python.exe`) can spawn its real interpreter before the job is attached; if the job cannot be created/attached the stop handler kills nothing and the service hangs in STOP_PENDING. **Deferred** (CREATE_SUSPENDED + assign + ResumeThread rework, Windows-only).

**PL-m2. Service-mode backends adopted as orphans → false "Backend stopped unexpectedly" toast on every deliberate stop.** `crates/manager-app/src/lib.rs:2092-2117` (`adopt_orphan_backends` has no service-mode check), `notify.rs:167-203`. **Deferred**.

**PL-m3. `SupervisedProcess::kill()` waited without bound while holding the global supervisor lock.** `crates/manager-supervisor/src/lib.rs:820-821` (and tokio's awaiting `Child::kill` inside `drain`'s timeout path, `:782`). A process stuck in a driver call froze list/start/stop and the notify monitor. **Fixed in this PR: `b03051a`** — `start_kill` + reap bounded by `KILL_REAP_TIMEOUT`, `DrainTimeout` on expiry (which `stop()` already records as failed). No regression test: an unkillable process is not reproducible in CI; existing kill/drain tests pass.

**PL-m4. Stopping a supervised process kills only the direct child on Windows.** `manager-supervisor/src/lib.rs:782, 820`. A `uvx`-launched MCP server leaves its python grandchild running. **Deferred** (Windows-only; tree kill or kill-on-close job).

**PL-m5. Supervisor crash backoff, restart budget and health tracker are never used.** `crash_delay` `:1430-1450`, `probe_tracked`/`register_probe` `:1256, 1474`, `note_healthy` `:1459` have no callers outside tests; `manager-app/src/lib.rs:788-794` builds an unused `BackoffPolicy`; `Starting` is never set in process mode. A crashed backend stays Failed and `restarts` is always 0, while the module docs promise exponential crash backoff. **Needs Pete's decision**: wire an app-level restart loop, or delete the code and fix the docs.

**PL-m6. Adopted processes tracked by pid alone.** `manager-app/src/lib.rs:704-739, 809-811` (`adopted_exited` by `pid_alive`, `kill_pid_tree` with no re-check, `wait_pid_gone` result ignored in `drain`). **Deferred** (hold a process handle from adoption; Windows-only).

**PL-m7. Service registration not atomic; DACL step can be skipped silently.** `scm.rs:383-458`. A failure after `CreateServiceW` leaves a half-configured service (retry fails `ERROR_SERVICE_EXISTS`); a missing SID skips the DACL with no error; over-the-shoulder elevation grants the admin, not the operator. **Deferred / Needs Pete's decision** for the over-the-shoulder case.

### Nits

**PL-n1.** `scm.rs:306-316` treats `ERROR_SERVICE_ALREADY_RUNNING` as success after `scm_start_backend` rewrote the definition (no "restart needed" hint). Deferred.
**PL-n2.** `llm-service-host.rs:666`: `std::env::vars()` panics on a non-Unicode variable, killing the shim before it reports status. **Fixed in this PR: `c41e921`** (`vars_os`, UTF-16 passthrough). Gate-verified only.

### Carried over from the v0.1.30 review

Re-checked at HEAD; none open in original form: 2.1 (budgets) fixed; 2.2 (tree kill / blocking work) fixed except the Windows orphan case reported here as PL-M3; 2.5, 2.10, 2.11, 2.15 fixed.

### Reviewed and found clean

Backoff arithmetic, log reader line caps and CR handling, device-unavailable watchdog, `spawn_backend` port wait and log-ring archival, `reap_exited`, Windows `pid_alive`/`GetExtendedTcpTable`, `quote_arg` (CommandLineToArgvW rules), ImagePath quoting, atomic definition writes, `resolve_env` refusing missing credentials, CredUI zeroing, Unix process-group tree kill, elevated-wait result handling, download stall timeout, PostgreSQL adopt-or-start and stale-pid cleanup, gateway health (≥ 2 sustained probes), `spawn_detached` creation flags.

---

## Area 4 — Parsers, arithmetic on untrusted input, FFI

### Major

**P-1. Wrong GGUF block sizes for IQ3_XXS and IQ1_S.** `crates/manager-models/src/gguf.rs:290-291`. `IQ3_XXS` was 50 bytes per 256 elements (ggml `block_iq3_xxs`: 2 + 96 = 98) and `IQ1_S` 52 (2 + 32 + 16 = 50). Every IQ3_XXS tensor — common in Unsloth UD-IQ3_XXS quants — counted at ~51 % of its size, so the "Exact" requirement and the discovery pricing could say a model fits when it will OOM. Also `Q8_K` was 256 (real 292) and ternary `TQ1_0`/`TQ2_0` were unsupported. **Fixed in this PR: `8ec16fb`**. Test: `quant_block_sizes_match_ggml`.

**P-3. No timeout on `nvidia-smi`.** `crates/manager-telemetry/src/smi.rs:53-79`, `store.rs:375-378, 419-422`. A hung nvidia-smi (driver update, TDR, GPU off the bus) stalled the poll loop forever (GPUs only went stale, no poll error recorded) and `PollHandle::stop` waited on it. **Fixed in this PR: `88197c8`** — 15 s bound, child killed on expiry, `SmiFailed` reported. Test: `run_query_times_out_on_a_hung_binary`.

### Minor

**P-4. A GGUF header still `Incomplete` at the 256 MiB rung is never cached.** `crates/manager-app/src/lib.rs:9298-9333`, `gguf.rs:156-157`. A corrupt or crafted header re-reads and allocates 8 + 64 + 256 MiB on every `list_models`. **Deferred**.

**P-5. Any `-ot` pattern mentioning `exps=CPU` is treated as all experts.** `crates/manager-models/src/memory.rs:227-233`. The common block-range recipe `-ot "blk\.([0-9]|1[0-9])\.ffn_.*_exps\.=CPU"` prices every expert in system RAM, understating GPU use. **Needs Pete's decision** (what an unresolvable offload should return; the documented degradation is all-on-GPU at `Estimate`).

**P-7. Out-of-range ports from the LLM silently truncated.** `crates/manager-optimize/src/parse.rs:212-213, 1708-1709`, `validate.rs:84-85`. `"port": 73617` passed parse, the plan showed 73617, and validation/apply used 8081; 65536 became 0. **Fixed in this PR: `ab5c0ce`** — out-of-range ports drop the change; `get_u16` no longer truncates. Test: `out_of_range_port_drops_the_create` (with a valid-port control).

**P-8. `extract_json_object` tried only the first `{`.** `parse.rs:41-71`. "Use the {ctx} setting" or a stray unbalanced `{` before the fenced plan failed the whole consultation as `NoJsonObject`, defeating the salvage. **Fixed in this PR: `ab5c0ce`** — every `{` tried (bounded at 256), candidates must parse as objects, `parse_plan` prefers the first with an `items` array. Test: `plan_survives_braces_and_objects_in_prose`.

**P-9. nvidia-smi XML text around entity references dropped.** `smi.rs:376-392, 442` (quick-xml 0.38 emits `GeneralRef`; `insert` overwrites the split text): `R&amp;D\llama-server.exe` → `D\llama-server.exe`. Display only (pid attribution unaffected). **Deferred**.

**P-10. Uniform-share arithmetic unchecked; position table O(n²).** `memory.rs:392-403, 460-465`. Saturated header sums can overflow (panic in debug, wrap in release); a crafted header with many expert blocks makes `sys_bytes_by_position` quadratic. **Deferred**.

### Nits

**P-11a** `gguf.rs:399-400`: `blocks.div_ceil(interval)` for recurrent-layer KV counting may differ from llama.cpp by one when the block count is not a multiple of the interval (agrees for every pinned file). **P-11b** `smi.rs:276`: `contains("a100")` also matches "RTX A1000". **P-11c** `parse.rs:474` vs `experiment.rs:306`: `.GGUF` accepted case-insensitively but only lowercase stripped in `candidate_model_id`. All Deferred.

### Carried over from the v0.1.30 review

All fixed at HEAD: 1.1 (`--cpu-moe` forms), 1.2 (per-block expert sums), 1.3 (`MAX_META_DEPTH = 32`), 1.4 (checked multiplication, `u32::try_from`), 1.5, 1.7, DXGI blocker 3.1 (`Flags` added, compile-time size assert 312), 3.2–3.9.

### Reviewed and found clean

GGUF reader bounds (`take` with `checked_add`, no header-count-driven allocation), placement NaN/range checks, `set_expert_offload` token walk, `parse.rs` numeric helpers and its `expect`s (guaranteed by preconditions), DXGI vtable slots and single `Release` on every path, PDH `QueryGuard`, wide-string lifetimes, `nvml.rs` (a stub with no FFI). The "~105 unsafe sites" are mostly outside telemetry (8 there, all in `pdh.rs`); the service host's sites are covered under PL-M1/PL-m1.

---

## Area 5 — UI ↔ Rust contract

Root cause for UI-1..4: the registry's `#[tauri::command]` adapters (`src-tauri/src/main.rs:80-97`) use Tauri 2's default camelCase argument binding, so a JS key `sweep_id` never binds the Rust parameter `sweep_id` — a required parameter fails with "missing required key", an `Option` silently becomes `None`. About 70 other call sites already use camelCase correctly.

### Major

**UI-1. Bench "Cancel run" always failed.** `ui/src/api.ts:4478` sent `{ sweep_id }` to `bench_run_cancel` (`registry.rs:383`, required). No other way to cancel a sweep from the UI. **Fixed in this PR: `973553b`**.
**UI-2. Bench "Delete sweep" always failed.** `api.ts:4501` → `bench_result_delete` (`registry.rs:385`). **Fixed in this PR: `973553b`**.
**UI-3. Bench prompt generation ignored the chosen backend.** `api.ts:4442` sent `backend_id`; `registry.rs:380` `Option<String>` stayed `None`, so `bench.rs:428-450` silently used the best running backend instead. **Fixed in this PR: `973553b`**.
**UI-4. Offload "Save with override" could never save.** `api.ts:1093` set `args.allow_override`; `registry.rs:205` stayed `None`, so `lib.rs:6111` always refused a guardrail-blocked position. **Fixed in this PR: `973553b`**.
Regression coverage for UI-1..4: a new smoke check flags snake_case top-level keys in literal `invoke` argument objects and `args.<snake> =` assignments (it flags exactly these four sites on the old code).

**UI-5. Backends view leaked seven `#view` listeners per visit.** `ui/src/views/static.ts:3573, 3750, 3828, 3857, 3930, 4002, 4011` vs cleanup `:5125-5139`. After N visits each MTP / parallel / KV-cache / MoE-cache change ran N handlers with stale closures — repainting from stale rows, duplicating saves, and (MoE-cache switch) possibly writing the opposite value. **Fixed in this PR: `2329c80`**.

### Minor

**UI-6.** Groups view never removed its eight `#view` listeners (`ui/src/views/groups.ts:609-625`). **Fixed in this PR: `2329c80`**.
**UI-7.** GPUs view's anonymous process-toggle listener could not be removed (`static.ts:673-680`). **Fixed in this PR: `2329c80`**.
**UI-8.** Models `load()` did not re-check disposal after its second await, so a navigation during `list_backends` could start a 2 s poll nothing ever cleared (`static.ts:5561-5611`). **Fixed in this PR: `2329c80`**.
Regression coverage for UI-5..7: a new smoke check requires every `root.addEventListener` in a view to have a matching `removeEventListener` and refuses anonymous root handlers. UI-8 is a runtime race (no smoke coverage).

### Reviewed and found clean

All other invoke argument keys (backend action family, `backend_add`/preview, model ops, logs, placement/assign, optimize, measurement, activation history, quiesce, updates, `admin_api_*`); payload shapes field-by-field for `get_status`, `list_backends`, `get_gpu_telemetry`, `get_gpu_history`, `gateway_status`, `get_gateway_config`, `gateway_activation_plan`, `list_models`, `optimize_get_state`, `app_update_status`, `run_wizard_step`, `get_logs`, admin API status/users/audit; router teardown of superseded views; dashboard, service map, MCP, gateway, settings, activation plan, wizard, logs, bench and optimize listener/timer cleanup.

---

## Coordinator nits

**D-1. Stale comments and missing public-item docs.** The `mcp_modes` field comment claimed no MCP section exists in `AppConfig` (it does); manager-api's module doc described auth as future work; `generate_token` claimed 256 bits (244 random); two docs sat on the wrong item. 120 public items in manager-api / manager-config / manager-supervisor / manager-services had no `///` docs. **Fixed in this PR: `c579fbe`, `a8f337e`**.

**D-2. Pre-existing rustfmt drift.** `cargo fmt --all -- --check` fails on `main` (~60 files, largest `manager-app/src/lib.rs`); the gates do not run fmt. **Deferred**: one mechanical `cargo fmt --all` commit on its own branch, after this PR merges, so it does not hide review diffs.

---

## Deferred (summary)

- **F1 follow-up — hash-locked requirements** for the gateway venv (`uv pip install --require-hashes` from a generated lock): needs a lock-generation step and a refresh workflow; the version pin closes the immediate exposure.
- SEC-05 follow-up (audit-log rotation policy), SEC-13, SEC-14, SEC-15, SEC-N1..N4.
- PS-8, PS-9, PS-11, PS-12.
- PL-m1, PL-m2, PL-m4, PL-m6, PL-n1.
- P-4, P-9, P-10, P-11a..c.
- D-2 (whole-tree rustfmt).

## Needs Pete's decision

1. **SEC-03** — enforce `must_change_password` (new `PASSWORD_CHANGE_REQUIRED` 403 in the API contract; CLI/UI must route to change-password after a reset).
2. **SEC-04 (remainder)** — require password step-up for first TOTP enrollment (client flow change).
3. **SEC-08** — stop regenerating the TLS certificate on SAN drift at startup; refresh SANs only via Regenerate.
4. ~~**SEC-16** — refuse non-loopback `http://` log-streaming endpoints.~~ Decided and fixed (see SEC-16 above).
5. **PS-4** — how wizard steps should persist config edits (return edits to the app vs. a locked save callback; fail the step instead of saving a snapshot over an unloadable file).
6. **PS-5 (remainder)** — derive MCP local-process specs from `AppConfig.mcp` (exposure → mode mapping).
7. **PS-6 (remainder) / PS-10** — journal rollback on a failed save, and which fields activation cancel compares/restores.
8. **PL-M2 / PL-m5** — crash-restart policy: SCM recovery for services (stop-reason plumbing + failure-actions flag) and whether process mode should get the unused backoff/restart loop or lose the dead code.
9. **PL-m7** — DACL grantee under over-the-shoulder elevation.
10. **P-5** — what an unresolvable `-ot` offload should report.
11. **`--n-cpu-moe N` semantics (unverified, not counted as a finding).** A reviewer believes llama.cpp offloads experts of *block indices* `< N` (so dense blocks count toward N, and repeated flags accumulate), while `memory.rs:386-403, 460-465` prices "the first N MoE blocks" and treats repeats as last-wins. That would understate GPU use for interleaved/leading-dense MoE models (e.g. the Nemotron catalog entry). An attempt to confirm against `common/arg.cpp` reached only `llm_add_n_cpu_ffn_overrides(value, LLM_FFN_EXPS_REGEX, …)` without its body, so this is not verified. Worth a check against the pinned llama.cpp build before changing slider semantics or catalog pins.
12. **F1 consequence** — "Unpin" on the LiteLLM component now re-pins to 1.105.1 at the next load. Moving LiteLLM forward means setting a newer explicit pin; if an unpinned "track latest" mode is wanted, it should come with hash-locking.
