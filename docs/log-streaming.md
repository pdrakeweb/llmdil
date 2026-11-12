# Log Streaming

Stream LLMdil's logs, redacted, to a destination Muse can review
in near realtime (design: `log-streaming-design.md`, decisions
D1–D10; placement per the Prometheus-local receiver design,
Oct 2026). Off by default (D5); push-based, egress-only; works
identically in the interactive app and in Session 0 service mode.

## MVP scope

- Sources: the `wizard` and `optimize` file tails (§2.3's S3/S4).
  The remaining sources (gateway, app events, service-manage,
  audit, per-backend logs) landed in Phase 2 — see "Phase 2" below;
  every `sources` toggle now controls a real tap.
- Transport: the self-hosted receiver (Option B) — crate
  `manager-logreceiver`, running on the same machine as the app as
  its own auto-start Windows service (registered through
  `manager-services`). The receiver binary ships inside the LLMdil
  installer beside the app; deployment is driven from the app
  (Settings → Supervision → Log receiver → Deploy, one UAC
  prompt) — `tools/prometheus/Deploy-LogReceiver.ps1` remains only
  as a thin wrapper over the same path. Ingest is localhost;
  Muse reads the store remotely over Tailscale (see "Read
  surface" below). The receiver was originally hosted on Muse's
  VM behind a Cloudflare tunnel hostname; that deployment retired
  with the Prometheus-local move (the VM store's records were
  synthetic test traffic only).
- Shipper: crate `manager-logship`, one background task in the app
  process. Bounded 1,024-record queue (drop-newest, counted),
  batches of 100 records or 2 s, gzip JSON POSTs, 50 MiB disk
  spool with in-order drain on recovery; spool-cap drops are
  counted and the count is itself shipped on recovery.

## Enabling (config file + credential + receiver service)

1. Deploy the receiver service on this machine: Settings →
   Supervision → Log receiver → **Deploy** (one UAC prompt;
   Repair re-runs the same path). It seeds the token files under
   `<data_dir>/log-receiver/`, registers and starts the service,
   adds the read listener's firewall rule, and verifies localhost
   ingest health. The receiver's ingest token is seeded from the
   same Credential Manager value the shipper uses (step 2). The
   shipper endpoint flip to `http://127.0.0.1:8473` is the separate
   Flip action on the same card — Deploy never flips. Remove (and
   uninstalling LLMdil, which attempts the same removal) reverses
   the deployment; the store and tokens stay in the data dir.
2. Store the per-machine ingest token in Windows Credential
   Manager under the target
   `local-llm-service-manager/log-streaming-token` (D6 — the token
   never lives in `config.json`).
3. In `config.json`, add the section:

   ```json
   "log_streaming": {
     "enabled": true,
     "transport": "receiver",
     "endpoint": "http://127.0.0.1:8473",
     "machine_name": "prometheus",
     "level_floor": "info",
     "sources": { "wizard": true, "optimize": true }
   }
   ```

4. Restart the app. The shipper starts after config load; its own
   local log is `<data_dir>/logs/shipper.log` (never shipped).

## Read surface (Muse, over Tailscale only)

The receiver's second listener binds the machine's tailnet IPv4
(100.64.0.0/10) on port 8474 — never the LAN, never a public
route — and requires a separate read-only Bearer token (generated
by the deploy to `<data_dir>/log-receiver/read-token.txt` and
shown once in the app for Secure Vault filing).
Endpoints, all GET, all JSON:

- `/healthz` — `{ok, records, db_bytes, last_heartbeat_utc}`.
- `/v1/status` — receiver uptime/counts/last heartbeat plus
  per-machine `{count, max_seq, last_received_at}`. The receiver
  writes a heartbeat row every 5 minutes, so a fresh heartbeat
  with a stale `last_received_at` means "receiver up, shipper
  quiet"; an unreachable endpoint means the machine, Tailscale, or
  the receiver service is down (the endpoint cannot distinguish
  those three).
- `/v1/query` — `source` (csv), `level`, `grep`, `since` (RFC
  3339), `limit` (default 200, max 5000); records newest-last.

## Redaction (design §5)

Every line is redacted in the shipper, on this machine, before
serialization — there is no configuration in which unredacted lines
leave the machine. Layer 1 replaces the deployment's known secret
values (the ingest token, the LiteLLM master key, the Postgres
password, the OpenRouter key, MCP servers' stored credential
values — read from Credential Manager at start). Layer 2 applies
pattern classes: `Authorization:` values, `Bearer` tokens, `sk-`
keys, secret-bearing key–value pairs (JSON, `key=value`, URL query
and DSN parameters), JWT shapes, and URI userinfo passwords.
Replacement preserves the key and shape (`"api_key": "«redacted»"`);
lines are never dropped by redaction. The proof is the sentinel
test (`crates/manager-logship/tests/sentinel.rs`), a gate for any
build touching the shipper or redactor: seeded sentinels of every
class must appear in no captured byte, while every seeded line must
be delivered.

## Retention (receiver, D8)

14 days or 512 MiB, whichever first, pruned hourly by the
receiver; every prune is recorded in the store's `prune_log`
table. Heartbeats prune separately at 7 days.

## Phase 2: the remaining sources

The design document this file cites (`log-streaming-design.md`) is
not in the repository; this section records what Phase 2 built, and
with the code is the specification for it.

### Sources and toggles

Every field of `log_streaming.sources` maps to a tap. A source that
is toggled off is not tailed and not subscribed.

| Toggle (default) | Source name on the wire | Input |
|---|---|---|
| `wizard` (on) | `wizard` | `logs/wizard.log` |
| `optimize` (on) | `optimize` | `logs/optimize.log` |
| `gateway` (on) | `gateway`, `stream` = `stdout` / `stderr` | `logs/gateway.stdout.log`, `logs/gateway.stderr.log` |
| `service_manage` (on) | `service-manage` | `logs/service-manage.log` |
| `audit` (on) | `audit` | `logs/admin-api-audit.jsonl` |
| `app` (on) | `app` | in-process feed (operation events, notification-monitor transitions) |
| `backends` (off) | `backend:<id>`, `stream` = `stdout` / `stderr` | service mode: `logs/backend-<id>.stdout.log` / `.stderr.log`; process mode: the supervisor's per-backend ring |

All paths are under `<data_dir>`. The wire format (`WireRecord`,
version 1) is unchanged: sources that have two streams use the
existing `stream` field.

- **Gateway (S2).** The gateway's captured output rotates to
  `<file>.prev` on every gateway spawn; the tail drains the
  unread remainder from `.prev` and starts the new file at its
  first line.
- **Service-manage (S6).** Written by the elevated
  `llm-service-host` helper, which resolves the data dir from the
  service definition's path (`<data_dir>/services/<name>.json`), so
  it lands in the same `logs` directory. Append-only; its writer
  never rotates it.
- **Audit (S7).** One JSON record per line, shipped verbatim
  through redaction. The level comes from the record's `result`:
  an HTTP status maps 5xx → error, 4xx → warn, otherwise info; an
  auth outcome word is info for `ok` / `totp_required` and warn
  otherwise. A line with no `result` falls back to the line
  heuristic. Append-only; never rotated.
- **App events (S5).** There is no single app-wide event
  broadcast: operations emit through the per-surface `EventSink`
  (the Tauri sink on the desktop, the broadcast sink in the admin
  API). The tap sits where both meet — `registry::dispatch` wraps
  the sink it is given — so an operation's events ship whichever
  surface ran it. The update checker's sink, which bypasses
  dispatch, is wrapped in the shell. Notification-monitor
  transitions (backend died, gateway down, activation resolved) are
  recorded at the `notify` choke point, before the operator's
  notification switches. A record's line is
  `<event> <payload JSON>`. Telemetry samples are not shipped;
  download-progress ticks ship at debug (below the default floor).
- **Per-backend (S1, opt-in, D3).** The config has no per-backend
  switch, so `sources.backends` covers every configured backend. A
  ring that already holds output when the app starts ships its last
  200 lines. A backend produces lines one way only — a service-mode
  backend's output is captured by its service host, not by the
  app's supervisor — so nothing ships twice.

### Rotation, truncation, restart

Offsets persist per tailed file in the spool's `state.json`
(`offsets`), now with a fingerprint of the consumed region
(`fingerprints`; older state files load unchanged). Files with no
`.prev` convention (service-manage, audit, service-mode backend
logs) are append-only and never rotated by their writers; if one is
truncated or replaced, the tail restarts it from the first line —
nothing re-shipped, nothing lost. The persisted fingerprint also
catches a file rotated or replaced while the app was down by one
already longer than the saved offset.

### In-process feed

The app hands the shipper records over a bounded broadcast feed
(1,024 records) rather than writing them to a file to tail.
Sending never blocks and is a no-op while no shipper is subscribed.
If the shipper falls behind, the oldest records are lost and the
shipper ships one warning record (source `shipper`) carrying the
dropped count, regardless of the level floor; the count is also in
the status payload (`dropped_events`).

Feed records and tailed lines take one intake path: level floor,
redaction, sequencing, the 1,024-record queue, batching, and the
spool cap all apply to the new sources unchanged.

### Status

`log_receiver_status` (Settings → Supervision → Log receiver) carries
a `shipper` object: the shipper's state and counters, and one row
per source —

- `active`: tailed or subscribed, and its input exists;
- `disabled`: toggled off;
- `file_missing`: on, but its log file does not exist;
- `waiting`: a backend with no service log and no captured output
  yet;
- `unavailable`: on, but the process gave the shipper no feed;
- `not_running`: on in the config, but the shipper is not running
  (streaming off, no endpoint, or no token).

### Applying changes

The shipper reads the config once, when the app starts; there is no
restart path short of restarting the app. A saved toggle that
differs from what the running shipper started with is reported as
`restart_pending` and shown on the card. Backends added to the
config after start ship from their ring once they run (process
mode); their service-mode files are tailed from the next start.

### Not in Phase 2

- No live apply of toggle changes (above).
- No Settings editor for the toggles: they remain the `sources`
  section of `config.json`.
- SEC-16 (refusing non-loopback `http://` endpoints) is an open
  decision and is not implemented.
