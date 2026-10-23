# Log Streaming

Stream LLMdil's logs, redacted, to a destination Muse can review
in near realtime (design: `log-streaming-design.md`, decisions
D1–D10). Off by default (D5); push-based, egress-only; works
identically in the interactive app and in Session 0 service mode.

## MVP scope

- Sources: the `wizard` and `optimize` file tails (§2.3's S3/S4).
  The remaining sources (gateway, app events, service-manage,
  audit, per-backend rings) are Phase 2; their config toggles exist
  and are stored, but their taps are not wired yet.
- Transport: the self-hosted receiver (Option B) — a small service
  on Muse's VM (`~/workspace/llmdil-log-receiver/`) behind a
  Cloudflare tunnel hostname; Muse reads its SQLite store with
  the `llmdil-logs` CLI (`tail`, `search`, `status`).
- Shipper: crate `manager-logship`, one background task in the app
  process. Bounded 1,024-record queue (drop-newest, counted),
  batches of 100 records or 2 s, gzip JSON POSTs, 50 MiB disk
  spool with in-order drain on recovery; spool-cap drops are
  counted and the count is itself shipped on recovery.

## Enabling (MVP: config file + credential)

1. Store the destination's per-machine ingest token in Windows
   Credential Manager under the target
   `local-llm-service-manager/log-streaming-token` (D6 — the token
   never lives in `config.json`).
2. In `config.json`, add the section:

   ```json
   "log_streaming": {
     "enabled": true,
     "transport": "receiver",
     "endpoint": "https://llmdil-logs.ourdrakefamily.com",
     "machine_name": "prometheus",
     "level_floor": "info",
     "sources": { "wizard": true, "optimize": true }
   }
   ```

3. Restart the app. The shipper starts after config load; its own
   local log is `<data_dir>/logs/shipper.log` (never shipped).

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

14 days or 2 GiB, whichever first, pruned hourly by the receiver;
every prune is recorded in the store's `prune_log` table.
