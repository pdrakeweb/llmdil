<#
.SYNOPSIS
    Regenerate (or verify) the hash-locked gateway venv requirements.

.DESCRIPTION
    Resolves crates/manager-wizard/assets/gateway-requirements.in for the
    interpreter and platform the start-gateway step builds its venv with
    (CPython 3.12 -- manager_wizard::GATEWAY_PYTHON_VERSION -- on Windows
    x86_64) and writes crates/manager-wizard/assets/gateway-requirements.lock
    with a --hash=sha256 line for every distribution of every package.
    The lock is embedded in the app (include_str!); the start-gateway step
    installs from it with `uv pip install --require-hashes` whenever the
    configured LiteLLM pin equals the lock's litellm version.

    Run after bumping manager_config::DEFAULT_LITELLM_PIN or
    manager_wizard::PRISMA_PINNED_VERSION (update the .in file first; the
    `checked_in_gateway_lock_matches_pins` test fails until both agree), then
    review the diff like code and commit the .in and .lock together.

    -Check re-resolves into a temporary copy (uv keeps the existing pins
    when they still satisfy the input) and fails when the result differs
    from the checked-in lock.

    Hashes come from uv's resolution against PyPI. Never edit them by hand.

.PARAMETER Uv
    The uv executable. Defaults to `uv` on PATH; the app's bootstrapped
    copy (<data dir>\bin\uv.exe) works too.

.EXAMPLE
    pwsh tools/gateway-lock/Update-GatewayLock.ps1
.EXAMPLE
    pwsh tools/gateway-lock/Update-GatewayLock.ps1 -Check
#>
[CmdletBinding()]
param(
    [string]$Uv = 'uv',
    [switch]$Check
)

$ErrorActionPreference = 'Stop'
$repo = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$assets = Join-Path $repo 'crates\manager-wizard\assets'
$lock = Join-Path $assets 'gateway-requirements.lock'

# Keep in sync with manager_wizard::GATEWAY_PYTHON_VERSION.
$pythonVersion = '3.12'
$pythonPlatform = 'x86_64-pc-windows-msvc'

$target = $lock
if ($Check) {
    $target = Join-Path ([System.IO.Path]::GetTempPath()) ("gateway-requirements-check-{0}.lock" -f [guid]::NewGuid())
    if ((Get-Content $lock -Raw) -match '(?m)^[A-Za-z0-9]') {
        Copy-Item $lock $target
    }
}

# Relative paths so the header uv writes into the lock is machine-independent.
Push-Location $repo
try {
    & $Uv pip compile `
        'crates/manager-wizard/assets/gateway-requirements.in' `
        --generate-hashes `
        --python-version $pythonVersion `
        --python-platform $pythonPlatform `
        --custom-compile-command 'tools/gateway-lock/Update-GatewayLock.ps1' `
        --output-file $target
    if ($LASTEXITCODE -ne 0) { throw "uv pip compile failed (exit $LASTEXITCODE)" }
} finally {
    Pop-Location
}

if ($Check) {
    $expected = (Get-Content $lock -Raw) -replace "`r`n", "`n"
    $actual = (Get-Content $target -Raw) -replace "`r`n", "`n"
    Remove-Item $target
    if ($expected -ne $actual) {
        Write-Error 'gateway-requirements.lock is out of date: run tools/gateway-lock/Update-GatewayLock.ps1 and commit the result.'
        exit 1
    }
    Write-Host 'gateway-requirements.lock is current.'
} else {
    Write-Host "wrote $lock -- review the diff, then run: cargo test -p manager-wizard gateway_lock"
}
