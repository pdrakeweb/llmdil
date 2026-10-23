# Log receiver cutover — Pete's on-machine checklist

Cutover phases 1–2 of the Prometheus-local log receiver
(design: `~/workspace/llmdil-log-receiver/DESIGN-prometheus-local.md`,
design copy in the repo at the project root history). The VM
receiver stays live throughout — rollback at every step is "use
the old endpoint string".

## Phase 1 — deploy dark (receiver runs, shipper unchanged)

1. **Update LLMdil to v0.1.52** (the release that ships the
   receiver binaries as release assets).
2. **Install Tailscale on Prometheus** and sign in (same tailnet
   as Muse's VM). Confirm the address:
   `tailscale ip -4` → a 100.64.x.x – 100.127.x.x address. Note it;
   it goes into the VM-side reader config (below).
3. **Download the deploy assets** from the v0.1.52 release page:
   `Deploy-LogReceiver.ps1` and this checklist (the two `.exe`
   assets are fetched by the script itself).
4. **Run the script elevated** (right-click → Run as
   administrator), from the folder holding it:

   ```powershell
   powershell -ExecutionPolicy Bypass -File .\Deploy-LogReceiver.ps1
   ```

   It downloads the binaries beside the app, seeds the token
   files (ingest token read from the same Credential Manager entry
   the shipper already uses — nothing to type), registers and
   starts the `llm-log-receiver` service (auto-start,
   LocalService), adds the firewall rule for the read listener
   (TCP 8474, tailnet sources only), and verifies end-to-end with
   a synthetic record.
   - When it prints the NEW READ TOKEN (first run only): file
     Muse's copy as `~/.config/llmdil-logs/read-token` on the VM
     (mode 600), via the Secure Vault — never in chat or the repo.
   - If Tailscale was not yet joined, the script warns and skips
     only the read-listener verification; re-run after joining.
5. **Approve the VM's first request** to Prometheus's tailnet IPv4
   when prompted (one-time per-address approval on the Muse
   side). Then Muse runs the synthetic read-back check:
   `/v1/status` shows the receiver heartbeat fresh and the
   `synthetic-e2e` machine present.
6. **Expect**: LLMdil logs still flow to the VM receiver (the
   shipper is untouched); the local store gains only synthetic
   records. Leave it in this state for the 48-hour soak window
   shared with phase 2.

## Phase 2 — flip the endpoint (your go-ahead, any time after phase 1)

1. Re-run the script with the flip switch:

   ```powershell
   powershell -ExecutionPolicy Bypass -File .\Deploy-LogReceiver.ps1 -FlipEndpoint
   ```

   It backs up `config.json` (timestamped copy beside it) and sets
   `log_streaming.endpoint` to `http://127.0.0.1:8473`. Nothing
   else in the config changes; token, spool, batching, redaction
   are untouched.
2. **Restart LLMdil.** (Or make the same one-line edit by hand
   and restart.)
3. **Verify with Muse**: `/v1/status` over Tailscale shows
   `last_received_at` for machine `prometheus` advancing; trigger
   a known LLMdil event (a wizard run) and confirm the record is
   queryable through the read listener. A few minutes of ordinary
   operation ends the verification.
4. **Rollback** (any problem): set the endpoint back to
   `https://llmdil-logs.ourdrakefamily.com` and restart LLMdil —
   the VM receiver has been live the whole time; the shipper
   spools losslessly across the gap.

## After the flip (48-hour soak, then a later retirement job)

- Phase 3: soak. The VM's 15-minute health poll and the local
  `/v1/status` view cover the window.
- Phase 4 (separate job, needs your confirmation at the time):
  delete the `llmdil-logs` Cloudflare CNAME, remove the tunnel
  ingress, stop the VM receiver and its watchdog.

## Uninstall (if ever needed)

```powershell
& "$env:LOCALAPPDATA\LLMdil\llmdil-log-receiver.exe" uninstall-service
Remove-NetFirewallRule -Name 'LLMdilLogReceiverRead'
```

The store and tokens stay in
`%APPDATA%\local-llm-service-manager\log-receiver\` — delete that
folder by hand to remove them.
