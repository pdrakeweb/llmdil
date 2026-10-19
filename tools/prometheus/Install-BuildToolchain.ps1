<#
.SYNOPSIS
    Prepare a Windows machine as an LLMdil build host.

.DESCRIPTION
    Installs the toolchain an LLMdil build host needs, idempotently:

    - Git (plus core.longpaths, which Rust workspaces need to stay
      clear of the 260-character path limit)
    - Node.js LTS (the UI build)
    - Visual Studio 2022 Build Tools with the VCTools workload: the
      MSVC compiler and the Windows SDK. Tauri links against MSVC on
      Windows, so this is the piece the src-tauri check / clippy /
      test gates stand on.
    - Rust (stable, x86_64-pc-windows-msvc), installed MACHINE-WIDE
      under C:\Rust (RUSTUP_HOME and CARGO_HOME are machine-level
      environment variables, and C:\Rust\cargo\bin is added to the
      machine PATH).
    - The LongPathsEnabled filesystem setting.

    Why machine-wide Rust: a default rustup install lands under the
    installing user's profile (%USERPROFILE%\.cargo / .rustup). A
    GitHub Actions runner installed as a service runs as a service
    account, not as that user, so a per-user Rust is invisible to it
    and every cargo step fails with "command not found". C:\Rust is
    reachable by any account on the machine.

    The intended use is a GitHub Actions self-hosted runner for the
    llmdil repo (label "prometheus"): the full gate set - workspace
    tests and clippy, the src-tauri check / clippy / test run inside
    src-tauri\ (whose test binary cannot link on a Linux host at
    all), and the UI build + smoke - executes natively on Windows
    there instead of on a Linux VM.

    This script does NOT install the GitHub Actions runner agent
    itself. The runner's registration token is generated per runner
    on the repo's Settings -> Actions -> Runners -> New self-hosted
    runner page and expires one hour after the page is opened, so
    the agent is installed from that page, after this script has run:
    extract to C:\actions-runner, add the label "prometheus" when
    config asks for additional labels, and install it as a service.

    Idempotent: every step detects an existing install and skips it,
    so the script is safe to re-run after a failure or a reboot. The
    MSVC step in particular does not rely on winget: on 2026-10-19,
    on Prometheus, `winget install Microsoft.VisualStudio.2022.
    BuildTools` failed with exit code 1 because Build Tools was
    already installed and winget attempted an upgrade-and-modify.
    The script therefore detects the C++ tools with vswhere first
    and modifies the existing instance with setup.exe only when the
    component is missing; winget is the fallback for a machine with
    no Visual Studio instance at all.

    Requires an elevated (administrator) PowerShell session.
    Works on Windows PowerShell 5.1 and PowerShell 7.

.EXAMPLE
    .\tools\prometheus\Install-BuildToolchain.ps1
#>
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:VsWherePath = 'C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe'
$script:VsWorkload = 'Microsoft.VisualStudio.Workload.VCTools'
$script:VsComponent = 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64'
$script:VsWingetId = 'Microsoft.VisualStudio.2022.BuildTools'
$script:RustRoot = 'C:\Rust'
$script:RebootPending = $false
$script:StepResults = [System.Collections.Generic.List[string]]::new()

function Add-StepResult {
    param(
        [Parameter(Mandatory = $true)][string]$Text
    )
    [void]$script:StepResults.Add($Text)
    Write-Host "  $Text"
}

function Update-SessionPath {
    # Winget installs update the machine PATH; the running session
    # keeps its stale copy until it is refreshed from the registry.
    $machine = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $user = [Environment]::GetEnvironmentVariable('Path', 'User')
    $env:Path = "$machine;$user"
}

function Get-ToolPath {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [string[]]$FallbackPaths = @()
    )
    $cmd = Get-Command $Name -ErrorAction SilentlyContinue
    if ($null -ne $cmd) { return $cmd.Source }
    foreach ($candidate in $FallbackPaths) {
        if (Test-Path -LiteralPath $candidate) { return $candidate }
    }
    return $null
}

function Invoke-WingetInstall {
    param(
        [Parameter(Mandatory = $true)][string]$Id
    )
    $winget = Get-Command 'winget' -ErrorAction SilentlyContinue
    if ($null -eq $winget) {
        throw 'winget is not available on this machine. Install "App Installer" from the Microsoft Store, or install the missing tool manually, then re-run this script.'
    }
    & $winget.Source install --id $Id -e --accept-package-agreements --accept-source-agreements
    if ($LASTEXITCODE -ne 0) {
        throw "winget install of $Id failed with exit code $LASTEXITCODE."
    }
    Update-SessionPath
}

function Test-VsSetupExitCode {
    param(
        [Parameter(Mandatory = $true)][int]$Code,
        [Parameter(Mandatory = $true)][string]$Label
    )
    if ($Code -eq 0) { return $true }
    # 3010: success, reboot required. 1641: success, reboot initiated.
    if ($Code -eq 3010 -or $Code -eq 1641) {
        $script:RebootPending = $true
        Write-Host "  $Label finished with exit code $Code - success, reboot pending."
        return $true
    }
    return $false
}

function Get-VsInstancePath {
    # Installation path of any Visual Studio instance, or $null.
    if (-not (Test-Path -LiteralPath $script:VsWherePath)) { return $null }
    $paths = @(& $script:VsWherePath -products '*' -property installationPath)
    foreach ($p in $paths) {
        if (-not [string]::IsNullOrWhiteSpace($p)) { return $p.Trim() }
    }
    return $null
}

function Get-VcToolsPath {
    # Installation path of an instance that already carries the MSVC
    # tools component, or $null.
    if (-not (Test-Path -LiteralPath $script:VsWherePath)) { return $null }
    $paths = @(& $script:VsWherePath -products '*' -requires $script:VsComponent -property installationPath)
    foreach ($p in $paths) {
        if (-not [string]::IsNullOrWhiteSpace($p)) { return $p.Trim() }
    }
    return $null
}

function Install-Git {
    $fallbacks = @('C:\Program Files\Git\cmd\git.exe')
    $gitPath = Get-ToolPath -Name 'git' -FallbackPaths $fallbacks
    if ($null -eq $gitPath) {
        Invoke-WingetInstall -Id 'Git.Git'
        $gitPath = Get-ToolPath -Name 'git' -FallbackPaths $fallbacks
        if ($null -eq $gitPath) { throw 'Git still not found after the winget install.' }
        Add-StepResult 'Git: installed via winget'
    } else {
        Add-StepResult 'Git: already present'
    }
    & $gitPath config --system core.longpaths true
    if ($LASTEXITCODE -ne 0) {
        throw "git config --system core.longpaths failed with exit code $LASTEXITCODE."
    }
    Write-Host '  git config --system core.longpaths true (set)'
}

function Install-Node {
    $fallbacks = @('C:\Program Files\nodejs\node.exe')
    $nodePath = Get-ToolPath -Name 'node' -FallbackPaths $fallbacks
    if ($null -eq $nodePath) {
        Invoke-WingetInstall -Id 'OpenJS.NodeJS.LTS'
        $nodePath = Get-ToolPath -Name 'node' -FallbackPaths $fallbacks
        if ($null -eq $nodePath) { throw 'Node still not found after the winget install.' }
        Add-StepResult 'Node.js LTS: installed via winget'
    } else {
        Add-StepResult 'Node.js LTS: already present'
    }
}

function Install-VcTools {
    $vcPath = Get-VcToolsPath
    if ($null -ne $vcPath) {
        Add-StepResult "MSVC + Windows SDK: already present ($vcPath)"
        return
    }
    $instancePath = Get-VsInstancePath
    if ($null -ne $instancePath) {
        Write-Host '  A Visual Studio instance exists without the C++ tools; adding the VCTools workload to it...'
        $setupExe = Join-Path (Split-Path $script:VsWherePath) 'setup.exe'
        & $setupExe modify --installPath $instancePath --add $script:VsWorkload --includeRecommended --quiet --wait
        if (-not (Test-VsSetupExitCode -Code $LASTEXITCODE -Label 'setup.exe modify')) {
            throw "setup.exe modify failed with exit code $LASTEXITCODE. A Visual Studio Installer window left open, or a reboot already pending, can cause this; reboot and re-run this script."
        }
    } else {
        Write-Host '  No Visual Studio instance found; installing Build Tools 2022 with the VCTools workload via winget...'
        $winget = Get-Command 'winget' -ErrorAction SilentlyContinue
        if ($null -eq $winget) {
            throw 'winget is not available on this machine. Install "App Installer" from the Microsoft Store, or install Visual Studio 2022 Build Tools with the C++ workload manually, then re-run this script.'
        }
        & $winget.Source install --id $script:VsWingetId -e --accept-package-agreements --accept-source-agreements --override '--wait --quiet --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended'
        if (-not (Test-VsSetupExitCode -Code $LASTEXITCODE -Label 'Build Tools install')) {
            throw "The Build Tools winget install failed with exit code $LASTEXITCODE."
        }
    }
    $vcPath = Get-VcToolsPath
    if ($null -eq $vcPath) {
        throw 'The VCTools workload still does not detect after setup. Reboot and re-run this script.'
    }
    Add-StepResult "MSVC + Windows SDK: installed ($vcPath)"
}

function Enable-LongPaths {
    $key = 'HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem'
    $current = Get-ItemProperty -LiteralPath $key -Name 'LongPathsEnabled' -ErrorAction SilentlyContinue
    if ($null -ne $current -and $current.LongPathsEnabled -eq 1) {
        Add-StepResult 'Long paths: already enabled (LongPathsEnabled = 1)'
        return
    }
    New-ItemProperty -LiteralPath $key -Name 'LongPathsEnabled' -Value 1 -PropertyType DWord -Force | Out-Null
    Add-StepResult 'Long paths: enabled (LongPathsEnabled = 1)'
}

function Install-Rust {
    $rustupHome = Join-Path $script:RustRoot 'rustup'
    $cargoHome = Join-Path $script:RustRoot 'cargo'
    $binDir = Join-Path $cargoHome 'bin'
    $cargoExe = Join-Path $binDir 'cargo.exe'

    # Machine-wide homes, so a runner service account can reach the
    # toolchain; then the same values for this session.
    [Environment]::SetEnvironmentVariable('RUSTUP_HOME', $rustupHome, 'Machine')
    [Environment]::SetEnvironmentVariable('CARGO_HOME', $cargoHome, 'Machine')
    $env:RUSTUP_HOME = $rustupHome
    $env:CARGO_HOME = $cargoHome

    if (Test-Path -LiteralPath $cargoExe) {
        Add-StepResult 'Rust: already present under C:\Rust (machine RUSTUP_HOME / CARGO_HOME re-asserted)'
    } else {
        $installer = Join-Path $env:TEMP 'rustup-init.exe'
        Write-Host '  Downloading rustup-init from https://win.rustup.rs/x86_64 ...'
        Invoke-WebRequest -Uri 'https://win.rustup.rs/x86_64' -OutFile $installer -UseBasicParsing
        & $installer -y --default-toolchain stable-x86_64-pc-windows-msvc
        if ($LASTEXITCODE -ne 0) {
            throw "rustup-init failed with exit code $LASTEXITCODE."
        }
        if (-not (Test-Path -LiteralPath $cargoExe)) {
            throw "rustup-init completed but $cargoExe was not found."
        }
        Add-StepResult 'Rust: installed stable-x86_64-pc-windows-msvc under C:\Rust'
    }

    $machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    if (($machinePath -split ';') -notcontains $binDir) {
        [Environment]::SetEnvironmentVariable('Path', ($machinePath.TrimEnd(';') + ';' + $binDir), 'Machine')
        Write-Host "  Added $binDir to the machine PATH."
    }
    if (($env:Path -split ';') -notcontains $binDir) {
        $env:Path = $env:Path + ';' + $binDir
    }
}

# ---------------------------------------------------------------- main

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltinRole]::Administrator)) {
    Write-Host 'This script must run elevated. Open PowerShell with "Run as administrator" and re-run it.'
    exit 1
}

Write-Host '== Git =='
Install-Git

Write-Host '== Node.js (LTS) =='
Install-Node

Write-Host '== Visual Studio Build Tools (MSVC + Windows SDK) =='
Install-VcTools

Write-Host '== Long paths =='
Enable-LongPaths

Write-Host '== Rust (machine-wide) =='
Install-Rust

Write-Host '== Verification =='
Update-SessionPath
$cargoBin = Join-Path $env:CARGO_HOME 'bin'
if (($env:Path -split ';') -notcontains $cargoBin) {
    $env:Path = $env:Path + ';' + $cargoBin
}
$allVerified = $true
foreach ($tool in @('git', 'node', 'rustc', 'cargo')) {
    $cmd = Get-Command $tool -ErrorAction SilentlyContinue
    if ($null -ne $cmd) {
        $versionLine = & $cmd.Source '--version' 2>&1 | Select-Object -First 1
        Write-Host ("  {0}: {1}" -f $tool, $versionLine)
    } else {
        Write-Host ("  {0}: NOT FOUND" -f $tool)
        $allVerified = $false
    }
}

Write-Host '== Summary =='
foreach ($result in $script:StepResults) {
    Write-Host "  $result"
}
if ($script:RebootPending) {
    Write-Host '  REBOOT PENDING: a setup step returned 3010. Reboot before installing or starting the GitHub Actions runner service, so the service picks up the new machine PATH and environment.'
}
if (-not $allVerified) {
    Write-Host '  One or more tools did not verify above. Resolve the errors and re-run this script; it is idempotent.'
    exit 1
}
Write-Host '  Toolchain ready. Next: install the GitHub Actions runner agent from the repo page Settings -> Actions -> Runners -> New self-hosted runner (the token on that page expires one hour after it is opened): extract to C:\actions-runner, add the label "prometheus" when config asks for additional labels, and install it as a service.'
