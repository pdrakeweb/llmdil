# Log Streaming

Stream LLMdil's logs, redacted, to a destination Muse can review
in near realtime (design: `log-streaming-design.md`, decisions
D1–D10; placement per the Prometheus-local receiver design,
Oct 2026). Off by default (D5); push-based, egress-only; works
identically in the interactive app and in Session 0 service mode.

## MVP scope

- Sources: the `wizard` and `optimize` file tails (§2.3's S3/S4).
  The remaining sources (gateway, app events, service-manage,
  audit, per-backend rings) are Phase 2; their config toggles exist
  and are stored, but their taps are not wired yet.
- Transport: the self-hosted receiver (Option B) — crate
  `manager-logreceiver`, running on the same machine as the app as
  its own auto-start Windows service (registered through
  `manager-services`; deploy:
  `tools/prometheus/Deploy-LogReceiver.ps1`). Ingest is localhost;
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

1. Deploy the receiver service on this machine:
   `tools/prometheus/Deploy-LogReceiver.ps1` (elevated) installs
   the service, seeds its token files under
   `<data_dir>/log-receiver/`, and verifies ingest + read-back with
   a synthetic record. The receiver's ingest token is seeded from
   the same Credential Manager value the shipper uses (step 2).
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
route — and requires a separate read-only Bearer token (seeded by
the deploy script to `<data_dir>/log-receiver/read-token.txt`).
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
