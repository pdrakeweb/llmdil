<#
.SYNOPSIS
    Read-only system snapshot for the Prometheus workstation.

.DESCRIPTION
    Captures the facts the Phase 0 baseline needs, in one run:

    - llama-server version (from -LlamaServerPath, from PATH, or by
      searching the usual LLMdil data locations for llama-server.exe)
    - Per-GPU name, VRAM used / free, and temperature (nvidia-smi)
    - DIMM inventory: locator, capacity, manufacturer, part number,
      speed, configured clock (Get-CimInstance Win32_PhysicalMemory)
    - Total physical RAM
    - Commit charge: committed bytes and commit limit
    - Pagefile: location, allocated size, current and peak usage

    Output goes to the console. With -OutFile, the same data is also
    written as JSON. The script is read-only and needs no admin rights.

    Works on Windows PowerShell 5.1 and PowerShell 7.
#>
[CmdletBinding()]
param(
    [string]$OutFile,
    [string]$LlamaServerPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Find-LlamaServer {
    if ($LlamaServerPath) {
        if (Test-Path -LiteralPath $LlamaServerPath) { return $LlamaServerPath }
        Write-Warning "LlamaServerPath '$LlamaServerPath' does not exist."
        return $null
    }
    $onPath = Get-Command 'llama-server' -ErrorAction SilentlyContinue
    if ($null -ne $onPath) { return $onPath.Source }

    $roots = @()
    if ($env:LOCALAPPDATA) {
        $roots += (Join-Path $env:LOCALAPPDATA 'LLMdil')
        $roots += (Join-Path $env:LOCALAPPDATA 'local-llm-service-manager')
        $roots += (Join-Path $env:LOCALAPPDATA 'Local LLM Service Manager')
    }
    if ($env:APPDATA) {
        $roots += (Join-Path $env:APPDATA 'LLMdil')
        $roots += (Join-Path $env:APPDATA 'local-llm-service-manager')
    }
    if ($env:ProgramData) {
        $roots += (Join-Path $env:ProgramData 'LLMdil')
    }
    foreach ($root in $roots) {
        if (-not (Test-Path -LiteralPath $root)) { continue }
        $found = Get-ChildItem -LiteralPath $root -Recurse -Depth 4 -Filter 'llama-server.exe' -ErrorAction SilentlyContinue |
            Select-Object -First 1
        if ($null -ne $found) { return $found.FullName }
    }
    return $null
}

function Get-LlamaServerVersion {
    $path = Find-LlamaServer
    if ($null -eq $path) {
        return @{ Path = $null; Version = $null; Note = 'llama-server not found on PATH or in the searched data locations. Pass -LlamaServerPath.' }
    }
    $version = $null
    try {
        $output = & $path '--version' 2>&1
        if ($output) { $version = ($output | Select-Object -First 1).ToString().Trim() }
    } catch {
        $version = "found but --version failed: $($_.Exception.Message)"
    }
    return @{ Path = $path; Version = $version; Note = '' }
}

function Get-GpuInfo {
    $smi = Get-Command 'nvidia-smi' -ErrorAction SilentlyContinue
    if ($null -eq $smi) { return @() }
    $lines = & $smi.Source '--query-gpu=index,name,memory.used,memory.free,temperature.gpu' '--format=csv,noheader,nounits' 2>$null
    $gpus = @()
    foreach ($line in $lines) {
        if ([string]::IsNullOrWhiteSpace($line)) { continue }
        $parts = $line -split ','
        if ($parts.Count -lt 5) { continue }
        $gpus += [PSCustomObject][ordered]@{
            Index         = $parts[0].Trim()
            Name          = ($parts[1..($parts.Count - 4)] -join ',').Trim()
            MemoryUsedMiB = $parts[$parts.Count - 3].Trim()
            MemoryFreeMiB = $parts[$parts.Count - 2].Trim()
            TemperatureC  = $parts[$parts.Count - 1].Trim()
        }
    }
    return $gpus
}

function Get-DimmInfo {
    $dimms = @()
    $modules = Get-CimInstance Win32_PhysicalMemory
    foreach ($m in $modules) {
        $dimms += [PSCustomObject][ordered]@{
            DeviceLocator        = $m.DeviceLocator
            CapacityGB           = [math]::Round($m.Capacity / 1GB, 2)
            Manufacturer         = "$($m.Manufacturer)".Trim()
            PartNumber           = "$($m.PartNumber)".Trim()
            SpeedMTs             = $m.Speed
            ConfiguredClockSpeed = $m.ConfiguredClockSpeed
        }
    }
    return $dimms
}

function Get-CommitInfo {
    try {
        $counters = Get-Counter -Counter '\Memory\Committed Bytes', '\Memory\Commit Limit' -ErrorAction Stop
        $committed = $null
        $limit = $null
        foreach ($sample in $counters.CounterSamples) {
            if ($sample.Path -match 'committed bytes') { $committed = [int64]$sample.CookedValue }
            if ($sample.Path -match 'commit limit') { $limit = [int64]$sample.CookedValue }
        }
        return [PSCustomObject][ordered]@{
            CommittedBytes = $committed
            CommitLimitBytes = $limit
            CommittedGB    = if ($null -ne $committed) { [math]::Round($committed / 1GB, 2) } else { $null }
            CommitLimitGB  = if ($null -ne $limit) { [math]::Round($limit / 1GB, 2) } else { $null }
        }
    } catch {
        Write-Warning "Commit counters unavailable: $($_.Exception.Message)"
        return $null
    }
}

function Get-PagefileInfo {
    $files = @()
    $usage = Get-CimInstance Win32_PageFileUsage -ErrorAction SilentlyContinue
    foreach ($p in $usage) {
        $files += [PSCustomObject][ordered]@{
            Name              = $p.Name
            AllocatedMB       = $p.AllocatedBaseSize
            CurrentUsageMB    = $p.CurrentUsage
            PeakUsageMB       = $p.PeakUsage
        }
    }
    return $files
}

# ---------------------------------------------------------------- main

$llama = Get-LlamaServerVersion
$gpus = @(Get-GpuInfo)
$dimms = @(Get-DimmInfo)
$totalRamBytes = (Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory
$commit = Get-CommitInfo
$pagefiles = @(Get-PagefileInfo)

Write-Host '== llama-server =='
if ($llama.Path) {
    Write-Host ("  Path:    {0}" -f $llama.Path)
    Write-Host ("  Version: {0}" -f $llama.Version)
} else {
    Write-Host ("  Not found. {0}" -f $llama.Note)
}

Write-Host '== GPUs (nvidia-smi) =='
if ($gpus.Count -eq 0) {
    Write-Host '  nvidia-smi not found or no GPUs reported.'
} else {
    foreach ($g in $gpus) {
        Write-Host ("  [{0}] {1} - used {2} MiB, free {3} MiB, {4} C" -f $g.Index, $g.Name, $g.MemoryUsedMiB, $g.MemoryFreeMiB, $g.TemperatureC)
    }
}

Write-Host '== Memory =='
Write-Host ("  Total physical RAM: {0} GB across {1} DIMM(s)" -f [math]::Round($totalRamBytes / 1GB, 2), $dimms.Count)
foreach ($d in $dimms) {
    Write-Host ("  {0}: {1} GB, {2}, part {3}, {4} MT/s (configured {5})" -f $d.DeviceLocator, $d.CapacityGB, $d.Manufacturer, $d.PartNumber, $d.SpeedMTs, $d.ConfiguredClockSpeed)
}
if ($null -ne $commit) {
    Write-Host ("  Commit: {0} GB of {1} GB limit" -f $commit.CommittedGB, $commit.CommitLimitGB)
}

Write-Host '== Pagefile =='
if ($pagefiles.Count -eq 0) {
    Write-Host '  No pagefile reported by Win32_PageFileUsage.'
} else {
    foreach ($p in $pagefiles) {
        Write-Host ("  {0}: allocated {1} MB, current {2} MB, peak {3} MB" -f $p.Name, $p.AllocatedMB, $p.CurrentUsageMB, $p.PeakUsageMB)
    }
}

$snapshot = [ordered]@{
    TimestampUtc   = (Get-Date).ToUniversalTime().ToString('o')
    Machine        = $env:COMPUTERNAME
    PowerShell     = $PSVersionTable.PSVersion.ToString()
    LlamaServer    = [PSCustomObject][ordered]@{ Path = $llama.Path; Version = $llama.Version; Note = $llama.Note }
    Gpus           = $gpus
    TotalRamGB     = [math]::Round($totalRamBytes / 1GB, 2)
    Dimms          = $dimms
    Commit         = $commit
    Pagefiles      = $pagefiles
}

if ($OutFile) {
    $snapshot | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $OutFile -Encoding UTF8
    Write-Host ("Snapshot JSON: {0}" -f $OutFile)
}
