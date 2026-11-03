# manager-cli — CLI/TUI client for the LLMdil Admin API

`llmdil-cli` talks to the LLMdil Admin API (v1, audited Stage 1:
124 live routes) from any machine on the LAN. It serves as the
Stage 2 test client (pairing + read verification from a second
LAN machine) and as an ongoing operator tool. It changes no
server behavior: every command maps to an existing API route.

## Connection: pinned TLS, no exceptions

Every command requires a base URL and a certificate fingerprint:

```
llmdil-cli --base-url https://192.168.5.23:47821 \
  --fingerprint <64 hex chars> <command>
```

The fingerprint is the **Certificate SHA-256** row the LLMdil UI
shows under Remote Access (hex SHA-256 of the certificate DER).
The client compares it against the presented certificate and
also verifies the TLS CertificateVerify signature against that
certificate's public key — a matching fingerprint alone is not
enough, because certificates are public data. A mismatch aborts
the connection. There is no click-through, no `--insecure`
flag, and no hostname check: the fingerprint IS the server
identity. The same flags can come from `LLMDIL_BASE_URL` and
`LLMDIL_FINGERPRINT`.

## Secrets: never as arguments, never on disk

No secret is ever a command-line argument, and no token is ever
written to disk or a config file. Tokens live in memory only.

| Secret | Environment | Stdin flag |
|---|---|---|
| Access token | `LLMDIL_API_TOKEN` | `--token-stdin` |
| Refresh token | `LLMDIL_API_REFRESH_TOKEN` | (refresh command: `--token-stdin`) |
| Password | `LLMDIL_API_PASSWORD` | `--password-stdin` |
| Enrollment code | `LLMDIL_ENROLL_CODE` | `--code-stdin` |
| TOTP code | `LLMDIL_TOTP_CODE` | `--totp-stdin` |

Only one `--*-stdin` flag per invocation: stdin is read once
(first line, newline trimmed). `auth login`, `auth enroll`,
`auth totp`, and `auth refresh` print the token pair as JSON to
stdout so it can be piped into the next command's stdin or
environment — never redirect that output to a file. On a 401
the client rotates the held refresh token once and retries the
request once; a failed rotation clears the held tokens.

## Enroll / pairing flow

1. In the LLMdil desktop UI (Remote Access), create the first
   admin user if none exists, enable Remote Access, and mint an
   enrollment code (admin users can also mint codes via
   `POST /api/v1/admin/enrollment-codes`).
2. From the second machine, copy the **Certificate SHA-256** row
   from the same UI page — that is the `--fingerprint` value.
3. Redeem the code (one-time; the code is worthless after use):

```
LLMDIL_ENROLL_CODE=<code> llmdil-cli --base-url https://192.168.5.23:47821 \
  --fingerprint <sha256> auth enroll --device-name my-laptop
```

The command prints the token pair as JSON. Pipe the access
token into later commands:

```
llmdil-cli --base-url https://192.168.5.23:47821 \
  --fingerprint <sha256> --token-stdin backends <<< "$LLMDIL_API_TOKEN"
```

Or export `LLMDIL_API_TOKEN` for the shell session (never put
it in a dotfile). There is no QR scanning in v1: the code and
the fingerprint are entered as text.

## Scripted verification mode (Stage 2 test client)

`verify` enrolls, then runs a full read sweep — identity,
status, backends, gateway, GPUs, models, logs — printing
PASS/FAIL per step. The exit code is 0 only when every step
passes. Non-interactive, no TUI, designed to run from a second
LAN machine:

```
LLMDIL_ENROLL_CODE=<code> llmdil-cli --base-url https://192.168.5.23:47821 \
  --fingerprint <sha256> verify
```

## Commands

Reads print pretty JSON. `server` is public (no token);
everything else needs a token.

```
server | status | backends | backend <id>
backend-start <id> [--allow-wddm-overcommit]
backend-stop <id> [--graceful] [--yes]
backend-restart <id> [--graceful] [--yes]
gateway-status | gateway-start | gateway-stop [--yes] | gateway-config
gpus | telemetry-history <range>
models | model <id>
logs [--scope all] [--level L] [--search Q] [--limit N] | logs-scopes
ops | op <id> | op-poll <id> | op-abort <id>
activation | activation-apply [--mode now|drain] [--yes] | activation-history
auth enroll | auth login --username <u> | auth totp --challenge-id <id>
auth me | auth logout | auth sessions | auth session-revoke <id> | auth refresh
tui
```

Control commands return a 202 operation record and poll it to
a terminal state on the API cadence (1 s for the first 10 s,
then 5 s). A 409 BUSY is reported as "already running", not an
error. Destructive commands (stop, restart, activation apply)
ask for confirmation on a TTY unless `--yes` is given; a
non-TTY without `--yes` refuses rather than guessing.

## TUI

`tui` opens a dashboard with tabs for Status, Backends,
Gateway, GPUs, and Logs, refreshing every 5 s (`r` refreshes
now, `Tab`/`1`–`5` switch tabs, `q`/`Esc` quits). Field
extraction is defensive: missing or new server fields fall
back to raw JSON instead of breaking the UI.

## Packaging (separate asset, never in the app installer)

`manager-cli` is a crate in the LLMdil repo, but it is NOT
bundled into the main LLMdil installer payload (Pete, Nov 3,
2026). The installer bundles only its explicit `externalBin`
sidecars (`llm-service-host`, `llmdil-log-receiver`). On each
release, `release.yml` builds `manager-cli` and uploads the
standalone binary as a separate release asset,
`LLMdil-cli_<version>_x64.exe`, alongside the installer. The
CLI needs no installation: download the asset and run it.
