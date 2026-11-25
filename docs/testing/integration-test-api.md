# Integration test: Admin REST API

The same cases as [integration-test-ui.md](integration-test-ui.md), run
over HTTP by a person or an agent. Case ids are identical, so a UI run and
an API run can be compared line by line. Windows services have their own
plan in [windows-services-test-plan.md](windows-services-test-plan.md).

Every path, method, body field and response field below was checked
against the code at commit `8187eb6` (registry, API layer, operations).
Nothing here was run against a live app yet. Where the API documents
disagree with the code, this file follows the code; the differences are
in [appendix A](#appendix-a-documents-that-disagree-with-the-code).

How to read a case:

| Mark | Meaning |
|---|---|
| `Gnn` | The API has no way to do this: [section 16](#16-api-gaps) |
| `Dnn` | A route exists but behaves in a way that breaks automation: [section 17](#17-defects-that-break-automation) |
| N/A (screen-only) | Only the window can show it |
| **Expected / Today** | Both are given when the code at this commit does not do what the case asks. Record **FAIL** when today's answer comes back |

## 1. What you need

Everything in section 1 of the UI test, plus:

| Need | Why |
|---|---|
| PowerShell 7.4 or newer, **on the managed machine** | The second source is local: files, processes, `nvidia-smi`, llama-server ports |
| The Admin API switched on by the owner | Off by default |
| A bearer token in `LLMDIL_TOKEN` | Every route except `/server`, login, refresh and enroll |
| A gateway key in `LLMDIL_GATEWAY_KEY` | Chat cases only |

### 1.1 The owner switches the API on

Owner only, in the app: *Settings → Remote access*.

1. *Listen on*: **This computer only (127.0.0.1)**, *Apply*. No firewall
   rule is needed for that. The default is all interfaces.
2. Create an administrator if there is none.
3. Switch *Admin API* on. Note *Base URL* (default
   `https://127.0.0.1:47821`) and *Certificate SHA-256* (64 hex digits).
4. Create a user for the test, role below, and press *Pairing code*
   (single use, 10 minutes).

| Role | Enough for |
|---|---|
| viewer | Every `GET` except launch command, bundle content and `/admin/*`; previews; `POST /updates/check`, `/placement/propose`, `/gateway/routing/test`, `/mxc/self-test` |
| operator | + start, stop, restart, test, apply the plan, gateway restart/stop, downloads, peaks and snapshots |
| configurator | + add and edit backends and groups, flags, placement, gateway config, pins |
| **admin** | + delete a backend or model, **cancel the plan**, `POST /gateway/start`, launch command, diagnostics, MXC, component update, audit log |

The whole test needs **admin**. With configurator, skip the cases marked
`admin`.

An **API token** (`POST /admin/api-tokens`) lasts longer but is limited to
viewer or operator by design, so it cannot run this test (G21). The test
uses a **session**: the access token lasts 60 minutes, the refresh token
30 days.

### 1.2 The owner supplies the secrets

The tester never creates accounts, never types or sees a password, never
redeems a pairing code. The owner runs this once in the PowerShell window
the test will use, after loading the helpers from 1.3:

```powershell
$env:LLMDIL_BASE_URL    = 'https://127.0.0.1:47821'
$env:LLMDIL_FINGERPRINT = '<Certificate SHA-256 from the Remote access tab>'
. .\llmdil-test.ps1
Set-LlmdilTokenFromPairing                       # asks for the code, masked
$env:LLMDIL_GATEWAY_KEY = Read-Host -MaskInput 'Gateway key'
```

- Use **one** PowerShell process for the run and start an agent from it,
  so it inherits the variables. Presenting a refresh token that was
  already rotated revokes the whole session (`401 SESSION_REVOKED`).
- Never print, log or save `LLMDIL_*` values. No transcript
  (`Start-Transcript`), no `-Verbose` or `-Debug` on web calls.
- The CLI (`crates/manager-cli`) uses other names
  (`LLMDIL_API_TOKEN`, `LLMDIL_API_REFRESH_TOKEN`). It has no
  configuration commands, so it is not used here.

### 1.3 Pinning and helpers

The certificate is self-signed. The helpers compare the SHA-256 of the
certificate the server presents with the fingerprint from the app and
refuse anything else. **Never** use `-SkipCertificateCheck` or `curl -k`:
they accept any certificate and hand the token to whoever answers. If the
pin fails, stop and ask the owner; a new fingerprint comes from the app's
window, not from the connection.

Save as `llmdil-test.ps1` outside the repository.

```powershell
#Requires -Version 7.4
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

if (-not ('LlmdilClient' -as [type])) {
Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Net.Http;
using System.Net.Http.Headers;
using System.Security.Cryptography;
using System.Text;
using System.Threading;

public sealed class LlmdilResponse {
    public int Status;
    public string ContentType = "";
    public string RequestId = "";
    public byte[] Bytes = Array.Empty<byte>();
    public string Text { get { return Encoding.UTF8.GetString(Bytes); } }
}

public sealed class LlmdilClient {
    readonly HttpClient http;
    public LlmdilClient(string baseUrl, string pinHex) {
        var uri = new Uri(baseUrl.TrimEnd('/'));
        if (uri.Scheme != "https") throw new ArgumentException("the base URL must be https");
        byte[] pin = Convert.FromHexString(pinHex.Trim());
        if (pin.Length != 32) throw new ArgumentException("the fingerprint must be 64 hex characters");
        var h = new HttpClientHandler { AllowAutoRedirect = false, UseProxy = false };
        // Trust exactly one certificate. TLS itself proves the server holds its key.
        h.ServerCertificateCustomValidationCallback = (msg, cert, chain, errors) =>
            cert != null && CryptographicOperations.FixedTimeEquals(SHA256.HashData(cert.RawData), pin);
        http = new HttpClient(h) { BaseAddress = uri, Timeout = Timeout.InfiniteTimeSpan };
    }
    public LlmdilResponse Send(string method, string path, string token, string json, int timeoutSec) {
        using var req = new HttpRequestMessage(new HttpMethod(method), path);
        if (!string.IsNullOrEmpty(token)) req.Headers.Authorization = new AuthenticationHeaderValue("Bearer", token);
        if (!string.IsNullOrEmpty(json)) req.Content = new StringContent(json, Encoding.UTF8, "application/json");
        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(timeoutSec));
        using var resp = http.Send(req, HttpCompletionOption.ResponseContentRead, cts.Token);
        var r = new LlmdilResponse { Status = (int)resp.StatusCode };
        if (resp.Content.Headers.ContentType != null) r.ContentType = resp.Content.Headers.ContentType.MediaType ?? "";
        if (resp.Headers.TryGetValues("x-request-id", out var ids)) r.RequestId = string.Join(",", ids);
        using var ms = new MemoryStream();
        resp.Content.ReadAsStream(cts.Token).CopyTo(ms);
        r.Bytes = ms.ToArray();
        return r;
    }
}
'@
}

foreach ($n in 'LLMDIL_BASE_URL', 'LLMDIL_FINGERPRINT') {
    if (-not [Environment]::GetEnvironmentVariable($n)) { throw "the owner must set $n first" }
}
$script:Client  = [LlmdilClient]::new($env:LLMDIL_BASE_URL, $env:LLMDIL_FINGERPRINT)
$script:Results = [Collections.Generic.List[object]]::new()

# One call. Never throws on an HTTP status; throws only when there is no
# HTTP answer (refused, timeout, wrong certificate).
function Invoke-Llmdil {
    param(
        [Parameter(Mandatory)][ValidateSet('GET','POST','PUT','PATCH','DELETE')][string]$Method,
        [Parameter(Mandatory)][string]$Path,       # '/backends', without /api/v1
        $Body = $null,                             # hashtable or JSON text; omit for no body
        [int]$TimeoutSec = 330,
        [switch]$Anonymous,
        [string]$TokenVar = 'LLMDIL_TOKEN'         # the NAME of the variable
    )
    $json = $null
    if ($null -ne $Body) {
        $json = if ($Body -is [string]) { $Body } else { ConvertTo-Json -InputObject $Body -Depth 30 -Compress }
    }
    $token = if ($Anonymous) { $null } else { [Environment]::GetEnvironmentVariable($TokenVar) }
    $r = $script:Client.Send($Method, "/api/v1$Path", $token, $json, $TimeoutSec)
    $parsed = $null
    if ($r.ContentType -eq 'application/json' -and $r.Bytes.Length) {
        $parsed = ConvertFrom-Json -InputObject $r.Text -Depth 60 -NoEnumerate
    }
    $err = if ($r.Status -ge 400 -and $parsed -isnot [array]) { $parsed.error } else { $null }
    [pscustomobject]@{
        Status = $r.Status; Code = $err.code; Message = $err.message; Details = $err.details
        Body = $parsed; Text = $r.Text; Bytes = $r.Bytes; RequestId = $r.RequestId
    }
}

# OWNER ONLY: redeem a pairing code. Prints nothing secret.
function Set-LlmdilTokenFromPairing {
    $code = Read-Host -MaskInput 'Pairing code'
    $r = Invoke-Llmdil POST '/enroll' -Anonymous -Body @{ code = $code; device_name = 'integration-test' }
    if ($r.Status -ne 201) { throw "enroll: $($r.Status) $($r.Code)" }
    $env:LLMDIL_TOKEN = $r.Body.access_token
    $env:LLMDIL_REFRESH_TOKEN = $r.Body.refresh_token
}

# After a 401: rotate once. Never retry it with an old refresh token.
function Update-LlmdilToken {
    $r = Invoke-Llmdil POST '/auth/refresh' -Anonymous -Body @{ refresh_token = $env:LLMDIL_REFRESH_TOKEN }
    if ($r.Status -ne 200) { throw "refresh: $($r.Status) $($r.Code) - ask the owner for a new token" }
    $env:LLMDIL_TOKEN = $r.Body.access_token
    $env:LLMDIL_REFRESH_TOKEN = $r.Body.refresh_token
}

# Record one assertion. Never throws, so a run continues after a failure.
function Assert {
    param([Parameter(Mandatory)][string]$Case, [AllowNull()]$Ok, [string]$What)
    $res = $Ok ? 'PASS' : 'FAIL'
    $script:Results.Add([pscustomobject]@{ Case = $Case; Result = $res; What = $What })
    Write-Host ("{0,-6} {1}  {2}" -f $Case, $res, $What)
}

function Assert-ApiError {
    param([string]$Case, $Response, [int]$Status, [string]$Code, [string]$Like = '*')
    $ok = $Response.Status -eq $Status -and "$($Response.Code)" -eq $Code -and "$($Response.Message)" -like $Like
    Assert $Case $ok "expected $Status $Code; got $($Response.Status) $($Response.Code) $($Response.Message)"
}

# Poll until done, failed or aborted. Takes the 202 answer, or an id (also
# an app task id). A failed operation is a result, not an exception.
function Wait-Operation {
    param([Parameter(Mandatory)]$Operation, [int]$TimeoutSec = 900)
    if ($Operation -is [string]) { $id = $Operation }
    elseif ($Operation.Status -eq 202) { $id = $Operation.Body.id }
    else { throw "not a 202 answer: $($Operation.Status) $($Operation.Code) $($Operation.Message)" }
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($true) {
        $r = Invoke-Llmdil GET "/operations/$id"
        if ($r.Status -eq 401) { Update-LlmdilToken; continue }
        if ($r.Status -ne 200) { throw "operation ${id}: $($r.Status) $($r.Code) $($r.Message)" }
        if ($r.Body.status -in 'done', 'failed', 'aborted') {
            # API records carry error; app task records carry detail.error.
            $e = if ($r.Body.error) { $r.Body.error } else { $r.Body.detail.error }
            return [pscustomobject]@{ Id = $id; Kind = $r.Body.kind; Status = $r.Body.status; Error = $e; Detail = $r.Body.detail }
        }
        if ($sw.Elapsed.TotalSeconds -gt $TimeoutSec) { throw "operation $id still $($r.Body.status) after $TimeoutSec s" }
        Start-Sleep -Seconds ($sw.Elapsed.TotalSeconds -lt 10 ? 1 : 5)
    }
}

# Operations carry error text only, no code (D03).
function Assert-OperationDone {
    param([string]$Case, $Response, [int]$TimeoutSec = 900)
    if ($Response.Status -ne 202) { Assert $Case $false "expected 202; got $($Response.Status) $($Response.Code) $($Response.Message)"; return }
    $o = Wait-Operation $Response -TimeoutSec $TimeoutSec
    Assert $Case ($o.Status -eq 'done') "operation $($o.Status) $($o.Error)"
    $o
}
function Assert-OperationFailed {
    param([string]$Case, $Response, [string]$Like, [int]$TimeoutSec = 120)
    if ($Response.Status -ne 202) { Assert $Case $false "expected 202; got $($Response.Status) $($Response.Code) $($Response.Message)"; return }
    $o = Wait-Operation $Response -TimeoutSec $TimeoutSec
    Assert $Case ($o.Status -eq 'failed' -and "$($o.Error)" -like $Like) "operation $($o.Status): $($o.Error)"
}

function Get-Plan { (Invoke-Llmdil GET '/activation-plan').Body }

# Rows of somebody else (rule 2). Two kinds name nothing: gateway_drift (added
# for every unapplied gateway change) and gateway_config (D09). They are yours
# only if the plan was empty when you started, which P-1 checks.
function Get-ForeignPlanRows {
    @((Get-Plan).changes | Where-Object {
        $_.kind -notin 'gateway_drift', 'gateway_config' -and
        "$($_.summary) $($_.targets -join ' ')" -notmatch '(^|[^a-z0-9-])qa-'
    })
}

function Get-Backend([string]$Id) { (Invoke-Llmdil GET "/backends/$Id").Body }

# A start operation is done when the process is spawned, not when the
# model is loaded: poll the backend itself.
function Wait-Backend {
    param([string]$Id, [string[]]$State = @('running', 'failed'), [int]$TimeoutSec = 600)
    $sw = [Diagnostics.Stopwatch]::StartNew()
    do {
        $b = Get-Backend $Id
        if ($b.state -in $State -and -not $b.transition) { return $b }
        Start-Sleep -Seconds 2
    } while ($sw.Elapsed.TotalSeconds -lt $TimeoutSec)
    throw "backend $Id is still $($b.state) after $TimeoutSec s"
}

# A port no configured object and no listener uses.
function Get-FreePort([int]$From = 18080) {
    $cfg  = (Invoke-Llmdil GET '/config').Body
    $used = @($cfg.gateway.port, $cfg.postgres.port, $cfg.admin_api.port, $cfg.winml.port) +
            @($cfg.backends.port) + @((Get-NetTCPConnection -State Listen).LocalPort)
    while ($From -in $used) { $From++ }
    $From
}

# One gateway chat. The key is read from the environment and never shown.
function Invoke-Gateway {
    param([Parameter(Mandatory)][string]$Model, [string]$Prompt = 'Reply with the word ready.',
          [int]$MaxTokens = 64, [switch]$NoKey, [int]$TimeoutSec = 300)
    $port = (Invoke-Llmdil GET '/gateway/status').Body.port
    $h = @{}
    if (-not $NoKey) {
        if (-not $env:LLMDIL_GATEWAY_KEY) { throw 'the owner must set LLMDIL_GATEWAY_KEY first' }
        $h.Authorization = "Bearer $env:LLMDIL_GATEWAY_KEY"
    }
    $json = ConvertTo-Json -Compress -Depth 5 @{
        model = $Model; max_tokens = $MaxTokens; messages = @(@{ role = 'user'; content = $Prompt })
    }
    $r = Invoke-WebRequest "http://127.0.0.1:$port/v1/chat/completions" -Method Post -Headers $h -Body $json `
        -ContentType 'application/json' -SkipHttpErrorCheck -TimeoutSec $TimeoutSec
    $j = try { $r.Content | ConvertFrom-Json -Depth 30 } catch { $null }
    $m = $j.choices ? $j.choices[0].message : $null
    [pscustomobject]@{ Status = [int]$r.StatusCode; Text = "$($m.content)$($m.reasoning_content)"; Raw = $j }
}
```

### 1.4 How answers really work

| Situation | Answer |
|---|---|
| Read or save | `200` with the payload; `POST /backends`, `/models`, `/gateway/groups`: `201` |
| Long work (start, stop, restart, gateway, apply, audit, download, verify, diagnostics, setup steps) | `202` with `{id, kind, status, detail, created_ms, started_ms, finished_ms, error}`; poll `GET /operations/{id}` |
| `POST /updates/components/{id}/apply` | `200` with `task_id` (null when there is nothing to do); `Wait-Operation $taskId` works on it |
| Error | `{"error": {code, kind, message, details}}`; assert on status and `code`, show `message` |
| Failed operation | `status: failed`; `error` is **text**. Its prefix gives the kind: `invalid input:`, `not found:`, `supervisor error:`, `config error:`; a busy refusal has no prefix and says "already in progress" |
| Wrong argument on a `202` route | `202`, then `failed` (D03), not `400` |
| Busy on a `202` route | `202`, then `failed` (D03), not `409` |
| `kind` | The operation name (`start_backend`). The app keeps a second record (`backend_start`) with its own id; `GET /operations` lists both (D10) |
| Wrong method on a known path | `405`, empty body |
| Unknown path | `404 NOT_FOUND`, with or without a token |

| code | Status | When |
|---|---|---|
| `INVALID_INPUT` | 400 | Argument missing, wrong type, out of range, unknown body field |
| `UNAUTHENTICATED` | 401 | No, wrong or expired token |
| `FORBIDDEN` | 403 | Role too low |
| `NOT_FOUND` | 404 | Unknown id |
| `BUSY` | 409 | A synchronous call meets a running operation |
| `CONFIG` | 422 | The whole configuration would become invalid (ports, splits, fallbacks, rules) |
| `JSON` | 500 | Some wrong types inside a patch (D06, D07) |
| `UNSUPPORTED` | 501 | Report-only components |
| `SUPERVISOR`, `GATEWAY`, `MODELS`, … | 500 | The part that failed |

Operation records live in memory: after the app restarts, an old id is
`404`. Messages are cut at 400 characters (D16).

## 2. Rules

The rules of the UI test apply. Adapted to HTTP:

1. **Own objects only.** Send `POST`, `PUT`, `PATCH`, `DELETE` only for
   ids that start with `qa-`. There is no confirmation dialog: a call
   takes effect at once. Check the id in the URL *and* in the body (D04).
2. **Plan first.** The plan must be **empty** before the first change
   (`pending: false`; the owner applies theirs first), and
   `Get-ForeignPlanRows` must be empty before every apply and cancel.
   `POST /activation-plan/cancel` restores the **whole** saved
   configuration.
3. **Whole arrays.** `PATCH /gateway/config` replaces arrays. Never send
   `groups`; use `/gateway/groups/{name}`. For `routing_rules`, read,
   change only your own rule, send all of them back at once.
4. **Not part of this test** (routes exist; never call them):
   `/auth/password`, `/auth/totp*`, `/admin/users*`, `/admin/api-tokens*`,
   `/admin/enrollment-codes`, `/enroll`, `DELETE /models/*`,
   `POST /setup/steps/*/run|retry|skip|mark-manual`,
   `POST /setup/complete`, `PUT /mxc/policy`, `/app-update/*` except
   `GET`, OpenRouter fields. Also never: the master key, MCP credentials,
   log-receiver deploy, rotating the TLS certificate.
5. **Ask first:** `bind_host` and `port` in `PATCH /gateway/config`,
   `PUT /mxc/mode`, `/updates/components/*`, model download,
   `/gateway/database/stop`, `/log-receiver/*`. Never send
   `allow_wddm_overcommit`, `offload_override`, `allow_override` or
   `PUT …/offload-override`: those are the API's confirmations.
6. **Two sources.** A case passes only when the API answer and a second
   source agree: `config.json`, `activation-journal.json`,
   `gateway/config.yaml`, processes, `nvidia-smi`, llama-server `/health`
   and `/props`, the lifecycle logs. Another API route does not count:
   it reads the same memory.
7. Keep at least 8 GB of RAM available; run one large MoE model at a time.
8. **Evidence stays local.** `/config`, `/backends/{id}/launch-command`,
   operation details and logs contain local paths (D19). Publish ids,
   statuses, codes and numbers; nothing else.

Record **PASS**, **FAIL**, **ODD** or **SKIP**, as in the UI test. A case
with no API is **SKIP (Gnn)**.

The snippets below use these variables:

```powershell
$cfg0    = (Invoke-Llmdil GET '/config').Body       # before any change
$DataDir = $cfg0.data_dir
$ConfigPath = $cfg0.config_path
$MOE2    = '<id of another MoE model>'
$SMALL   = '<id of a registered model that is not downloaded>'
$WDDM    = 1        # index of a WDDM GPU, from nvidia-smi
$N       = (Invoke-Llmdil GET '/telemetry/gpus').Body.samples.Count
function Get-SavedConfig { Get-Content $ConfigPath -Raw | ConvertFrom-Json -Depth 60 }
function Get-SavedBackend([string]$Id) { (Get-SavedConfig).backends | Where-Object id -eq $Id }
function Get-Pid([string]$Id) { $b = Get-Backend $Id; if ($b.child_pid) { $b.child_pid } else { $b.pid } }
function Get-Argv([string]$Id) { (Get-CimInstance Win32_Process -Filter "ProcessId=$(Get-Pid $Id)").CommandLine }
function Get-Props([string]$Id) { Invoke-RestMethod "http://127.0.0.1:$((Get-Backend $Id).port)/props" }
function Get-GpuUsed { nvidia-smi --query-gpu=index,memory.used --format=csv,noheader,nounits }
function One-Gpu([int]$g) { ,@(0..($N - 1) | ForEach-Object { $_ -eq $g ? 1.0 : 0.0 }) }
```

## 3. Prepare

| Id | Calls | Assert | Second source |
|---|---|---|---|
| P-1 ★ | `GET /server` (no token), `/status`, `/telemetry/gpus`, `/backends`, `/operations` | 200; `server.api_version` = `1`; `server.app_version` = `status.version`; `status.config_load` noted if present | `nvidia-smi -L`, `Get-Process`, `Get-NetTCPConnection -State Listen`, `Get-CimInstance Win32_OperatingSystem`, `Get-PSDrive` |
| P-2 ★ | No route. Take `data_dir` from `GET /config` and copy it without `models` | `robocopy` exit code 0–7 | `Get-FileHash` of both `config.json` |
| P-3 | `POST /gateway/stop` `{}`, then `POST /gateway/database/stop` `{}` (ask first); copy `postgres` and `python` (files only); `POST /gateway/start` (`admin`) | Both operations `done`; `detail.stopped` true. Wrong order: database stop while the gateway runs ends `failed`, `*stop the gateway first*` | Ports closed; file count and byte totals |
| P-4 ★ | `GET /activation-plan` | `pending`, `changes[]` `{ts, kind, summary, scope, targets, activation}` | `activation-journal.json`: its entries are the same rows. Extra journal rows whose backends are stopped are already effective and not in the plan; a row of kind `gateway_drift` is only in the plan |

Session checks, run once (case id `P-1`):

```powershell
Assert P-1 ((Invoke-Llmdil GET '/server' -Anonymous).Status -eq 200) 'identity is public'
Assert-ApiError P-1 (Invoke-Llmdil GET '/status' -Anonymous) 401 'UNAUTHENTICATED'
Assert-ApiError P-1 (Invoke-Llmdil GET '/no-such-route' -Anonymous) 404 'NOT_FOUND'
Assert P-1 ((Invoke-Llmdil DELETE '/status').Status -eq 405) 'wrong method'
$me = (Invoke-Llmdil GET '/auth/me').Body
Assert P-1 ($me.user.role -eq 'admin' -and $me.auth_type -eq 'session' -and -not $me.must_change_password) "role $($me.user.role)"
# If the owner also set a viewer token: a refused change, nothing is created.
if ($env:LLMDIL_VIEWER_TOKEN) {
    Assert-ApiError P-1 (Invoke-Llmdil POST '/gateway/groups' -TokenVar LLMDIL_VIEWER_TOKEN -Body @{ name = 'qa-denied' }) 403 'FORBIDDEN'
}
Assert P-1 (-not (Get-Plan).pending) 'no pending changes of the owner; otherwise stop here'
```

Second source for what the API did: `GET /admin/audit?limit=20` (`admin`)
and `logs/admin-api-audit.jsonl`. Every authenticated change that reached
its handler is there with its status; refusals with 401 and 403 are not.

## 4. Screens (read-only)

The layout checks are screen-only. The API checks the data behind each
screen.

| Id | Calls | Assert | Second source |
|---|---|---|---|
| S-1 ★ | `GET /backends`, `/gateway/status`, `/telemetry/gpus` | Each backend's `state`, `pid`, `port` and `group`; gateway `state` `running`. Layout, pause, views: N/A (screen-only) | Process list, listening ports |
| S-2 | N/A (screen-only) | | |
| S-3 ★ | `GET /telemetry/gpus`; `GET /telemetry/history/1h`, `/24h`, `/7d`; `POST /telemetry/reset-peaks`; `POST /telemetry/snapshots` | `has_polled` true, `source_error` null; `vram_used_mib` within 200 MiB; ≤ 600 points per GPU; after the reset `temp_peak_c` ≤ the old value; `{path}` returned. Negative: `/telemetry/history/2h` → `400 INVALID_INPUT` | `nvidia-smi`; `Test-Path` on `path` (no route reads it: G20) |
| S-4 ★ | `GET /backends`, `GET /backends/{id}` | Rows have `id`, `group`, `model_file`, `state`; displayed name = `name` of the model with that `gguf_path` in `GET /models`. `GET /backends/nope` → `404 NOT_FOUND`. Sort, filter, menus: N/A (screen-only) | `config.json` |
| S-5 | `GET /models`, `GET /models/{id}` | `on_disk`, `size_bytes`, `verified_at`. `GET /models/nope` → `404`. Forms: N/A (screen-only) | File size on disk |
| S-6 ★ | Block below | | The log file |
| S-7 ★ | `GET /gateway/config`, `/gateway/status`, `/gateway/generated-config` | `effective_strategy` and `status.groups` (enabled groups); YAML is `text/yaml`. The generated routing file is not returned (G17). Running versus saved labels: N/A (screen-only); `status.port` is the **saved** port (D15) | `gateway/config.yaml` |
| S-8 | `GET /gateway/groups`, `/activation-plan`, `/activation-plan/history`, `GET /config` (`.mcp`) | 200. MCP list: G07. Optimize: G11. Bench: G15, G16 | Files in the data directory |
| S-9 | `GET /config` | Equal to `config.json` field by field, apart from `config_path` | `config.json` |
| S-10 | `GET /setup/status`, `/setup/steps`, `/setup/prerequisites`; `POST /setup/audit` | `setup_completed` true; `steps`, `done`, `total`; audit operation `done`. `detail.already_running` true means it did **not** run (D17): repeat | Scope `wizard` in the logs |

```powershell
# S-6
$scopes = (Invoke-Llmdil GET '/logs/scopes').Body.scopes
foreach ($s in $scopes) {
    $id = [uri]::EscapeDataString($s.id)
    $r = Invoke-Llmdil GET "/logs?scope=$id&limit=50&order=desc"
    Assert S-6 ($r.Status -eq 200 -and @($r.Body.lines).Count -le 50) "scope $($s.id)"
}
$d = (Invoke-Llmdil GET '/logs?scope=gateway&order=desc&limit=20').Body
$a = (Invoke-Llmdil GET '/logs?scope=gateway&order=asc&limit=20').Body
Assert S-6 ($d.total_matching -ge $a.lines.Count) 'both orders answer'
$p2 = (Invoke-Llmdil GET '/logs?scope=gateway&order=desc&limit=10&offset=10').Body
Assert S-6 ($p2.truncated -and $p2.lines[0].line -eq $d.lines[10].line) 'offset pages'
$now = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
$w = (Invoke-Llmdil GET "/logs?scope=all&since_ms=$($now - 600000)&until_ms=$now&limit=200").Body
Assert S-6 (@($w.lines | Where-Object { [DateTimeOffset]$_.ts -lt [DateTimeOffset]::FromUnixTimeMilliseconds($now - 600000) }).Count -eq 0) 'window'
Assert-ApiError S-6 (Invoke-Llmdil GET '/logs?scope=all&order=up') 400 'INVALID_INPUT'
Assert-ApiError S-6 (Invoke-Llmdil GET '/logs') 400 'INVALID_INPUT' '*scope*'
# Expected 200; today 400 INVALID_INPUT, a numeric search is sent on as a number (D05):
Assert S-6 ((Invoke-Llmdil GET '/logs?scope=all&search=4000').Status -eq 200) 'search for a number'
# Expected 400; today 200 unfiltered:
Assert S-6 ((Invoke-Llmdil GET '/logs?scope=all&level=nonsense').Status -eq 400) 'unknown level refused'
```

Line times: compare `ts` of a line with its own `[timestamp]` in the file.
Lines without one carry the **file's modification time**, so they fail
"event times, not read times" whenever the file is written later.

## 5. App life cycle

| Id | API |
|---|---|
| L-1 ★, L-2 ★ | N/A (screen-only). After the step: `GET /server` answers with the same `deployment_id`, and exactly one app process exists |
| L-3 | Tray actions: N/A (screen-only). After Quit the Admin API is gone (it lives in the app): `Invoke-Llmdil` throws, while the gateway still answers on its port. After relaunch: `GET /backends` shows `adopted: true` for running backends; gateway `state` `running` with the same `pid` |
| L-4 | Window: N/A (screen-only). `GET /setup/status` → `setup_completed: true` |

## 6. Create

### Groups

```powershell
# G-1 ★
foreach ($g in 'qa-small', 'qa-moe') {
    $r = Invoke-Llmdil POST '/gateway/groups' -Body @{ name = $g }
    Assert G-1 ($r.Status -eq 201 -and $r.Body.name -eq $g) "group $g"
    Assert G-1 ((Get-SavedConfig).gateway.groups.name -contains $g) "$g in config.json"
}

# G-2: refusals the route makes
Assert-ApiError G-2 (Invoke-Llmdil POST '/gateway/groups' -Body @{ name = '' }) 400 'INVALID_INPUT' '*required*'
Assert-ApiError G-2 (Invoke-Llmdil POST '/gateway/groups' -Body @{ name = 'qa-small' }) 400 'INVALID_INPUT' '*already exists*'
Assert-ApiError G-2 (Invoke-Llmdil POST '/gateway/groups' -Body @{ name = 'qa-x'; nope = 1 }) 400 'INVALID_INPUT' '*unknown field*'
Assert-ApiError G-2 (Invoke-Llmdil PATCH '/gateway/groups/qa-none' -Body @{ purpose = 'x' }) 404 'NOT_FOUND'
# G-2: the name rules. Expected 400; today 201, this route does not have the app's rules (G06, D08).
foreach ($bad in 'qa bad!', ('qa-' + 'x' * 60)) {
    $r = Invoke-Llmdil POST '/gateway/groups' -Body @{ name = $bad }
    Assert-ApiError G-2 $r 400 'INVALID_INPUT'
    if ($r.Status -eq 201) { $null = Invoke-Llmdil DELETE "/gateway/groups/$([uri]::EscapeDataString($bad))" }
}

# G-3: an empty group (the confirmation is screen-only)
$null = Invoke-Llmdil POST '/gateway/groups' -Body @{ name = 'qa-empty' }
$r = Invoke-Llmdil DELETE '/gateway/groups/qa-empty'
Assert G-3 ($r.Status -eq 200 -and $r.Body.deleted -eq 'qa-empty') 'deleted'
Assert G-3 ((Get-SavedConfig).gateway.groups.name -notcontains 'qa-empty') 'gone from config.json'
Assert-ApiError G-3 (Invoke-Llmdil DELETE '/gateway/groups/qa-empty') 404 'NOT_FOUND'
```

Do **not** send `openrouter` as a new group name: it would be created
(D08), and the name is not yours. Do not delete a group with members to
see the refusal: this route deletes it with its member list (D08).

### Backends

```powershell
function Add-QaBackend([string]$Case, [hashtable]$Body) {
    if (-not $Body.ContainsKey('port')) { $Body.port = Get-FreePort }
    $b = @{ create_group = $false; enabled = $true }
    foreach ($k in $Body.Keys) { $b[$k] = $Body[$k] }
    $r = Invoke-Llmdil POST '/backends' -Body $b
    Assert $Case ($r.Status -eq 201 -and $r.Body.backend_id -eq $b.id -and $r.Body.persisted) "add $($b.id): $($r.Status) $($r.Message)"
    Assert $Case ((Get-SavedBackend $b.id).port -eq $b.port) "$($b.id) in config.json"
    $r
}
Add-QaBackend B-1 @{ id = 'qa-qwen8b';  model_id = 'qwen3-8b';         group = 'qa-small'; gpu_indices = @(0); n_ctx = 32768 }
Add-QaBackend B-2 @{ id = 'qa-coder7b'; model_id = 'qwen2.5-coder-7b'; group = 'qa-small'; gpu_indices = @(1); cache_k = 'q8_0'; cache_v = 'q4_0'; flash_attn = 'auto' }
Add-QaBackend B-3 @{ id = 'qa-nemo';    model_id = 'nemotron-3.5-lightning-30b'; group = 'qa-moe'; gpu_indices = @(0) }
Add-QaBackend B-4 @{ id = 'qa-moe2';    model_id = $MOE2;              group = 'qa-moe'; gpu_indices = @(1) }
Add-QaBackend B-5 @{ id = 'qa-dense27'; model_id = 'qwen3.8-27b-mtp';  group = 'qa-split'; create_group = $true; gpu_indices = @(0, 1) }
Add-QaBackend B-6 @{ id = 'qa-big';     model_id = 'qwen3.8-27b-mtp';  group = 'qa-split'; gpu_indices = @(0); n_ctx = 262144; enabled = $false }
```

| Id | Assert after the call | Second source |
|---|---|---|
| B-1 ★ | `GET /backends/qa-qwen8b`: `n_ctx` 32768, `n_batch` 512, `placement` `GPU 0`, `group` `qa-small` | `config.json`: `tensor_split` 1.0 on GPU 0, `raw_flags` null or the model default |
| B-2 ★ | `cache` = `{k: q8_0, v: q4_0, flash_attn: auto}`; `placement` `GPU 1` | `raw_flags` contains `--cache-type-k q8_0 --cache-type-v q4_0 --flash-attn auto` |
| B-3 ★ | Add with a middle position: `offload_layers` = half of `offload.moe_layers`. On a restricted GPU (compute capability 6.x) expected: `400 INVALID_INPUT`, `*outside the verified range*`, and no backend. Then add without `offload_layers` as above and read `offload.guardrail` from `GET /backends/qa-nemo`. The form's preview: G02 | `config.json` has no `qa-nemo` after the 400 |
| B-4 | `offload.layers` = `offload.green_limit_layers` | `raw_flags` |
| B-5 `2GPU` | `placement` `GPU 0+1`; group `qa-split` exists | `tensor_split` 0.5 / 0.5 |
| B-6 | `fit.state` `does_not_fit` (TCC) or `wddm_paged` (WDDM); `enabled` false | `fit.per_gpu[0].required_mib` > `total_mib` from `nvidia-smi` |
| B-7 | `PUT /backends/qa-qwen8b/flags` `{"n_ctx":10000,"n_batch":700}` and `PUT …/parallel` `{"slots":3}` → 200, `persisted`. Restart, then put back 32768 / 512 / `{"slots":null}` | Command line has `-c 10000`, `-b 700`, `--parallel 3`; `/props` `total_slots` = 3 |

**Refusals** (id `B-neg`). Each must leave `GET /backends/qa-bad` at 404:

```powershell
$ok = @{ id = 'qa-bad'; model_id = 'qwen3-8b'; port = (Get-FreePort); group = 'qa-small'; create_group = $false; enabled = $false; gpu_indices = @(0) }
function Add-Bad($Change, $Status, $Code, $Like = '*') {
    $b = $ok.Clone(); foreach ($k in $Change.Keys) { $b[$k] = $Change[$k] }
    $r = Invoke-Llmdil POST '/backends' -Body $b
    Assert-ApiError B-neg $r $Status $Code $Like
    if ($r.Status -eq 201) { $null = Invoke-Llmdil DELETE "/backends/$($b.id)" }
}
Add-Bad @{ id = 'qa-qwen8b' }                 400 'INVALID_INPUT' '*already exists*'   # never delete on this one:
                                                                                       # it cannot answer 201
Add-Bad @{ id = 'QA!' }                       400 'INVALID_INPUT' '*lowercase*'
Add-Bad @{ gpu_indices = @() }                400 'INVALID_INPUT' '*at least one GPU*'
Add-Bad @{ gpu_indices = @(99) }              400 'INVALID_INPUT' '*not in the telemetry*'
Add-Bad @{ port = (Get-Backend 'qa-coder7b').port } 422 'CONFIG' '*claimed by both*'
Add-Bad @{ port = $cfg0.gateway.port }        422 'CONFIG' '*claimed by both*'
Add-Bad @{ port = 0 }                         400 'INVALID_INPUT'
Add-Bad @{ port = 70000 }                     400 'INVALID_INPUT' '*port*'
Add-Bad @{ port = $null }                     400 'INVALID_INPUT' '*port*'
Add-Bad @{ n_ctx = 1023 }                     400 'INVALID_INPUT' '*1024*'
Add-Bad @{ n_ctx = 1048577 }                  400 'INVALID_INPUT' '*1048576*'
Add-Bad @{ n_batch = 0 }                      400 'INVALID_INPUT' '*batch*'
Add-Bad @{ n_ctx = 2048; n_batch = 4096 }     400 'INVALID_INPUT' '*batch*'
Add-Bad @{ parallel = 0 }                     400 'INVALID_INPUT' '*parallel*'
Add-Bad @{ parallel = 1025 }                  400 'INVALID_INPUT' '*parallel*'
Add-Bad @{ cache_k = 'f16'; cache_v = 'q8_0'; flash_attn = 'off' } 400 'INVALID_INPUT' '*flash*'
Add-Bad @{ cache_k = 'f16' }                  400 'INVALID_INPUT' '*set together*'
Add-Bad @{ create_group = $true }             400 'INVALID_INPUT' '*already exists*'
Add-Bad @{ group = 'QA Small!'; create_group = $true } 400 'INVALID_INPUT' '*group name*'
Add-Bad @{ group = 'qa-none' }                404 'NOT_FOUND'
Add-Bad @{ model_id = 'no-such-model' }       404 'NOT_FOUND'
Add-Bad @{ nope = 1 }                         400 'INVALID_INPUT' '*unknown field*'
Assert-ApiError B-neg (Invoke-Llmdil POST '/backends' -Body '{bad') 400 'INVALID_INPUT' '*malformed JSON*'
Assert-ApiError B-neg (Invoke-Llmdil GET '/backends/qa-bad') 404 'NOT_FOUND'
```

The port of the Admin API itself is **not** checked by the configuration
rules: don't use it. The live memory lines of the form: G02, G03. Focus and
captions: N/A (screen-only).

## 7. Activation Plan

```powershell
# A-1 ★ (the gateway must be running; qa-coder7b stopped). First make the plan empty with A-2.
$b = Invoke-Llmdil PATCH '/backends/qa-coder7b/config' -Body @{ patch = @{ enabled = $false }; apply_mode = 'OnNextRestart' }
Assert A-1 ($b.Status -eq 200 -and $b.Body.persisted -and $b.Body.diff.changes[0].field -eq 'enabled') 'saved, diff names the field'
$p = Get-Plan
Assert A-1 (@($p.changes | Where-Object summary -match 'qa-coder7b').Count -eq 1) 'one row names the backend'
Assert A-1 (@($p.changes).Count -eq 1) 'nothing else'            # today 2: a gateway_drift row comes with it (D21)
Assert A-1 ("$($p.changes.summary)" -match 'enabled|disabled') 'names what changed'   # today "settings changed"
# (then switch it back on the same way)

# A-2 ★
Assert A-2 (@(Get-ForeignPlanRows).Count -eq 0) 'only own rows'
$o = Assert-OperationDone A-2 (Invoke-Llmdil POST '/activation-plan/apply' -Body @{ mode = 'now' })
Assert A-2 ($o.Detail.ok -and $o.Detail.status -in 'completed', 'nothing_pending') $o.Detail.status
Assert A-2 ((Invoke-Llmdil GET '/gateway/status').Body.state -eq 'running') 'gateway is back'
$h = (Invoke-Llmdil GET "/activation-plan/history?execution_id=$($o.Detail.execution_id)").Body.execution
Assert A-2 ($h.source -eq 'api' -and $h.items.Count -gt 0) 'history row'
Assert A-2 ((Get-Content "$DataDir\gateway\config.yaml" -Raw) -match 'qa-small') 'config.yaml has the group'
Assert A-2 (-not (Get-Plan).pending) 'plan empty'

# A-3 ★ (needs one pending own change; no traffic)
$t = [Diagnostics.Stopwatch]::StartNew()
$w = Invoke-Llmdil POST '/activation-plan/quiesce-wait' -Body @{ timeout_secs = 30 }
Assert A-3 ($w.Body.drained -and $t.Elapsed.TotalSeconds -lt 10) 'drains in seconds'
$o = Assert-OperationDone A-3 (Invoke-Llmdil POST '/activation-plan/apply' -Body @{ mode = 'drain' })
Assert A-3 ($o.Detail.gateway_restarted) 'gateway restarted'
Assert-ApiError A-3 (Invoke-Llmdil POST '/activation-plan/apply' -Body @{ mode = 'later' }) 400 'INVALID_INPUT'

# A-5 ★ (admin; the plan is empty; qa-qwen8b is running)
Assert A-5 (-not (Get-Plan).pending) 'start from an empty plan'
$before = Get-SavedBackend 'qa-qwen8b' | ConvertTo-Json -Depth 20
$null = Invoke-Llmdil PUT '/backends/qa-qwen8b/flags'    -Body @{ n_ctx = 16384; n_batch = 512 }
$null = Invoke-Llmdil PUT '/backends/qa-qwen8b/cache'    -Body @{ cache_k = 'q8_0'; cache_v = 'q8_0'; flash_attn = 'on' }
$null = Invoke-Llmdil PUT '/backends/qa-qwen8b/parallel' -Body @{ slots = 2 }
$p = Get-Plan
Assert A-5 ($p.backend_restart_ids -contains 'qa-qwen8b' -and @(Get-ForeignPlanRows).Count -eq 0) 'only own rows'
$c = Invoke-Llmdil POST '/activation-plan/cancel'
Assert A-5 ($c.Status -eq 200 -and $c.Body.cancelled -and $c.Body.yaml_regenerated) "cancelled $($c.Body.reason)"
$p = Get-Plan
Assert A-5 (-not $p.pending -and -not $p.drift) 'nothing pending, no drift'
Assert A-5 ((Get-SavedBackend 'qa-qwen8b' | ConvertTo-Json -Depth 20) -eq $before) 'exactly those edits undone'
$c = Invoke-Llmdil POST '/activation-plan/cancel'
Assert A-5 (-not $c.Body.cancelled -and $c.Body.reason -eq 'no pending changes') 'second cancel does nothing'
```

| Id | API |
|---|---|
| A-4 | Start a long streaming chat (W-5 block, do not stop it), then `POST /activation-plan/apply` `{"mode":"drain"}`. While it waits: `GET /gateway/status` → `activation_applying.step` = `gateway`. At the end `detail.quiesce` = `{drained, in_flight, timed_out}`; with `timed_out: true` the apply went on and the detail says so. Second source: scope `gateway`, lines "draining in-flight requests" and "drain finished". A second apply meanwhile: `202`, then `failed`, `*activation apply*running*` |
| A-6 | `GET /activation-plan` twice gives the same rows; `GET /activation-plan/history` and `?execution_id=`. An unknown id returns `200` with `execution: null`. *Later* and *Review*: N/A (screen-only). Rows "Gateway configuration changed" name no target (D09) |
| A-7 | Make a **running** backend not fit: `PUT /backends/qa-qwen8b/flags` with a context whose `fit.state` is not `fits`, plus one group change, then apply. Expected: operation `failed`, item of `qa-qwen8b` `failed` with the memory reason, **other items applied**. Today: the apply **stops** at the first backend that fails; later items and the gateway are `skipped` ("Not attempted …"), `status` `failed`. Put the flags back and apply again |

## 8. Run and fail

```powershell
# R-1 ★
$gpu0 = Get-GpuUsed
$o = Assert-OperationDone R-1 (Invoke-Llmdil POST '/backends/qa-qwen8b/start' -Body @{})
$b = Wait-Backend 'qa-qwen8b'
Assert R-1 ($b.state -eq 'running') "state $($b.state) $($b.failure_reason)"
Assert R-1 ((Invoke-RestMethod "http://127.0.0.1:$($b.port)/health").status -eq 'ok') 'health'
$t = Invoke-Llmdil POST '/backends/qa-qwen8b/test' -Body @{ prompt = 'Reply with the word ready.' }
Assert R-1 ($t.Status -eq 200 -and $t.Body.status -eq 200 -and "$($t.Body.content)$($t.Body.reasoning_content)") 'answers'
$l = Invoke-Llmdil GET '/backends/qa-qwen8b/launch-command'          # admin
Assert R-1 ($l.Body.argv -contains '--port') 'launch command'
Get-Argv 'qa-qwen8b'                                                 # compare with argv (local only)
$need = $b.fit.per_gpu | ForEach-Object { "GPU $($_.gpu): $($_.required_mib) MiB" }
$need; Get-GpuUsed; $gpu0                                            # difference within 512 MiB of required_mib
# Test tool calls: NOT POSSIBLE VIA API (G04)

# R-2: two starts at once (backend stopped)
$two = @(1..2 | ForEach-Object { Invoke-Llmdil POST '/backends/qa-coder7b/start' -Body @{} })
$x = @($two | ForEach-Object { Wait-Operation $_ })
Assert R-2 (@($x | Where-Object Status -eq 'done').Count -eq 1) "one starts; the other: $(($x | Where-Object Status -eq 'failed').Error)"
$port = (Get-Backend 'qa-coder7b').port
Assert R-2 (@(Get-NetTCPConnection -State Listen -LocalPort $port -ErrorAction Ignore).Count -le 1) 'one process'

# R-3 (the body of stop and restart must contain graceful; without it: 202, then failed)
Assert-OperationFailed R-3 (Invoke-Llmdil POST '/backends/qa-coder7b/stop' -Body @{}) '*graceful*'
$null = Invoke-Llmdil POST '/backends/qa-coder7b/start' -Body @{}
$s = Wait-Operation (Invoke-Llmdil POST '/backends/qa-coder7b/stop' -Body @{ graceful = $true })
Assert R-3 ($s.Status -eq 'done' -or $s.Error -match 'already in progress') "stop while starting: $($s.Status) $($s.Error)"
$b = Wait-Backend 'qa-coder7b' -State 'running', 'stopped', 'failed'
Assert R-3 ([bool](Get-Pid 'qa-coder7b') -eq ($b.state -in 'running', 'starting')) 'state matches the process'
$o = Assert-OperationDone R-3 (Invoke-Llmdil POST '/backends/qa-coder7b/restart' -Body @{ graceful = $true; wait_ready_secs = 300 })
Assert R-3 ($o.Detail.ready) 'restart waited until ready'
$null = Assert-OperationDone R-3 (Invoke-Llmdil POST '/backends/qa-coder7b/stop' -Body @{ graceful = $true })
Assert R-3 ((Wait-Backend 'qa-coder7b' -State 'stopped', 'failed').state -eq 'stopped') 'stopped'

# R-4 ★ (use the largest qa- model that runs here)
$null = Assert-OperationDone R-4 (Invoke-Llmdil POST '/backends/qa-dense27/stop' -Body @{ graceful = $true })
$b = Wait-Backend 'qa-dense27' -State 'stopped', 'failed'
Assert R-4 ($b.state -eq 'stopped' -and -not $b.failure_reason) "state $($b.state) $($b.failure_reason)"

# R-8 ★ another program holds the port (this PowerShell process)
$port = (Get-Backend 'qa-qwen8b').port     # backend stopped
$hold = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, $port); $hold.Start()
$t = [Diagnostics.Stopwatch]::StartNew()
Assert-OperationFailed R-8 (Invoke-Llmdil POST '/backends/qa-qwen8b/start' -Body @{}) "*port $port is already in use*"
Assert R-8 ($t.Elapsed.TotalSeconds -lt 20) "refused in $([int]$t.Elapsed.TotalSeconds) s"
Assert R-8 ((Get-Backend 'qa-qwen8b').failure_reason -match 'pwsh') 'names the program'      # today: only the port
$hold.Stop()

# R-9 ★ kill one own llama-server
$id = Get-Pid 'qa-qwen8b'                   # backend running
Assert R-9 ((Get-Process -Id $id).ProcessName -eq 'llama-server') 'own process'
Stop-Process -Id $id -Force
Start-Sleep 6                               # the monitor looks every 3 s
$b = Get-Backend 'qa-qwen8b'
Assert R-9 ($b.state -in 'failed', 'restarting', 'starting' -and $b.failure_reason) "$($b.state): $($b.failure_reason)"
Start-Sleep 60
Assert R-9 ((Get-Backend 'qa-qwen8b').state -eq 'running') 'restarted by its policy'          # today: stays failed
```

| Id | Calls | Assert | Second source |
|---|---|---|---|
| R-5 ★ | `POST /backends/{id}/start` for a `qa-*` backend that needs more than GPU 0's **free** memory | 202, then `failed`, `*cannot start*GPU 0 needs*only*MiB is free*`, naming the running backends. `fit.state` of the row compares with **total** memory, so it may say `fits` here: record ODD | `nvidia-smi` free memory before and after is unchanged |
| R-6 | `PATCH /backends/qa-big/config` `{"patch":{"enabled":true},"apply_mode":"OnNextRestart"}`, then start on a TCC GPU 0 | 202, then `failed`, `*weights * MiB + KV cache*exceeds the GPU's total memory*`. Disable again. Disabled: `failed`, `*is disabled*` | No new process |
| R-7 | `POST /placement/apply` `{"model_id":"qwen3.8-27b-mtp","split": One-Gpu $WDDM,"backend_id":"qa-big"}`, then one start **without** `allow_wddm_overcommit` | `GET /backends/qa-big`: `fit.state` `wddm_paged`, `fit.paged_mib` > 0; 202, then `failed`, `*cannot start without confirmation*will page to system RAM*`. Only the text tells this refusal from R-6 (D03) | No new process |
| R-10 ★ | On a CC 6.x GPU 0: `PUT /backends/qa-nemo/offload` `{"layers": <half>}` | `400 INVALID_INPUT` naming the safe positions; `offload.guardrail.status`, `nearest_verified_layers` from `GET /backends/qa-nemo`. Then `{"layers": <moe_layers>}` → 200; start; `POST …/test` answers | `raw_flags` in `config.json` unchanged by the 400 |
| R-11 | Start two backends on one GPU until nearly full | Second runs, or 202 then `failed` as R-5 | `nvidia-smi` |
| R-12 | `POST /backends` `qa-small-missing` with `model_id` `$SMALL` in `qa-small`, then start | 201; 202, then `failed`, `*model file is missing*` (path shown as `<data-dir>`). Delete it (`admin`) | File absent |
| R-13 | `PUT /backends/qa-coder7b/flags-mode` `{"advanced":true}`; `PUT …/raw-flags` `{"raw_flags":"--qa-no-such-flag 1"}`; start | 200 with `warnings[]` not empty; the start operation is **done** (spawned); then `state` `failed`. `GET /logs?scope=qa-coder7b&search=invalid` shows llama-server's message. Put back: `{"raw_flags":null}`, `{"advanced":false}` | llama-server exits; log scope |

The log of a failed start must be of **this** start: compare `ts` with the
start time; lines of an earlier run have `archived: true`.

## 9. Reconfigure

Pattern for each row: save, apply (or restart with `wait_ready_secs`),
check, put back.

| Change | Call | Check |
|---|---|---|
| Context, batch | `PUT /backends/{id}/flags` `{"n_ctx","n_batch"}` | `-c`, `-b` |
| Parallel | `PUT …/parallel` `{"slots": n or null}` | `--parallel`; `/props` `total_slots` |
| K/V cache, flash | `PUT …/cache` `{"cache_k","cache_v","flash_attn"}` together; `{}` clears. Values `f16`, `q8_0`, `q4_0`; `auto`, `on`, `off` | Command line |
| MTP draft | `PUT …/mtp` `{"n_max": 1..4 or null}`; 5 → `400`; on a model without MTP → `400` | `--spec-draft-n-max` |
| Expert cache | `PUT …/moe-cache` `{"mib": n or null}` or `{"auto": true}`; both → `400`; with offload 0 → `400` | `--moe-cache-mib`; `resolved_mib` |
| MoE offload | `PUT …/offload` `{"layers": 0 / middle / all}`; more than `moe_layers` → `400` | `raw_flags`; RAM and VRAM |
| Flags as text | `PUT …/flags-mode`, `PUT …/raw-flags` | Command line |
| Several fields | `PATCH …/config` `{"patch":{…},"apply_mode":"OnNextRestart"}`; preview: `POST …/config/preview-diff` `{"patch":{…}}`. Never `RestartNow` (D01) | `diff.changes` |
| Placement | `POST /placement/propose` and `/apply` `{"model_id","split":[…],"backend_id"}`; GPU 0 → 1 → both. Wrong length → `400` (apply) | `CUDA_VISIBLE_DEVICES`, `--tensor-split`, `nvidia-smi` |

```powershell
# One full example; the others follow the same pattern
$r = Invoke-Llmdil PUT '/backends/qa-qwen8b/flags' -Body @{ n_ctx = 16384; n_batch = 1024 }
Assert C-5 ($r.Status -eq 200 -and $r.Body.applies -eq 'next start') 'saved'
$null = Assert-OperationDone C-5 (Invoke-Llmdil POST '/backends/qa-qwen8b/restart' -Body @{ graceful = $true; wait_ready_secs = 300 })
Assert C-5 ((Get-Argv 'qa-qwen8b') -match '-c 16384 -b 1024') 'command line'
Assert C-5 ((Invoke-Llmdil GET '/backends/qa-qwen8b/launch-command').Body.argv -contains '16384') 'launch command follows'

# C-4: text mode refuses the structured calls
$null = Invoke-Llmdil PUT '/backends/qa-coder7b/flags-mode' -Body @{ advanced = $true }
Assert-ApiError C-4 (Invoke-Llmdil PUT '/backends/qa-coder7b/flags' -Body @{ n_ctx = 8192; n_batch = 512 }) 400 'INVALID_INPUT' '*advanced flags mode*'
$r = Invoke-Llmdil PUT '/backends/qa-coder7b/raw-flags' -Body @{ raw_flags = '--parallel 1 --flash-attn auto' }
Assert C-4 ($r.Status -eq 200 -and @($r.Body.warnings).Count -eq 0) 'known flags do not warn'
$null = Invoke-Llmdil PUT '/backends/qa-coder7b/flags-mode' -Body @{ advanced = $false }
Assert-ApiError C-4 (Invoke-Llmdil PUT '/backends/qa-coder7b/raw-flags' -Body @{ raw_flags = '--parallel 1' }) 400 'INVALID_INPUT' '*not in advanced*'

# C-neg: the patch route (own backend, stopped). Expected / today in the comments.
Assert-ApiError C-neg (Invoke-Llmdil PATCH '/backends/qa-none/config' -Body @{ patch = @{ enabled = $true }; apply_mode = 'OnNextRestart' }) 404 'NOT_FOUND'   # today 422 CONFIG (D06)
Assert-ApiError C-neg (Invoke-Llmdil PATCH '/backends/qa-coder7b/config' -Body @{ patch = @{ n_ctxx = 1 }; apply_mode = 'OnNextRestart' }) 400 'INVALID_INPUT' # today 200 (D06)
Assert-ApiError C-neg (Invoke-Llmdil PATCH '/backends/qa-coder7b/config' -Body @{ patch = @{ n_ctx = 'x' }; apply_mode = 'OnNextRestart' }) 400 'INVALID_INPUT' # today 500 JSON (D06)
Assert-ApiError C-neg (Invoke-Llmdil PATCH '/backends/qa-coder7b/config' -Body @{ patch = @{ }; apply_mode = 'Soon' }) 422 'CONFIG'
$n = (Get-SavedBackend 'qa-coder7b').flags.n_ctx
$r = Invoke-Llmdil PATCH '/backends/qa-coder7b/config' -Body @{ patch = @{ n_ctx = 10 }; apply_mode = 'OnNextRestart' }
Assert-ApiError C-neg $r 400 'INVALID_INPUT'                                                                                                                  # today 200 (D06)
if ($r.Status -eq 200) { $null = Invoke-Llmdil PUT '/backends/qa-coder7b/flags' -Body @{ n_ctx = $n; n_batch = 512 } }
```

| Id | API |
|---|---|
| C-1 ★ | Screen rules: N/A (screen-only). Every API write saves at once; check `config.json` after each |
| C-2 `2GPU` | `qa-dense27` running: `placement` `GPU 0+1`; `fit.per_gpu` has two rows; `GET /telemetry/gpus` `processes[]` with its `backend_id` on both GPUs. `vram_mib` is null on WDDM: use the `nvidia-smi` difference |
| C-3 ★ | `POST /placement/propose` → `fits` false, `per_gpu_fits`, `additional_mib`, `free_mib`, `weights_only` false. `POST /placement/apply` still saves (200); the start is then refused as R-5/R-6. Second source: no process with a lower `-ngl` than shown |
| C-4 | Block above. Discard question: N/A (screen-only) |
| C-5 | Block above, plus `GET /backends/{id}`: `gpu_ram_mib`, `kv_ram_mib`, `placement` change with the save |

## 10. Gateway and chat

The gateway's OpenAI endpoint is `http://127.0.0.1:<port>/v1` with the
owner's key in `LLMDIL_GATEWAY_KEY`. The model name is the **group** name.

```powershell
# W-1 ★ both members running, plan applied
$h = @{ Authorization = "Bearer $env:LLMDIL_GATEWAY_KEY" }
$gp = (Invoke-Llmdil GET '/gateway/status').Body.port
$models = (Invoke-RestMethod "http://127.0.0.1:$gp/v1/models" -Headers $h).data.id
Assert W-1 ($models -contains 'qa-small') 'group listed'
foreach ($q in 'Say hi.', 'Write a Python function that reverses a string.', ('Summarise: ' + ('word ' * 3000)), 'And now in one line.') {
    $r = Invoke-Gateway 'qa-small' $q -MaxTokens 200
    Assert W-1 ($r.Status -eq 200 -and $r.Text) "chat $($r.Status)"
}
1..12 | ForEach-Object { $null = Invoke-Gateway 'qa-small' "Count to $_." }
Start-Sleep 40
'qa-qwen8b', 'qa-coder7b' | ForEach-Object { Assert W-1 ((Get-Backend $_).requests_1h -gt 0) "$_ got requests" }

# W-2 ★ one member stopped
$null = Wait-Operation (Invoke-Llmdil POST '/backends/qa-coder7b/stop' -Body @{ graceful = $true })
$codes = 1..10 | ForEach-Object { (Invoke-Gateway 'qa-small' "Say $_.").Status }
Assert W-2 (@($codes | Where-Object { $_ -ne 200 }).Count -eq 0) "statuses $($codes -join ' ')"

# W-3 both stopped, no fallback
$null = Wait-Operation (Invoke-Llmdil POST '/backends/qa-qwen8b/stop' -Body @{ graceful = $true })
$r = Invoke-Gateway 'qa-small'
Assert W-3 ($r.Status -ge 400 -and $r.Raw.error.message) "clear error: $($r.Status)"

# W-4 fallback (qa-nemo running)
$r = Invoke-Llmdil PATCH '/gateway/groups/qa-small' -Body @{ fallbacks = @('qa-nemo') }
$null = Assert-OperationDone W-4 (Invoke-Llmdil POST '/activation-plan/apply' -Body @{ mode = 'now' })
$r = Invoke-Gateway 'qa-small'
Assert W-4 ($r.Status -eq 200) "fallback answers as $($r.Raw.model)"
$null = Invoke-Llmdil PATCH '/gateway/groups/qa-small' -Body @{ fallbacks = @() }

# W-5 stop a streaming reply (qa-qwen8b the only running member)
$c = [Net.Http.HttpClient]::new()
$q = [Net.Http.HttpRequestMessage]::new('POST', "http://127.0.0.1:$gp/v1/chat/completions")
$q.Headers.Authorization = [Net.Http.Headers.AuthenticationHeaderValue]::new('Bearer', $env:LLMDIL_GATEWAY_KEY)
$q.Content = [Net.Http.StringContent]::new('{"model":"qa-small","stream":true,"max_tokens":4000,"messages":[{"role":"user","content":"Write a very long story."}]}', [Text.Encoding]::UTF8, 'application/json')
$resp = $c.Send($q, [Net.Http.HttpCompletionOption]::ResponseHeadersRead)
Start-Sleep 5; $resp.Dispose(); $c.Dispose()
Start-Sleep 5
$slots = Invoke-RestMethod "http://127.0.0.1:$((Get-Backend 'qa-qwen8b').port)/slots"
Assert W-5 (@($slots | Where-Object is_processing).Count -eq 0) 'slot idle'

# W-6 ★ after traffic
$b = Get-Backend 'qa-qwen8b'
Assert W-6 ($b.requests_1h -gt 0 -and $null -ne $b.errors_1h) "requests $($b.requests_1h) errors $($b.errors_1h)"
$hist = (Invoke-Llmdil GET '/telemetry/history/1h').Body.gpus | ForEach-Object { $_.points.throughput_toks }
Assert W-6 (@($hist | Where-Object { $_ -gt 0 }).Count -gt 0) 'non-zero throughput in the GPU history'
$null = Assert-OperationDone W-6 (Invoke-Llmdil POST '/gateway/restart' -Body @{})
Start-Sleep 40
Assert W-6 ((Get-Backend 'qa-qwen8b').requests_1h -ge $b.requests_1h) 'counts survive a gateway restart'

# W-7 rules: read all, change only the own one, write all
function Set-QaRule([object[]]$Own) {
    $keep = @((Invoke-Llmdil GET '/gateway/config').Body.routing_rules | Where-Object id -notmatch '^qa-')
    Invoke-Llmdil PATCH '/gateway/config' -Body @{ routing_rules = @($keep) + @($Own) }
}
function New-QaRule($Id, $Tokens, $Enabled = $true) {
    @{ id = $Id; description = 'integration test'; enabled = $Enabled; source = 'user'
       kind = @{ type = 'context-overflow'; target_group = 'qa-moe'; optimal_tokens = $Tokens } }
}
$ownEnabled = @((Invoke-Llmdil GET '/gateway/config').Body.routing_rules | Where-Object { $_.enabled -and $_.kind -and $_.id -notmatch '^qa-' }).Count
# With an enabled rule of the owner, add yours disabled and skip the two-rule check.
$on = $ownEnabled -eq 0
Assert W-7 ((Set-QaRule (New-QaRule 'qa-rule-1' 2000 $on)).Status -eq 200) 'add'
Assert-ApiError W-7 (Set-QaRule (New-QaRule 'qa-rule-1' -5 $on)) 400 'INVALID_INPUT'                     # today 500 JSON (D07)
Assert-ApiError W-7 (Set-QaRule (New-QaRule 'qa-rule-1' 0 $on)) 422 'CONFIG' '*optimal_tokens*'
if ($on) { Assert-ApiError W-7 (Set-QaRule (New-QaRule 'qa-rule-1' 2000), (New-QaRule 'qa-rule-2' 2000)) 422 'CONFIG' '*at most one*' }
Assert W-7 ((Set-QaRule (New-QaRule 'qa-rule-1' 3000 $on)).Status -eq 200) 'edit'
Assert W-7 ((Set-QaRule (New-QaRule 'qa-rule-1' 3000 $false)).Status -eq 200) 'disable'
Assert W-7 ((Set-QaRule @()).Status -eq 200) 'delete'
Assert W-7 (@((Get-SavedConfig).gateway.routing_rules | Where-Object id -match '^qa-').Count -eq 0) 'gone from config.json'

# W-8 fallbacks
Assert-ApiError W-8 (Invoke-Llmdil PATCH '/gateway/groups/qa-small' -Body @{ fallbacks = @('qa-none') })  422 'CONFIG' '*unknown fallback*'
Assert-ApiError W-8 (Invoke-Llmdil PATCH '/gateway/groups/qa-small' -Body @{ fallbacks = @('qa-small') }) 422 'CONFIG' '*unknown fallback*'
Assert W-8 ((Invoke-Llmdil PATCH '/gateway/groups/qa-small' -Body @{ fallbacks = @('qa-nemo') }).Status -eq 200) 'valid one saved'
Assert W-8 ((Invoke-Llmdil PATCH '/gateway/groups/qa-small' -Body @{ fallbacks = 'x' }).Status -eq 400) 'wrong type refused'
$null = Invoke-Llmdil PATCH '/gateway/groups/qa-small' -Body @{ fallbacks = @() }

# W-9 move and back
$m = Invoke-Llmdil PUT '/backends/qa-coder7b/group' -Body @{ group = 'qa-moe' }
Assert W-9 ($m.Body.changed -and $m.Body.removed_from -contains 'qa-small') 'moved'
$m = Invoke-Llmdil PUT '/backends/qa-coder7b/group' -Body @{ group = 'qa-moe' }
Assert W-9 ($m.Status -eq 200 -and -not $m.Body.changed) 'same group: no change'
Assert-ApiError W-9 (Invoke-Llmdil PUT '/backends/qa-coder7b/group' -Body @{ group = 'qa-none' }) 404 'NOT_FOUND'
Assert-ApiError W-9 (Invoke-Llmdil PUT '/backends/qa-coder7b/group' -Body @{ group = '' }) 400 'INVALID_INPUT'
$null = Invoke-Llmdil PUT '/backends/qa-coder7b/group' -Body @{ group = 'qa-small' }

# W-11
Assert W-11 ((Invoke-Gateway 'qa-small' -NoKey).Status -eq 401) 'no key'
Assert W-11 ((Invoke-Gateway 'qa-no-such-model').Status -eq 404) 'wrong model'
```

| Id | Notes |
|---|---|
| W-1 | Second source: requests in scope `gateway` and both llama-server logs. The counts come from a poll every 15 s |
| W-4 | Fallback names are **backend** ids or `openrouter`, not groups |
| W-8 | A fallback that is itself a member of the group is accepted (200): record ODD |
| W-9 | Drag, menu and `Escape`: N/A (screen-only). Count the rows after moving and moving back; expected one net change or none |
| W-10 | `PATCH /gateway/groups/qa-moe` `{"strategy":"least-busy"}` (`simple-shuffle`, `least-busy`, `latency-based`; anything else `400`), then `GET /gateway/config` → `effective_strategy`. "Why" is N/A (screen-only). The strategy is **global**: if `effective_strategy` changes, put it back before any apply |
| W-11 | The status for a wrong model is LiteLLM's, not the app's; the code here does not decide it. Record what comes back |

## 11. Optimize and Bench

Almost all of it has no API. Deliberate for v1 (rest-api.md §6.16; registry
comments at lines 374 and 402).

| Id | API |
|---|---|
| O-1 | NOT POSSIBLE VIA API (G11). Only a group's purpose can be written: `PATCH /gateway/groups/{name}` `{"purpose"}` |
| O-2 ★ | Estimate and Measure: NOT POSSIBLE VIA API (G13). The result is readable: `predicted` on `GET /backends/{id}` (`prompt_per_second`, `predicted_per_second`, `ttft_ms`, `measured_at`, `source`). `POST /measurements/lookup` with the request **itself** as body, e.g. `{"backend_id":"qa-qwen8b"}` → `fingerprint`, `exact`, `near`; an unknown backend → error. Delete only an entry you measured: `DELETE /measurements` `{"fingerprint","measured_at"}` (`admin`); unknown → `404` |
| O-3, O-4 | NOT POSSIBLE VIA API (G11, G12) |
| O-5 ★ | NOT POSSIBLE VIA API (G14) |
| O-6 ★ | Start: NOT POSSIBLE VIA API (G13, G14). Cancel of a sweep started in the window **is** possible: find `id` of the running task in `GET /operations`, `POST /operations/{id}/abort` → `stopped`, `routed_to.op` = `optimize_cancel_job`. Then check every backend it stopped with `GET /backends` |
| O-7 | NOT POSSIBLE VIA API (G11–G14) |
| N-1 | NOT POSSIBLE VIA API (G15) |
| N-2 ★, N-3, N-4 ★ | NOT POSSIBLE VIA API (G16). A sweep started in the window can be watched in `GET /operations` and aborted as in O-6 (`bench_run_cancel`) |
| N-5 | NOT POSSIBLE VIA API (G16) |

Second source for all of them: `optimize-*.json`, `bench-*.json`.

## 12. Models, MCP, settings

```powershell
# M-1 (owner's agreement; $SMALL not used by a running backend)
$o = Assert-OperationDone M-1 (Invoke-Llmdil POST "/models/$SMALL/download")      # done = started, not finished
$task = $o.Detail.task_id
do { Start-Sleep 2; $m = (Invoke-Llmdil GET "/models/$SMALL").Body } until ($m.partial_bytes -gt 0 -or -not $m.downloading)
Assert M-1 ($m.catalog_size_bytes -and $m.partial_bytes -le $m.catalog_size_bytes) "percentage $([int](100 * $m.partial_bytes / $m.catalog_size_bytes))"
Assert-OperationFailed M-1 (Invoke-Llmdil POST "/models/$SMALL/download") '*already in flight*'
Assert M-1 ((Invoke-Llmdil POST "/models/$SMALL/download/pause").Body.paused) 'pause'
Assert M-1 ((Invoke-Llmdil GET "/models/$SMALL").Body.paused) 'paused in the list'
$null = Assert-OperationDone M-1 (Invoke-Llmdil POST "/models/$SMALL/download/resume")
Assert M-1 ((Invoke-Llmdil POST "/models/$SMALL/download/cancel").Body.cancelled) 'cancel'
Assert-ApiError M-1 (Invoke-Llmdil POST "/models/$SMALL/download/cancel") 404 'NOT_FOUND'
Assert-OperationFailed M-1 (Invoke-Llmdil POST "/models/$SMALL/download/resume") '*no download in flight*'
# after a cancel the partial file is gone: start again, then wait for the app task
$o = Assert-OperationDone M-1 (Invoke-Llmdil POST "/models/$SMALL/download")
$null = Wait-Operation $o.Detail.task_id -TimeoutSec 7200
do { Start-Sleep 5; $m = (Invoke-Llmdil GET "/models/$SMALL").Body } while ($m.downloading -or $m.verifying)
Assert M-1 ($m.on_disk -and $m.verified_at -and -not $m.download_error) "verified $($m.download_error)"
$null = Assert-OperationDone M-1 (Invoke-Llmdil POST "/models/$SMALL/verify") -TimeoutSec 3600

# M-2
function Add-BadModel($Body, $Like) { Assert-ApiError M-2 (Invoke-Llmdil POST '/models' -Body $Body) 400 'INVALID_INPUT' $Like }
Add-BadModel @{ id = '';           source_url = 'https://example.invalid/qa.gguf'; params_b = 1 } '*id is required*'
Add-BadModel @{ id = 'qwen3-8b';   source_url = 'https://example.invalid/qa.gguf'; params_b = 1 } '*already exists*'
Add-BadModel @{ id = 'qa-model';   source_url = 'http://example.invalid/qa.gguf';  params_b = 1 } '*https*'
Add-BadModel @{ id = 'qa-model';   source_url = 'https://example.invalid/qa.bin';  params_b = 1 } '*.gguf*'
Add-BadModel @{ id = 'qa-model';   source_url = 'https://example.invalid/qa.gguf' }               '*params_b*'
Add-BadModel @{ id = 'qa-model';   source_url = 'https://example.invalid/qa.gguf'; params_b = 1; sha256 = 'abc' } '*sha256*'
Assert-ApiError M-2 (Invoke-Llmdil GET '/models/qa-model') 404 'NOT_FOUND'
```

| Id | API | Second source |
|---|---|---|
| M-1 | Block above. Speed is not in the API; take it from two readings, or from `download-progress` on `GET /events`. The dialog: N/A (screen-only) | Size of the file and of its `.part` file; SHA-256 |
| M-2 | Block above. Lookup: NOT POSSIBLE VIA API (G05). Form position: N/A (screen-only) | `config.json` |
| K-1, K-2 | NOT POSSIBLE VIA API (G07). Read-only: `GET /config` → `.mcp.servers` (no credential values) | `config.json` |
| T-1 | `PUT /telemetry/interval` `{"ms":-1}` → `400`; `{"ms":"x"}` → `400`; `{"ms":0}` is valid (off). `{"ms":999999999}`: expected `400`, today `200` (D11): put the old value back at once | `telemetry.poll_interval_ms` in `config.json` |
| T-2 ★ | Block below (ask first) | Listening port; `Get-NetFirewallRule` |
| T-3 | `PATCH /gateway/config` `{"bind_host":"127.0.0.1"}` → 200, `restart_required: true`; one plan row of kind `gateway_bind`; `{"bind_host":"nope"}` → `400 INVALID_INPUT`. Apply, check, put back, apply | `Get-NetTCPConnection` local address |
| T-4 ★ | `POST /mxc/self-test` → `cases[]`, `summary.failed` = 0; nothing says whether the runtime is installed (G19). `PUT /mxc/mode` `{"mode":"nonsense"}` → `400`. `{"mode":"enforced"}`: expected a refusal, today `200` (D12): only with the owner's agreement, and put the old mode back | `mxc.mode` in `config.json` |
| T-5 | NOT POSSIBLE VIA API (G08). Read: `GET /config` → `.notifications` | `config.json` |
| T-6 | NOT POSSIBLE VIA API (G01, G09). Read: `.supervision.mode`; `service_managed` on backends and gateway | See the Windows services plan |
| T-7 | Block below (`admin`) | The zip itself |

```powershell
# T-2 (ask first; the firewall step may open a Windows prompt on the machine)
$old = (Get-SavedConfig).gateway.port
Assert-ApiError T-2 (Invoke-Llmdil PATCH '/gateway/config' -Body @{ port = (Get-Backend 'qa-qwen8b').port }) 422 'CONFIG' '*claimed by both*'
Assert-ApiError T-2 (Invoke-Llmdil PATCH '/gateway/config' -Body @{ port = 0 }) 422 'CONFIG' '*port 0*'
Assert-ApiError T-2 (Invoke-Llmdil PATCH '/gateway/config' -Body @{ port = 70000 }) 400 'INVALID_INPUT'              # today 500 JSON (D07)
$new = Get-FreePort 18400
Assert T-2 ((Invoke-Llmdil PATCH '/gateway/config' -Body @{ port = $new }).Status -eq 200) 'saved'
Assert T-2 ((Get-Plan).pending) 'the plan shows it'                                                                   # today: no row (D15)
Assert T-2 ((Invoke-Llmdil GET '/gateway/status').Body.port -eq $old) 'status still shows the running port'           # today: the saved one (D15)
$null = Assert-OperationDone T-2 (Invoke-Llmdil POST '/gateway/restart' -Body @{})
Assert T-2 ([bool](Get-NetTCPConnection -State Listen -LocalPort $new -ErrorAction Ignore)) 'listens on the new port'
$null = Invoke-Llmdil PATCH '/gateway/config' -Body @{ port = $old }
$null = Assert-OperationDone T-2 (Invoke-Llmdil POST '/gateway/restart' -Body @{})
Get-NetFirewallRule | Where-Object DisplayName -like 'Local LLM Gateway (TCP*' | Select-Object DisplayName, Enabled   # only the old port's rule, if any

# T-7
$o = Assert-OperationDone T-7 (Invoke-Llmdil POST '/diagnostics/bundles')
$r = Invoke-Llmdil GET "/diagnostics/bundles/$($o.Detail.bundle_id)/content"
Assert T-7 ($r.Status -eq 200 -and $r.Bytes[0] -eq 0x50 -and $r.Bytes[1] -eq 0x4B) 'zip'
$zip = [IO.Compression.ZipArchive]::new([IO.MemoryStream]::new($r.Bytes))
$hit = foreach ($e in $zip.Entries) {
    $t = [IO.StreamReader]::new($e.Open()).ReadToEnd()
    if ($t -match 'sk-[A-Za-z0-9_-]{16,}|postgres(ql)?://[^\s"]*:[^\s"]*@|"password"\s*:\s*"[^"]+"') { $e.FullName }   # names only, never the match
}
Assert T-7 (-not $hit) "no secrets; suspicious files: $($hit -join ', ')"
Assert T-7 ((Invoke-Llmdil GET '/diagnostics/bundles/nope/content').Status -eq 404) 'unknown bundle'
```

## 13. Updates (owner's agreement)

| Id | API | Second source |
|---|---|---|
| U-1 | `POST /updates/check` → `checked_at`, `components[]` `{id, installed_version, latest_available, target_version, pinned, pinned_version, update_available, applyable, note, check_error}`. The result cannot be read again: `GET /updates` gives only pins and `last_checked` (G18). `GET /app-update` is the app's own update state | `updates/last-check.json` |
| U-2 | `POST /updates/components/llamacpp/apply` `{}` (`admin`) with only `qa-qwen8b` running → `task_id` and `plan`, or `message: nothing to update`. `Wait-Operation $taskId -TimeoutSec 1800`; `detail.results[]` has the outcome and message. A second apply meanwhile: `400 INVALID_INPUT`, `*already running*` | `updates/history.json`; R-1 again |
| U-3 | After P-3: `POST /updates/components/litellm/apply` `{"version":"<newer>"}`; wait as U-2; then W-1 and W-2. Without `version` → `400` | `litellm_version` in `GET /gateway/status` |
| U-4 | Pin: `PUT /updates/components/litellm/pin` `{"version":"0.0.0-qa"}`: expected `400`, today `200` (D13): put the old pin back at once (`{"version": null}` clears). Apply refuses: `…/litellm/apply` `{"version":"not-a-version"}` → `400 INVALID_INPUT`; `…/llamacpp/apply` `{"version":"b1"}` → `400`; `…/postgres/apply` → `501 UNSUPPORTED` | `updates.components` in `config.json` |

## 14. Negative checks of the API itself

No UI counterpart. Ids `X-n`.

```powershell
# X-1 a body id must not override the path id (both are own backends). Expected 400; today it edits the OTHER one (D04).
$n = (Get-SavedBackend 'qa-coder7b').flags
$r = Invoke-Llmdil PUT '/backends/qa-qwen8b/flags' -Body @{ id = 'qa-coder7b'; n_ctx = 12288; n_batch = 512 }
Assert-ApiError X-1 $r 400 'INVALID_INPUT'
Assert X-1 ((Get-SavedBackend 'qa-coder7b').flags.n_ctx -eq $n.n_ctx) 'the other backend is untouched'
if ((Get-SavedBackend 'qa-coder7b').flags.n_ctx -ne $n.n_ctx) { $null = Invoke-Llmdil PUT '/backends/qa-coder7b/flags' -Body @{ n_ctx = $n.n_ctx; n_batch = $n.n_batch } }

# X-2 abort of a finished operation must not rewrite it. Expected done; today aborted (D02).
$o = Wait-Operation (Invoke-Llmdil POST '/setup/audit')
$null = Invoke-Llmdil POST "/operations/$($o.Id)/abort"
Assert X-2 ((Invoke-Llmdil GET "/operations/$($o.Id)").Body.status -eq $o.Status) 'a finished operation keeps its status'

# X-3 unknown operation; list shape
Assert-ApiError X-3 (Invoke-Llmdil GET '/operations/nope') 404 'NOT_FOUND'
Assert-ApiError X-3 (Invoke-Llmdil POST '/operations/nope/abort') 404 'NOT_FOUND'
Assert X-3 ((Invoke-Llmdil GET '/operations').Body -isnot [array]) 'documented envelope {operations}'          # today a bare array (D10)

# X-4 RestartNow must restart (qa-qwen8b running). Today it never does (D01): run only once, it leaves a task "running".
$p0 = Get-Pid 'qa-qwen8b'
$r = Invoke-Llmdil PATCH '/backends/qa-qwen8b/config' -Body @{ patch = @{ max_retries = 5 }; apply_mode = 'RestartNow' }
Start-Sleep 30
Assert X-4 ((Get-Pid 'qa-qwen8b') -ne $p0) 'restarted'
Assert X-4 ((Invoke-Llmdil GET "/operations/$($r.Body.restart_task_id)").Body.status -ne 'running') 'task finished'
$null = Invoke-Llmdil POST "/operations/$($r.Body.restart_task_id)/abort"      # clear the stuck record

# X-5 a typed name in a gateway patch must not be ignored. Expected 400; today 200, nothing saved (D07).
Assert-ApiError X-5 (Invoke-Llmdil PATCH '/gateway/config' -Body @{ no_such_field = 1 }) 400 'INVALID_INPUT'

# X-6 routing test needs a group
Assert X-6 ((Invoke-Llmdil POST '/gateway/routing/test' -Body @{ group = 'qa-small' }).Body.dry_run) 'dry run'
Assert-ApiError X-6 (Invoke-Llmdil POST '/gateway/routing/test' -Body @{ group = 'qa-none' }) 404 'NOT_FOUND'
Assert-ApiError X-6 (Invoke-Llmdil POST '/gateway/routing/test' -Body @{}) 400 'INVALID_INPUT'                 # today 500 GATEWAY
```

## 15. Coverage

**Full**: every check of the case that is not about the window's layout can
be done by API. **Partial**: some checks cannot (reason given). **None**.

| Case | API | Missing |
|---|---|---|
| P-1 | full | |
| P-2 | partial | File copy only; no route for backup |
| P-3 | partial | File copy only |
| P-4 | full | |
| S-1 | partial | Layout, pause, views |
| S-2 | none | Screen-only |
| S-3 | partial | Snapshot content (G20), labels |
| S-4 | partial | Sort, filter, menus |
| S-5 | partial | Forms |
| S-6 | partial | Colours, follow, recents, folder; D05 |
| S-7 | partial | Routing file (G17), labels |
| S-8 | partial | MCP (G07), Optimize (G11), Bench (G15, G16) |
| S-9 | partial | Styling |
| S-10 | full | |
| L-1, L-2 | none | Screen-only |
| L-3 | partial | Tray |
| L-4 | partial | Window |
| G-1 | full | |
| G-2 | partial | Name rules (G06) |
| G-3 | partial | Empty-only rule (G06), confirmation |
| B-1 … B-5 | full | The form's preview (G02) |
| B-6 | full | |
| B-7 | full | |
| B-neg (refusals) | full | Form focus, captions, live memory lines (G02, G03) |
| A-1, A-2, A-3, A-4 | full | |
| A-5 | full | Needs admin |
| A-6 | partial | Buttons; D09 |
| A-7 | full | Fails today |
| R-1 | partial | Tool-call test (G04) |
| R-2 … R-6 | full | |
| R-7 | full | Confirmation is text only (D03) |
| R-8, R-9 | full | Fail today |
| R-10 … R-13 | full | |
| C-1 | partial | Screen rules |
| C-2, C-3 | full | |
| C-4 | partial | Discard question |
| C-5 | full | |
| W-1 … W-8 | full | |
| W-9 | partial | Drag, menu |
| W-10 | partial | "Why" |
| W-11 | full | |
| O-1 | none | G11 |
| O-2 | partial | Run (G13); result readable |
| O-3, O-4, O-5 | none | G11, G12, G14 |
| O-6 | partial | Start (G13, G14); cancel possible |
| O-7 | none | G11–G14 |
| N-1 | none | G15 |
| N-2, N-4 | partial | Start (G16); watch and cancel possible |
| N-3, N-5 | none | G16 |
| M-1 | partial | Speed, dialog |
| M-2 | partial | Lookup (G05), form |
| K-1, K-2 | none | G07 |
| T-1, T-2 | full | Fail today (D11, D15) |
| T-3 | full | |
| T-4 | partial | Runtime state (G19) |
| T-5 | none | G08 |
| T-6 | none | G01, G09 |
| T-7 | full | Needs admin |
| U-1 | partial | Result not readable again (G18) |
| U-2, U-3, U-4 | full | U-4 fails today (D13) |

## 16. API gaps

Line numbers are in `crates/manager-app/src/registry.rs`. "Excluded" cites
the decision; "omission" means no document gives a reason. Of 159
operations, 89 have a route.

| Gap | UI action that cannot be done | Operation (line) | Why it matters for tests | Excluded or omission | Proposed route |
|---|---|---|---|---|---|
| G01 | Supervision mode and other settings sections (PostgreSQL port, WinML, log streaming, view modes) | `update_config` (198) | T-6, and putting settings back, need the window | Excluded: PLAN.md decision 11, rest-api.md §6.12, §6.16 | Section routes, not the raw patch: `PUT /settings/supervision`, `PUT /settings/{section}` with a closed body |
| G02 | Add-form preview (memory per GPU, offload range, cache choice) | `backend_add_preview` (209) | "Fits" can be checked only after creating the backend | Excluded: §6.16 "desktop-only preview" | `POST /backends/preview` |
| G03 | Cache and context preview on a backend | `backend_cache_preview` (223) | Memory lines of unsaved edits cannot be compared with the saved result | Excluded: §6.16 | `POST /backends/{id}/cache/preview` |
| G04 | Test tool calls | `backend_test_tool_call` (347) | R-1 stays partial; `tool_call_probe` can be read but never refreshed | Excluded: §6.16 files it under MCP (decision 10), but it is a backend probe with no MCP state | `POST /backends/{id}/test-tool-call` |
| G05 | Model lookup and file header | `model_hf_lookup` (271), `model_hf_header` (272) | M-2 lookup | Excluded: registry comment 265–270, §6.16 | `POST /models/lookup`, `POST /models/header` |
| G06 | Create and delete a group **with the app's rules** | `gateway_create_group` (324), `gateway_delete_group` (325) | The REST group routes accept names the app refuses and delete groups with members, so G-2 and G-3 test a different rule | Excluded with a replacement that is weaker: registry comment 314–322, §6.5 | Let `POST` and `DELETE /gateway/groups` call these operations; keep `?cascade=true` for the old delete |
| G07 | MCP list, save, test, exposure, delete, preflight | `mcp_list` (340), `mcp_start_preflight` (341), `mcp_test_connection` (342), `mcp_set_exposure` (343), `mcp_save_server` (344), `mcp_delete_server` (345) | K-1, K-2. `mcp_store_credential` (346) should stay without a route | Excluded: PLAN.md decision 10 | `GET /mcp/servers`, `PUT`/`DELETE /mcp/servers/{id}`, `POST …/{id}/test`, `PUT …/{id}/exposure`, `GET /mcp/preflight` |
| G08 | Notification settings and test toast | `notifications_set_config` (350), `notifications_send_test` (351) | T-5 | Excluded: §6.16 (delivery is a shell concern). The settings half is plain configuration | `PUT /notifications`, `POST /notifications/test` |
| G09 | Windows services list, register, unregister | `services_list` (354), `services_register` (355), `services_unregister` (356) | T-6 and the services plan cannot read registration state | Excluded: §6.16 (elevation). Reading needs no elevation | `GET /services` first |
| G10 | Log receiver status | `log_receiver_status` (361) | Start, stop and restart are routed; their result cannot be read back | Excluded: §6.16 (shows local paths) | `GET /log-receiver` without paths |
| G11 | Optimize: state, purpose, plan items, apply, discard | `optimize_get_state` (374), `optimize_validate_plan` (375), `optimize_save_purpose` (376), `optimize_set_item_state` (377), `optimize_set_item_outcome` (378), `optimize_discard_plan` (379), `optimize_apply_plan` (384), `optimize_set_experiment_state` (385) | O-1, O-3, O-4, O-7 | Excluded: §6.16 "desktop-only in v1" | `GET /optimize`, `PUT /optimize/purpose`, `PUT /optimize/plan/items/{id}`, `POST /optimize/plan/apply`, `DELETE /optimize/plan` |
| G12 | Optimize: consult, discover, cancel | `optimize_consult` (386), `optimize_discover` (387), `optimize_cancel_consult` (388) | O-3, O-7 | As G11 | `POST /optimize/consult`, `POST /optimize/discover` (operations) |
| G13 | Optimize: measure, estimate, baseline, experiments, cancel | `optimize_measure_backend` (389), `optimize_estimate_backend` (397), `optimize_cancel_job` (398), `optimize_run_baseline` (399), `optimize_run_experiments` (400), `optimize_estimate_experiments` (401) | O-2, O-6: the best candidates for unattended runs | As G11, although the measurement **store** is routed (395, 396) | `POST /backends/{id}/measure`, `POST /backends/{id}/estimate` (operations, aborted through `/operations`) |
| G14 | Optimize: tuning preview, run, adopt | `optimize_tuning_preview` (419), `optimize_tuning_run` (420), `optimize_tuning_adopt` (421) | O-5, O-6, O-7 | As G11 | `GET /backends/{id}/tuning`, `POST …/tuning/run`, `POST …/tuning/adopt` |
| G15 | Bench prompts | `bench_prompts_list` (406), `bench_prompt_save` (407), `bench_prompt_delete` (408), `bench_prompt_generate` (409), `bench_prompt_generate_cancel` (410) | N-1 | Excluded: registry comment 402–405, §6.16 | `GET`/`POST /bench/prompts`, `DELETE /bench/prompts/{id}`, `POST /bench/prompts/generate` |
| G16 | Bench sweeps and results | `bench_run_start` (411), `bench_run_cancel` (412), `bench_results_list` (413), `bench_result_delete` (414) | N-2 to N-5 | As G15 | `POST /bench/runs` (operation), `POST /bench/runs/{id}/cancel`, `GET /bench/results`, `DELETE /bench/results/{id}` |
| G17 | The generated routing file (second tab of *generated config*) | none; `gateway_preview_yaml` (311) has `routing_py`, the API layer drops it (`http.rs` 452–461) | W-7 cannot check the rule that will run without reading files | Omission | `GET /gateway/generated-routing` |
| G18 | Result of the last update check | none (file `updates/last-check.json`) | U-1 "still shown" | Omission | `GET /updates/last-check` |
| G19 | Is the MXC runtime installed | none | T-4 | Omission | `runtime` in `GET /mxc/policy` |
| G20 | Snapshot content, list, delete | none; `telemetry_save_snapshot` (330) returns a path | The test leaves files it cannot list or remove | Excluded: rest-api.md §7 item 4 | `GET /telemetry/snapshots`, `GET`/`DELETE …/{id}` |
| G21 | Unattended credential above operator | none; `auth/api_tokens.rs` 19 refuses configurator and admin | An automated run must refresh a 60-minute session | Excluded: auth-plan.md §3.3 and §6; the owner can decide otherwise | Short-lived (≤ 24 h) configurator token, owner-issued in the window |
| G22 | Wait for readiness on start | `start_backend` (201) has no `wait_ready_secs`; `restart_backend` (203) has it | Every start needs a second polling loop | Omission | Add `wait_ready_secs` to `POST /backends/{id}/start` |

No route and not needed by any case: `open_url` (244), `open_folder` (245),
`gateway_master_key` (248, decision 4), `gateway_activation_complete`
(303), `gateway_activation_record_execution` (308), `log_receiver_deploy`
(362), `log_receiver_remove` (363), `log_receiver_flip_endpoint` (364),
VS Code (422, 423), WinML (432–435). Served inside another route:
`gateway_set_bind_host` (251), `model_set_short_name` (261),
`model_set_enabled` (273), `model_set_offload_spec` (276),
`model_set_mtp` (280), `activation_apply` (300).

## 17. Defects that break automation

A route exists, but its answer or effect makes a test impossible or wrong.
Read in the code; not yet reproduced on a running app. Tracked in
https://github.com/pdrakeweb/llmdil/issues/6.

| Id | Defect | Where |
|---|---|---|
| D01 | `apply_mode: RestartNow` saves and records a `backend_restart` task, but nothing restarts the backend and the task stays `running` for the life of the app. The answer looks like a real restart | `ops/backend_config.rs` 115–123 |
| D02 | `POST /operations/{id}/abort` with the id of a `202` answer only rewrites the API's record. The work goes on, and a `done` or `failed` record becomes `aborted`. Only the app's own task id (found in `GET /operations` by `kind` and `detail`) really cancels | `http.rs` 296–305, `operations.rs` 144 |
| D03 | A failed operation has text, no `code`. Argument errors and busy refusals on `202` routes are failed operations, so `400` and `409 BUSY` never come from them; the `409` branch for a busy setup step is dead code (the route is async) | `http.rs` 255–270, 341–355, 443 |
| D04 | **Fixed.** A field in the body replaces the path parameter of the same name: `PUT /backends/a/flags` with `"id": "b"` edits `b`, while the audit log names `a` | `http.rs` 367–411 |
| D05 | Query values that look like a number, `true`, `false`, `inf` or `nan` are converted: `search=8080` is `400`, `search=inf` becomes "no filter" | `http.rs` 425 |
| D06 | `PATCH /backends/{id}/config` and `preview-diff`: unknown patch fields are ignored (200); wrong types are `500 JSON`; an unknown backend is `422` on apply; `raw_flags: null` cannot clear the flags; context and batch limits of the other routes are not checked | `diff.rs` 28, `ops/backend_config.rs` 60 |
| D07 | `PATCH /gateway/config`: unknown fields are ignored (200); wrong types and out-of-range numbers are `500 JSON` | `ops/gateway.rs` 49 |
| D08 | `POST /gateway/groups` accepts any non-empty name (spaces, 63 characters, `openrouter`); `DELETE` removes a group with members (its role class now matches the app's) | `composites.rs` 189, 264 |
| D09 | Group changes made by REST are journaled as "Gateway configuration changed" with no targets: a test cannot tell its own plan rows from the owner's | `ops/gateway.rs` 55–64 |
| D10 | `GET /operations` is a bare array in no order. App tasks have no `error`, `started_ms`, `finished_ms`. Most `202` calls appear twice, with two ids and two kinds | `ops/logs.rs` 473, `http.rs` 273–295 |
| D11 | `PUT /telemetry/interval` accepts any number, and changes memory before saving | `ops/telemetry.rs` 264 |
| D12 | **Fixed.** `PUT /mxc/mode` accepts `enforced` without the runtime | `ops/platform.rs` 64 |
| D13 | **Fixed.** `PUT /updates/components/{id}/pin` and `/auto` accept any component and any version, and create the entry | `ops/platform.rs` 234 |
| D14 | `POST /backends/{id}/test` holds the configuration read lock until the model answers (up to 30 s): a save from anywhere waits, and reads after it wait too | `ops/backends.rs` 838 |
| D15 | **Fixed.** A changed gateway port is not a plan row (the generated YAML does not contain it), and `GET /gateway/status` `port` is the saved port while the gateway still listens on the old one | `ops/gateway.rs` 39 and 679 |
| D16 | Error messages are cut at 400 characters: a memory refusal for two GPUs may lose "who uses it" | `http.rs` 539 |
| D17 | `POST /setup/audit` during another audit ends `done` with `already_running: true`, zero steps | `ops/wizard.rs` 527–545 |
| D18 | `done` does not mean finished for start (spawned), download (started) and, by `restart_task_id`, config saves | `backend_lifecycle.rs` 590–650, `ops/models.rs` 526 |
| D19 | Only error messages are cleaned of local paths. `detail` of downloads, diagnostics and steps, `launch-command`, snapshots and `/config` contain them | `http.rs` 341–355, 529 |
| D20 | `GET /logs` with an unknown `level` answers unfiltered (200), while an unknown `order` is `400`; a wrong `group` type in the routing test is `500 GATEWAY` | `ops/logs.rs`, `op_gateway_test_routing` |
| D21 | **Fixed.** While the gateway runs, any unapplied gateway change adds a `gateway_drift` row next to its own row, so one edit counts as two changes (`activation_changes` too) | `ops/gateway_activation.rs` 109–123 |

## 18. Put back and compare

```powershell
# 1. Stop and disable every qa- backend (delete only if the owner asked)
$qa = @((Invoke-Llmdil GET '/backends').Body.backends | Where-Object id -like 'qa-*')
foreach ($b in $qa) {
    if ($b.state -notin 'stopped', 'failed') { $null = Wait-Operation (Invoke-Llmdil POST "/backends/$($b.id)/stop" -Body @{ graceful = $true }) }
    $null = Invoke-Llmdil PUT   "/backends/$($b.id)/flags-mode" -Body @{ advanced = $false }
    $null = Invoke-Llmdil PATCH "/backends/$($b.id)/config" -Body @{ patch = @{ enabled = $false }; apply_mode = 'OnNextRestart' }
    # asked to delete (admin): $null = Invoke-Llmdil DELETE "/backends/$($b.id)"
}
# asked to delete: the empty groups afterwards
# 'qa-small','qa-moe','qa-split' | ForEach-Object { $null = Invoke-Llmdil DELETE "/gateway/groups/$_" }

# 2. Apply, then nothing may be pending
Assert END (@(Get-ForeignPlanRows).Count -eq 0) 'only own rows'
$null = Assert-OperationDone END (Invoke-Llmdil POST '/activation-plan/apply' -Body @{ mode = 'now' })
Assert END (-not (Get-Plan).pending) 'plan empty'

# 3. Compare with the backup, field by field (keys and ids only)
function Get-Leaves($o, $p = '') {
    if ($o -is [Management.Automation.PSCustomObject]) { foreach ($q in $o.PSObject.Properties) { Get-Leaves $q.Value "$p.$($q.Name)" } }
    elseif ($o -is [array]) { for ($i = 0; $i -lt $o.Count; $i++) { $k = $o[$i].id ?? $o[$i].name ?? $i; Get-Leaves $o[$i] "$p[$k]" } }
    else { "$p=$o" }
}
$a = Get-Leaves (Get-Content '<backup>\config.json' -Raw | ConvertFrom-Json -Depth 60)
$z = Get-Leaves (Get-SavedConfig)
$diff = Compare-Object $a $z | Where-Object { $_.InputObject -notmatch 'qa-' } | ForEach-Object { ($_.InputObject -split '=', 2)[0] } | Sort-Object -Unique
$diff      # allowed: assigned_backends of the used models, verified_at, last_checked, and what the owner approved

# 4. Results and the end of the session
$script:Results | Group-Object Case | ForEach-Object {
    [pscustomobject]@{ Case = $_.Name; Result = ($_.Group.Result -contains 'FAIL' ? 'FAIL' : 'PASS') }
}
$null = Invoke-Llmdil POST '/auth/logout'
Remove-Item Env:LLMDIL_TOKEN, Env:LLMDIL_REFRESH_TOKEN, Env:LLMDIL_GATEWAY_KEY -ErrorAction Ignore
```

5. Restart the owner's backends that were stopped (`POST
   /backends/{id}/start`, then `Wait-Backend`) **before** the logout.
   Check the gateway, one app process, no leftover llama-server.
6. Not undone by any route: telemetry snapshots (G20), diagnostics
   bundles, history rows, audit rows, peaks, the stuck task of X-4.
   Dashboard view and the files of section 11: as in the UI test, with the
   app closed. List what stays.
7. Tell the owner to delete the test user and, if they wish, switch the
   Admin API off.

Result sheet: as in the UI test, plus `API commit: ____  Role: ____`.

## Appendix A: documents that disagree with the code

| Document says | Code does |
|---|---|
| rest-api.md header, §6.16: 158 operations, 69 without a route; registry test is named "82 commands" | 159 operations, 89 routed, 70 not |
| rest-api.md §5: 78 registry routes, 117 records | 89 + 15 + 24 = 128 |
| rest-api.md §2.1: base `http://…` | HTTPS, except a loopback development mode |
| §2.2, §2.5: unknown fields rejected | Not inside `patch` (D06) or the gateway config (D07) |
| §2.5, §6.7: `409 BUSY` for a running setup step, download, apply | `202`, then `failed` (D03). A download in flight is kind `Models` |
| §3.1: `kind` is `wizard_step`, `backend_restart`, `model_download`, … | API records carry the operation name (`run_wizard_step`, `restart_backend`, `download_model`) |
| §6.2: `RestartNow` names an operation that restarts | D01 |
| §6.2: delete while running is `409` / `Supervisor` | `400 INVALID_INPUT` |
| §6.2: `POST /backends` answers `{backend_id, group}`; its cache fields are not listed | Also `persisted`; `cache_k`, `cache_v`, `flash_attn` exist. `PUT …/cache` is not documented at all |
| §6.2: test allows up to 300 s | Client timeout inside the app is 30 s |
| §6.3: verify without a pin fails with `MODELS` | Invalid input (as a failed operation) |
| §6.4: propose answers `weights_only: true` | false when a backend is named |
| §6.5: deleting a group is `destructive` | Class C (configurator) |
| §6.6: quiesce wait at most 120 s | Clamped to 5–600 s |
| §6.7: `already_running` becomes `409` | `done` with `already_running: true` (D17) |
| §6.10: `{operations: […]}`, newest first | Bare array, no order (D10) |
| §6.10: aborting a finished operation changes nothing | API records become `aborted` (D02) |
| §6.14, §7 item 9, PLAN.md: `updates/check` and `apply` are `501` stubs; apply stays local | Both work; apply is routed for admins and answers `200` with `task_id`. Only `postgres` and `mxc` answer `501` |
| §6.18: lookup body `{"request": {…}}` | The request object itself is the body |
| auth-plan.md §5: enabling needs an admin user | The switch starts the listener without checking (`src-tauri/src/main.rs` `admin_api_set_enabled`); without a user nobody can sign in |
| auth-plan.md §4.3: the disclosure reads are audited | Only POST, PUT, PATCH, DELETE are; `GET …/launch-command` and bundle content are not |
| auth-plan.md §6: 403s are audited; denied and unknown routes look alike | 403 is not audited; unknown is `404`, denied `403` |
| Gateway page tip: the plan tracks a port change | It does not (D15) |

Since this was written, R-8 (the program is named), R-9 (restart by
policy) and both G-2 cases were implemented. A-7 still stops at the first
failed backend.

## Appendix B: not determined from the code

| Case | Open question |
|---|---|
| W-11 | Status of a wrong model name (LiteLLM decides: 400, 401 or 404) |
| W-1 | How evenly requests spread (LiteLLM's strategy) |
| W-6 | Whether counts survive the restart to the digit (the window is re-read from LiteLLM's counters) |
| W-9 | Whether moving a backend and moving it back leaves two plan rows or none |
| B-7 | What `/props` reports for a context that is not a multiple of the slot count |
| T-2 | Whether the old firewall rule is removed without a Windows prompt |
| U-2, U-3 | Duration and rollback: need real releases |
