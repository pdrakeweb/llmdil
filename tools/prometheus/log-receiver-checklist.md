# Log receiver cutover — Pete's on-machine checklist

Cutover phases 1–2 of the Prometheus-local log receiver (design:
`~/workspace/llmdil-log-receiver/DESIGN-prometheus-local.md`).
Since v0.1.53 the receiver ships inside the LLMdil installer and
deploys from the app — no scripts to download or run. The VM
receiver stays live throughout — rollback at every step is "use
the old endpoint string".

## Phase 1 — deploy dark (receiver runs, shipper unchanged)

1. **Install or update LLMdil to v0.1.53 or later.** The installer
   places `llmdil-log-receiver.exe` and `llm-service-host.exe`
   beside the app in `%LOCALAPPDATA%\LLMdil`.
2. **Install Tailscale on Prometheus** and sign in (same tailnet
   as Muse's VM). Confirm the address:
   `tailscale ip -4` → a 100.64.x.x – 100.127.x.x address. Note it;
   it goes into the VM-side reader config (Muse's side).
3. **Deploy from the app**: Settings → Supervision → Log receiver
   → **Deploy**. Approve the UAC prompt. Deploy creates the
   receiver's data directory and ACLs, seeds its token files
   (ingest token read from the same Credential Manager entry the
   shipper already uses — nothing to type; the read token is
   generated fresh), registers and starts the `llm-log-receiver`
   service (auto-start, LocalService), adds the firewall rule for
   the read listener (TCP 8474, tailnet sources only), and
   verifies localhost ingest health. Re-running Deploy later is
   the repair path: it re-registers the service and keeps the
   existing read token.
   - When the deploy finishes and the card shows a **new read
     token** (first deploy only): file Muse's copy as
     `~/.config/llmdil-logs/read-token` on the VM (mode 600), via
     the Secure Vault — never in chat or the repo.
   - Command-line equivalent (scripted use only):
     `tools/prometheus/Deploy-LogReceiver.ps1` now just invokes
     the installed binary's `deploy` subcommand elevated.
4. **Approve the VM's first request** to Prometheus's tailnet IPv4
   when prompted (one-time per-address approval on the Muse
   side). Then Muse runs the synthetic read-back check:
   `/v1/status` shows the receiver heartbeat fresh.
5. **Expect**: LLMdil logs still flow to the VM receiver (the
   shipper is untouched); the local store gains only service
   heartbeats. Leave it in this state for the 48-hour soak window
   shared with phase 2.

## Phase 2 — flip the endpoint (your go-ahead, any time after phase 1)

1. On the same card: **Flip endpoint to local receiver**. The app
   backs up `config.json` (timestamped copy beside it) and sets
   `log_streaming.endpoint` to `http://127.0.0.1:8473` through its
   validated config save. Nothing else in the config changes;
   token, spool, batching, redaction are untouched. The flip is a
   separate deliberate action — Deploy never flips.
2. **Restart LLMdil** to apply.
3. **Verify with Muse**: `/v1/status` over Tailscale shows
   `last_received_at` for machine `prometheus` advancing; trigger
   a known LLMdil event (a wizard run) and confirm the record is
   queryable through the read listener. A few minutes of ordinary
   operation ends the verification.
4. **Rollback** (any problem): set the endpoint back to
   `https://llmdil-logs.ourdrakefamily.com` (Settings → Log
   streaming) and restart LLMdil — the VM receiver has been live
   the whole time; the shipper spools losslessly across the gap.

## After the flip (48-hour soak, then a later retirement job)

- Phase 3: soak. The VM's 15-minute health poll and the local
  `/v1/status` view cover the window.
- Phase 4 (separate job, needs your confirmation at the time):
  delete the `llmdil-logs` Cloudflare CNAME, remove the tunnel
  ingress, stop the VM receiver and its watchdog.

## Removing the receiver (if ever needed)

- From the app: Settings → Supervision → Log receiver →
  **Remove** (one UAC prompt) — stops and unregisters the service
  and deletes the firewall rule.
- Uninstalling LLMdil also attempts the removal: the uninstaller
  runs the same remove step elevated when the service is present
  (declining the prompt leaves the service registered, with an
  on-screen instruction for removing it manually).
- The store and tokens stay in
  `%APPDATA%\local-llm-service-manager\log-receiver\` — delete
  that folder by hand to remove them.
