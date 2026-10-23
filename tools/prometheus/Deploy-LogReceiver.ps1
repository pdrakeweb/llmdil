<#
.SYNOPSIS
    Deploy the Prometheus-local LLMdil log receiver (DESIGN-prometheus-local.md).

.DESCRIPTION
    Phase 1 (deploy dark) + optional phase 2 (endpoint flip) of the
    Prometheus-local log receiver cutover:

      1. Downloads llmdil-log-receiver.exe and llm-service-host.exe
         from the GitHub release into the LLMdil install dir.
      2. Creates <data_dir>\log-receiver, grants the LocalService
         account write access, and seeds the token files:
         ingest-tokens.txt from the existing Credential Manager
         target the shipper uses (same value, so the shipper needs
         no credential change), read-token.txt generated fresh.
      3. Registers the llm-log-receiver Windows service (auto-start,
         NT AUTHORITY\LocalService, SCM restart failure actions)
         through the receiver's own install-service subcommand,
         which uses the product's manager-services registration.
      4. Adds the Windows Firewall rule for the read listener
         (TCP 8474, source 100.64.0.0/10 only).
      5. Starts the service and verifies: localhost health, a
         synthetic batch through ingest, and read-back of that
         record through the tailnet read listener.
      6. With -FlipEndpoint (phase 2, Pete's go-ahead): backs up
         config.json and sets log_streaming.endpoint to
         http://127.0.0.1:8473. Restart LLMdil afterwards to apply.

    Must run elevated. Rollback of the flip is one string: set the
    endpoint back to https://llmdil-logs.ourdrakefamily.com and
    restart LLMdil (the VM receiver stays live until the phase-4
    retirement).

.PARAMETER ReleaseTag
    GitHub release tag to take the binaries from (default v0.1.52).

.PARAMETER DataDir
    LLMdil data dir (default %APPDATA%\local-llm-service-manager).

.PARAMETER InstallDir
    LLMdil install dir the binaries are placed in
    (default %LOCALAPPDATA%\LLMdil).

.PARAMETER IngestToken
    Override for the ingest token seed. Default: read from Windows
    Credential Manager target
    local-llm-service-manager/log-streaming-token.

.PARAMETER FlipEndpoint
    Also flip log_streaming.endpoint in config.json to the local
    receiver (phase 2).

.PARAMETER SkipDownload
    Use the binaries already present in the install dir.
#>
[CmdletBinding()]
param(
    [string]$ReleaseTag = 'v0.1.52',
    [string]$Repo = 'pdrakeweb/llmdil',
    [string]$DataDir = (Join-Path $env:APPDATA 'local-llm-service-manager'),
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'LLMdil'),
    [string]$IngestToken,
    [switch]$FlipEndpoint,
    [switch]$SkipDownload
)

$ErrorActionPreference = 'Stop'
$ServiceName = 'llm-log-receiver'
$IngestUrl = 'http://127.0.0.1:8473'
$ReadPort = 8474

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Deploy-LogReceiver.ps1 must run elevated (Run as administrator).'
}

# --- Credential Manager read (same decoding rule as llm-service-host) ---
if (-not ('CredManager' -as [type])) {
    Add-Type @'
using System;
using System.Runtime.InteropServices;

public static class CredManager {
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    private struct CREDENTIAL {
        public uint Flags;
        public uint Type;
        public string TargetName;
        public string Comment;
        public long LastWritten;
        public uint CredentialBlobSize;
        public IntPtr CredentialBlob;
        public uint Persist;
        public uint AttributeCount;
        public IntPtr Attributes;
        public string TargetAlias;
        public string UserName;
    }

    [DllImport("advapi32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern bool CredRead(string target, uint type, uint flags, out IntPtr credential);

    [DllImport("advapi32.dll")]
    private static extern void CredFree(IntPtr credential);

    public static string Read(string target) {
        IntPtr ptr;
        if (!CredRead(target, 1, 0, out ptr)) return null;
        try {
            CREDENTIAL cred = (CREDENTIAL)Marshal.PtrToStructure(ptr, typeof(CREDENTIAL));
            if (cred.CredentialBlob == IntPtr.Zero || cred.CredentialBlobSize == 0) return null;
            byte[] blob = new byte[cred.CredentialBlobSize];
            Marshal.Copy(cred.CredentialBlob, blob, 0, blob.Length);
            if (blob.Length >= 2 && blob.Length % 2 == 0) {
                string u16 = System.Text.Encoding.Unicode.GetString(blob).TrimEnd('\0');
                bool ascii = u16.Length > 0;
                foreach (char c in u16) { if (c > 0x7F) { ascii = false; break; } }
                if (ascii) return u16;
            }
            return System.Text.Encoding.UTF8.GetString(blob).TrimEnd('\0');
        } finally {
            CredFree(ptr);
        }
    }
}
'@
}

function Get-ReleaseAsset([string]$Name) {
    $dest = Join-Path $InstallDir $Name
    if ($SkipDownload) {
        if (-not (Test-Path $dest)) { throw "$Name not in $InstallDir and -SkipDownload was given." }
        return $dest
    }
    if (Get-Command gh -ErrorAction SilentlyContinue) {
        gh release download $ReleaseTag --repo $Repo --pattern $Name --dir $InstallDir --clobber
        if ($LASTEXITCODE -ne 0) { throw "gh release download failed for $Name (tag $ReleaseTag)." }
    } else {
        $url = "https://github.com/$Repo/releases/download/$ReleaseTag/$Name"
        Invoke-WebRequest -Uri $url -OutFile $dest -UseBasicParsing
    }
    if (-not (Test-Path $dest)) { throw "asset $Name was not downloaded." }
    return $dest
}

function Write-Utf8NoBom([string]$Path, [string]$Text) {
    [System.IO.File]::WriteAllText($Path, $Text, (New-Object System.Text.UTF8Encoding($false)))
}

Write-Host "== Log receiver deploy (release $ReleaseTag) =="
Write-Host "Data dir:    $DataDir"
Write-Host "Install dir: $InstallDir"

# --- 1. Binaries beside the app ---
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$receiverExe = Get-ReleaseAsset 'llmdil-log-receiver.exe'
$hostExe = Get-ReleaseAsset 'llm-service-host.exe'
Write-Host "Binaries in place: $receiverExe, $hostExe"

# --- 2. Receiver dir, ACLs, token files ---
$lrDir = Join-Path $DataDir 'log-receiver'
New-Item -ItemType Directory -Force -Path (Join-Path $lrDir 'logs') | Out-Null
icacls $lrDir /grant 'NT AUTHORITY\LOCAL SERVICE:(OI)(CI)M' 'Administrators:(OI)(CI)F' | Out-Null

if (-not $IngestToken) {
    $IngestToken = [CredManager]::Read('local-llm-service-manager/log-streaming-token')
}
if (-not $IngestToken) {
    throw 'No ingest token: Credential Manager target local-llm-service-manager/log-streaming-token not found and -IngestToken not given. Set up log streaming in LLMdil first (docs/log-streaming.md), or pass -IngestToken.'
}
$ingestFile = Join-Path $lrDir 'ingest-tokens.txt'
Write-Utf8NoBom $ingestFile ($IngestToken + "`n")
icacls $ingestFile /inheritance:r /grant 'NT AUTHORITY\LOCAL SERVICE:R' 'Administrators:F' | Out-Null
Write-Host 'Ingest token seeded from Credential Manager (same value the shipper uses).'

$readFile = Join-Path $lrDir 'read-token.txt'
if (Test-Path $readFile) {
    $readToken = (Get-Content $readFile -Raw).Trim()
    Write-Host 'Read token: existing read-token.txt kept (value unchanged).'
} else {
    $bytes = New-Object byte[] 32
    [System.Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
    $readToken = ($bytes | ForEach-Object { $_.ToString('x2') }) -join ''
    Write-Utf8NoBom $readFile ($readToken + "`n")
    Write-Host ''
    Write-Host 'NEW READ TOKEN (shown once — file Muse''s copy from this value):'
    Write-Host "  $readToken"
    Write-Host '  Muse-side copy goes to ~/.config/llmdil-logs/read-token on the VM (mode 600),'
    Write-Host '  via the Secure Vault or your preferred channel — not in chat, not in the repo.'
    Write-Host ''
}
icacls $readFile /inheritance:r /grant 'NT AUTHORITY\LOCAL SERVICE:R' 'Administrators:F' | Out-Null

# --- 3. Service registration ---
& $receiverExe install-service --data-dir $DataDir --install-dir $InstallDir
if ($LASTEXITCODE -ne 0) { throw "install-service failed with exit code $LASTEXITCODE." }

# --- 4. Firewall rule (read listener, tailnet sources only) ---
Remove-NetFirewallRule -Name 'LLMdilLogReceiverRead' -ErrorAction SilentlyContinue
New-NetFirewallRule -Name 'LLMdilLogReceiverRead' -DisplayName 'LLMdil Log Receiver (Tailscale read)' `
    -Direction Inbound -Protocol TCP -LocalPort $ReadPort -RemoteAddress '100.64.0.0/10' `
    -Action Allow -Profile Any | Out-Null
Write-Host "Firewall rule LLMdilLogReceiverRead: inbound TCP $ReadPort from 100.64.0.0/10."

# --- 5. Start + verify ---
Start-Service -Name $ServiceName
$deadline = (Get-Date).AddSeconds(60)
do {
    Start-Sleep -Milliseconds 500
    $svc = Get-Service -Name $ServiceName
} while ($svc.Status -ne 'Running' -and (Get-Date) -lt $deadline)
if ($svc.Status -ne 'Running') { throw "service $ServiceName did not reach Running (status: $($svc.Status))." }
Write-Host "Service $ServiceName is Running."

$health = $null
$deadline = (Get-Date).AddSeconds(30)
while (-not $health -and (Get-Date) -lt $deadline) {
    try { $health = Invoke-RestMethod -Uri "$IngestUrl/healthz" -TimeoutSec 3 } catch { Start-Sleep -Seconds 1 }
}
if (-not $health -or -not $health.ok) { throw 'ingest /healthz did not answer ok within 30 s.' }
Write-Host "Ingest health: ok (records=$($health.records), db_bytes=$($health.db_bytes))"

$seq = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
$record = [ordered]@{
    v = 1; machine = 'synthetic-e2e'; app_version = $ReleaseTag; seq = $seq
    ts_wall = (Get-Date).ToString('yyyy-MM-ddTHH:mm:sszzz'); ts_mono_ms = 0
    source = 'wizard'; stream = 'stdout'; level = 'info'
    line = 'prometheus-local receiver synthetic verification'
}
$body = ConvertTo-Json -InputObject @($record) -Compress
$posted = Invoke-RestMethod -Method Post -Uri "$IngestUrl/v1/batches" `
    -Headers @{ Authorization = "Bearer $IngestToken" } -ContentType 'application/json' -Body $body
Write-Host "Synthetic batch posted (accepted=$($posted.accepted), duplicates=$($posted.duplicates))."

$tailnetIp = Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue |
    Where-Object {
        $o = $_.IPAddress.Split('.')
        $o.Count -eq 4 -and [int]$o[0] -eq 100 -and [int]$o[1] -ge 64 -and [int]$o[1] -le 127
    } |
    Select-Object -First 1 -ExpandProperty IPAddress
if ($tailnetIp) {
    $status = Invoke-RestMethod -Uri "http://${tailnetIp}:$ReadPort/v1/status" `
        -Headers @{ Authorization = "Bearer $readToken" } -TimeoutSec 5
    $found = Invoke-RestMethod -Uri "http://${tailnetIp}:$ReadPort/v1/query?grep=synthetic%20verification&limit=50" `
        -Headers @{ Authorization = "Bearer $readToken" } -TimeoutSec 5
    $mine = @($found.records | Where-Object { $_.seq -eq $seq })
    if ($mine.Count -lt 1) { throw 'synthetic record was NOT returned by the read listener.' }
    Write-Host "Read listener verified at ${tailnetIp}:$ReadPort (status ok, synthetic record read back; heartbeat $($status.receiver.last_heartbeat_utc))."
    Write-Host "Muse reads at http://${tailnetIp}:$ReadPort — the VM's first request to this address needs your one-time approval."
} else {
    Write-Warning 'No tailnet IPv4 (100.64.0.0/10) on this machine yet — join Tailscale (see checklist), then re-run this script or verify the read listener manually.'
}

# --- 6. Optional phase 2: flip the shipper endpoint ---
if ($FlipEndpoint) {
    $configPath = Join-Path $DataDir 'config.json'
    if (-not (Test-Path $configPath)) { throw "config.json not found at $configPath; cannot flip the endpoint." }
    $backup = "$configPath.bak-$(Get-Date -Format yyyyMMdd-HHmmss)"
    Copy-Item $configPath $backup
    $cfg = Get-Content $configPath -Raw | ConvertFrom-Json
    if (-not $cfg.log_streaming) {
        $cfg | Add-Member -NotePropertyName 'log_streaming' -NotePropertyValue ([pscustomobject]@{})
    }
    $cfg.log_streaming | Add-Member -NotePropertyName 'endpoint' -NotePropertyValue $IngestUrl -Force
    Write-Utf8NoBom $configPath ($cfg | ConvertTo-Json -Depth 32)
    Write-Host "Endpoint flipped: log_streaming.endpoint = $IngestUrl (backup: $backup)."
    Write-Host 'Restart LLMdil to apply; the shipper spools losslessly while it is down.'
} else {
    Write-Host 'Endpoint NOT flipped (phase 1 deploy-dark). Re-run with -FlipEndpoint when ready for phase 2.'
}

Write-Host '== Deploy complete =='
