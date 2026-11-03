<#
.SYNOPSIS
    Compatibility wrapper: deploy the LLMdil log receiver from the
    command line (the primary path is inside the app).

.DESCRIPTION
    RETIRED AS THE PRIMARY PATH (v0.1.53): the receiver and the
    service host ship inside the LLMdil installer, and deployment is
    driven from the app — Settings → Supervision → Log receiver →
    Deploy (one UAC prompt). The endpoint flip is the separate Flip
    button on the same card. See log-receiver-checklist.md.

    This wrapper remains for scripted use: it invokes the installed
    receiver binary's own `deploy` subcommand — the same elevated
    path the app drives — which creates the receiver directory and
    ACLs, seeds the token files (ingest from the Credential Manager
    target the shipper uses; read generated fresh and kept on
    redeploy), registers the auto-start LocalService service, adds
    the tailnet read-listener firewall rule, starts the service,
    and verifies localhost ingest health. When the deploy generates
    the read token, the token is printed once by this wrapper for
    Secure Vault filing (the receiver records it in
    deploy-result.json; the wrapper deletes that file after reading
    it, exactly as the app does).

    Must run elevated. The endpoint flip is deliberately NOT part of
    this wrapper: use the app's Flip endpoint button, which backs up
    config.json and goes through the app's validated config save.

.PARAMETER DataDir
    LLMdil data dir (default %APPDATA%\local-llm-service-manager).

.PARAMETER InstallDir
    LLMdil install dir holding the bundled binaries
    (default %LOCALAPPDATA%\LLMdil).

.PARAMETER IngestToken
    Override for the ingest token seed. Default: the receiver reads
    Credential Manager target
    local-llm-service-manager/log-streaming-token itself.
#>
[CmdletBinding()]
param(
    [string]$DataDir = (Join-Path $env:APPDATA 'local-llm-service-manager'),
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'LLMdil'),
    [string]$IngestToken
)

$ErrorActionPreference = 'Stop'

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Deploy-LogReceiver.ps1 must run elevated (Run as administrator). The app-driven Deploy (Settings → Supervision → Log receiver) prompts for elevation itself.'
}

$receiverExe = Join-Path $InstallDir 'llmdil-log-receiver.exe'
if (-not (Test-Path $receiverExe)) {
    throw "llmdil-log-receiver.exe not found in $InstallDir. It ships inside the LLMdil installer (v0.1.53+) — install or update LLMdil first."
}
if (-not (Test-Path (Join-Path $InstallDir 'llm-service-host.exe'))) {
    throw "llm-service-host.exe not found in $InstallDir. It ships inside the LLMdil installer (v0.1.53+) — install or update LLMdil first."
}

Write-Host '== Log receiver deploy (bundled binary, app-equivalent path) =='
Write-Host "Data dir:    $DataDir"
Write-Host "Install dir: $InstallDir"
Write-Host 'Note: the primary path is Settings → Supervision → Log receiver → Deploy in LLMdil.'

$deployArgs = @('deploy', '--data-dir', $DataDir, '--install-dir', $InstallDir)
if ($IngestToken) { $deployArgs += @('--ingest-token', $IngestToken) }
& $receiverExe @deployArgs
if ($LASTEXITCODE -ne 0) { throw "receiver deploy failed with exit code $LASTEXITCODE (see log-receiver\deploy.log in the data dir)." }

$resultPath = Join-Path (Join-Path $DataDir 'log-receiver') 'deploy-result.json'
if (Test-Path $resultPath) {
    $result = Get-Content $resultPath -Raw | ConvertFrom-Json
    Remove-Item $resultPath -Force
    if ($result.read_token_new) {
        Write-Host ''
        Write-Host 'NEW READ TOKEN (shown once — file Muse''s copy from this value):'
        Write-Host "  $($result.read_token_new)"
        Write-Host '  Muse-side copy goes to ~/.config/llmdil-logs/read-token on the VM (mode 600),'
        Write-Host '  via the Secure Vault or your preferred channel — not in chat, not in the repo.'
        Write-Host ''
    }
}

Write-Host 'Endpoint NOT flipped. Flip it deliberately from the app:'
Write-Host '  Settings → Supervision → Log receiver → Flip endpoint to local receiver,'
Write-Host '  then restart LLMdil. Until then the shipper keeps sending to the old endpoint.'
Write-Host '== Deploy complete =='
