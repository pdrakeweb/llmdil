<#
.SYNOPSIS
    Baseline benchmark for the LLMdil planner and coder models.

.DESCRIPTION
    Sends a fixed set of prompts to an OpenAI-compatible endpoint
    (llama-server directly, or the LiteLLM gateway) and records the
    timings the server reports: prompt tokens, prompt tok/s, predicted
    tokens, generation tok/s, and - when speculative decoding is active -
    drafted / accepted token counts and the acceptance rate.

    Measurement only. Model output is never executed. The script writes
    a per-run CSV and a JSON summary (medians per target + prompt) into
    the output directory. It changes nothing on the machine except
    those result files.

    Defaults target the LiteLLM gateway (http://127.0.0.1:4000/v1) with
    the gateway group names "planner" and "coder". To measure a backend
    directly, as the 2026-10-17 baseline was measured, pass that
    backend's own base URL in -BaseUrl; the model name the backend
    serves is then whatever -PlannerModel / -CoderModel say (for a
    direct llama-server the model field is usually ignored, but it is
    sent as given).

.PARAMETER Target
    Planner, Coder, or Both (default Both).

.PARAMETER Label
    Free-text arm label written into every result row and the summary,
    for example "base-file", "mtp-nmax2", "coder-baseline".

.PARAMETER Deterministic
    Sets temperature 0 and seed 42 for every request (the parity pass).
    Without this switch no temperature or seed is sent at all, so the
    server uses its production sampling settings.

.NOTES
    Works on Windows PowerShell 5.1 and PowerShell 7. No admin rights.
    Coder thermal guard: the Tesla P100 previously hit 81 C idle and
    fell off the bus (maximum 85 C). When the Coder target runs, this
    script reads the P100 temperature from nvidia-smi before the
    warm-up, before every measured run, and after every run. If it
    reaches -AbortTempC (default 80) the coder target is aborted
    before the next run starts; results gathered so far are still
    written, and the script exits with code 2.
#>
[CmdletBinding()]
param(
    [ValidateSet('Planner', 'Coder', 'Both')]
    [string]$Target = 'Both',

    [string]$BaseUrl = 'http://127.0.0.1:4000/v1',

    [string]$PlannerModel = 'planner',
    [string]$CoderModel = 'coder',

    [int]$Runs = 3,
    [int]$MaxTokens = 512,
    [int]$TimeoutSec = 1800,

    [switch]$Deterministic,

    [string]$Label = 'baseline',

    [string]$OutDir = './baseline-results',

    [string]$PromptDir = (Join-Path $PSScriptRoot 'prompts'),

    [int]$AbortTempC = 80,
    [switch]$NoThermalGuard,
    [switch]$SkipPreflight
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-Median {
    param([double[]]$Values)
    if ($null -eq $Values -or $Values.Count -eq 0) { return $null }
    $sorted = @($Values | Sort-Object)
    $n = $sorted.Count
    if ($n % 2 -eq 1) {
        return [double]$sorted[[int](($n - 1) / 2)]
    }
    return [double](($sorted[$n / 2 - 1] + $sorted[$n / 2]) / 2.0)
}

function Get-PropertyOrNull {
    param($Object, [string]$Name)
    if ($null -eq $Object) { return $null }
    if ($Object.PSObject.Properties.Name -contains $Name) {
        return $Object.$Name
    }
    return $null
}

function Get-P100Temperature {
    # Returns the highest temperature (C) among GPUs whose name contains
    # "P100", or $null when nvidia-smi or a P100 cannot be found.
    $smi = Get-Command 'nvidia-smi' -ErrorAction SilentlyContinue
    if ($null -eq $smi) { return $null }
    try {
        $lines = & $smi.Source '--query-gpu=name,temperature.gpu' '--format=csv,noheader,nounits' 2>$null
    } catch {
        return $null
    }
    $max = $null
    foreach ($line in $lines) {
        if ([string]::IsNullOrWhiteSpace($line)) { continue }
        if ($line -notmatch 'P100') { continue }
        $parts = $line -split ','
        $tempText = $parts[$parts.Count - 1].Trim()
        $temp = 0
        if ([int]::TryParse($tempText, [ref]$temp)) {
            if ($null -eq $max -or $temp -gt $max) { $max = $temp }
        }
    }
    return $max
}

function Test-CoderTemperature {
    # Returns $true when it is safe to continue, $false when the coder
    # target must abort. Only meaningful for the Coder target.
    param([string]$Phase)
    if ($NoThermalGuard) { return $true }
    $temp = Get-P100Temperature
    if ($null -eq $temp) {
        Write-Warning "Thermal guard: no P100 temperature available from nvidia-smi ($Phase); continuing without a reading."
        return $true
    }
    Write-Host ("  P100 temperature ({0}): {1} C (abort at {2} C)" -f $Phase, $temp, $AbortTempC)
    if ($temp -ge $AbortTempC) {
        Write-Warning ("ABORT: P100 at {0} C reached the {1} C limit during phase '{2}'. Coder target stopped; no further coder runs will start." -f $temp, $AbortTempC, $Phase)
        return $false
    }
    return $true
}

function Invoke-ChatCompletion {
    param(
        [string]$Model,
        [string]$PromptText
    )
    $body = [ordered]@{
        model       = $Model
        messages    = @(@{ role = 'user'; content = $PromptText })
        max_tokens  = $MaxTokens
        stream      = $false
    }
    if ($Deterministic) {
        $body['temperature'] = 0
        $body['seed'] = 42
    }
    $json = $body | ConvertTo-Json -Depth 6
    $url = $BaseUrl.TrimEnd('/') + '/chat/completions'
    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    $response = Invoke-RestMethod -Uri $url -Method Post -ContentType 'application/json' -Body $json -TimeoutSec $TimeoutSec
    $watch.Stop()
    return @{ Response = $response; ElapsedSec = $watch.Elapsed.TotalSeconds }
}

function ConvertTo-RunRecord {
    param(
        [string]$TargetName,
        [string]$ModelName,
        [string]$PromptName,
        [int]$RunIndex,
        $Call
    )
    $response = $Call.Response
    $elapsed = [double]$Call.ElapsedSec
    $timings = Get-PropertyOrNull $response 'timings'
    $usage = Get-PropertyOrNull $response 'usage'

    $promptN = Get-PropertyOrNull $timings 'prompt_n'
    $promptTps = Get-PropertyOrNull $timings 'prompt_per_second'
    $predictedN = Get-PropertyOrNull $timings 'predicted_n'
    $predictedTps = Get-PropertyOrNull $timings 'predicted_per_second'
    $draftN = Get-PropertyOrNull $timings 'draft_n'
    $draftAccepted = Get-PropertyOrNull $timings 'draft_n_accepted'

    $source = 'timings'
    if ($null -eq $timings) {
        # Wall-clock fallback. Prompt speed cannot be separated from
        # generation with a single round trip, so prompt tok/s stays
        # empty and generation tok/s is a whole-request average.
        $source = 'wallclock-total'
        if ($null -ne $usage) {
            $promptN = Get-PropertyOrNull $usage 'prompt_tokens'
            $predictedN = Get-PropertyOrNull $usage 'completion_tokens'
        }
        if ($null -ne $predictedN -and $elapsed -gt 0) {
            $predictedTps = [double]$predictedN / $elapsed
        }
    }

    $acceptance = $null
    if ($null -ne $draftN -and $null -ne $draftAccepted -and [double]$draftN -gt 0) {
        $acceptance = [double]$draftAccepted / [double]$draftN
    }

    return [PSCustomObject][ordered]@{
        Timestamp          = (Get-Date).ToString('o')
        Label              = $Label
        Target             = $TargetName
        Model              = $ModelName
        Prompt             = $PromptName
        Run                = $RunIndex
        Deterministic      = [bool]$Deterministic
        PromptTokens       = $promptN
        PromptTokPerSec    = $promptTps
        PredictedTokens    = $predictedN
        GenerationTokPerSec = $predictedTps
        DraftN             = $draftN
        DraftNAccepted     = $draftAccepted
        AcceptanceRate     = $acceptance
        TimingsSource      = $source
        WallClockSec       = [math]::Round($elapsed, 3)
        Status             = 'ok'
        Error              = ''
    }
}

# ---------------------------------------------------------------- main

$targetsToRun = @()
if ($Target -eq 'Planner' -or $Target -eq 'Both') {
    $targetsToRun += @{ Name = 'Planner'; Model = $PlannerModel; Prefix = 'planner-' }
}
if ($Target -eq 'Coder' -or $Target -eq 'Both') {
    $targetsToRun += @{ Name = 'Coder'; Model = $CoderModel; Prefix = 'coder-' }
}

foreach ($t in $targetsToRun) {
    $promptFiles = @(Get-ChildItem -LiteralPath $PromptDir -Filter ($t.Prefix + '*.txt') -ErrorAction SilentlyContinue | Sort-Object Name)
    if ($promptFiles.Count -eq 0) {
        throw "No prompt files found for target '$($t.Name)' (pattern '$($t.Prefix)*.txt' in '$PromptDir')."
    }
    $t['Prompts'] = $promptFiles
}

if (-not $SkipPreflight) {
    $modelsUrl = $BaseUrl.TrimEnd('/') + '/models'
    try {
        $null = Invoke-RestMethod -Uri $modelsUrl -Method Get -TimeoutSec 15
    } catch {
        throw "Endpoint unreachable: GET $modelsUrl failed ($($_.Exception.Message)). The gateway (or the direct backend named by -BaseUrl) must be running before benchmarking. Pass -SkipPreflight if the endpoint does not implement /models."
    }
}

if (-not (Test-Path -LiteralPath $OutDir)) {
    $null = New-Item -ItemType Directory -Path $OutDir -Force
}
$OutDir = (Resolve-Path -LiteralPath $OutDir).Path

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$safeLabel = ($Label -replace '[^\w\-]', '_')
$csvPath = Join-Path $OutDir ("baseline-{0}-{1}.csv" -f $safeLabel, $stamp)
$jsonPath = Join-Path $OutDir ("baseline-{0}-{1}.summary.json" -f $safeLabel, $stamp)

$records = @()
$coderAborted = $false

foreach ($t in $targetsToRun) {
    $targetName = $t.Name
    $model = $t.Model
    Write-Host ("== Target {0} (model '{1}') via {2} ==" -f $targetName, $model, $BaseUrl)

    if ($targetName -eq 'Coder') {
        if (-not (Test-CoderTemperature -Phase 'before warm-up')) { $coderAborted = $true; continue }
    }

    # One warm-up request per target, discarded. Uses the first prompt
    # with a small token cap so model load and first-touch costs do not
    # land in the measured runs.
    $warmPrompt = Get-Content -LiteralPath $t.Prompts[0].FullName -Raw
    $savedMaxTokens = $MaxTokens
    $MaxTokens = 32
    try {
        $null = Invoke-ChatCompletion -Model $model -PromptText $warmPrompt
    } catch {
        throw "Warm-up request for target '$targetName' failed: $($_.Exception.Message). Check that the backend serving model '$model' is running at $BaseUrl."
    } finally {
        $MaxTokens = $savedMaxTokens
    }

    foreach ($promptFile in $t.Prompts) {
        if ($coderAborted) { break }
        $promptText = Get-Content -LiteralPath $promptFile.FullName -Raw
        $promptName = [System.IO.Path]::GetFileNameWithoutExtension($promptFile.Name)
        for ($run = 1; $run -le $Runs; $run++) {
            if ($targetName -eq 'Coder') {
                if (-not (Test-CoderTemperature -Phase ("before {0} run {1}" -f $promptName, $run))) {
                    $coderAborted = $true
                    break
                }
            }
            Write-Host ("  {0} run {1}/{2} ..." -f $promptName, $run, $Runs)
            try {
                $call = Invoke-ChatCompletion -Model $model -PromptText $promptText
                $record = ConvertTo-RunRecord -TargetName $targetName -ModelName $model -PromptName $promptName -RunIndex $run -Call $call
                $records += $record
                Write-Host ("    generation {0} tok/s (prompt {1} tok/s, {2} tokens, source {3})" -f $record.GenerationTokPerSec, $record.PromptTokPerSec, $record.PredictedTokens, $record.TimingsSource)
            } catch {
                Write-Warning ("Run failed ({0} run {1}): {2}" -f $promptName, $run, $_.Exception.Message)
                $records += [PSCustomObject][ordered]@{
                    Timestamp = (Get-Date).ToString('o'); Label = $Label; Target = $targetName
                    Model = $model; Prompt = $promptName; Run = $run
                    Deterministic = [bool]$Deterministic; PromptTokens = $null; PromptTokPerSec = $null
                    PredictedTokens = $null; GenerationTokPerSec = $null; DraftN = $null
                    DraftNAccepted = $null; AcceptanceRate = $null; TimingsSource = ''
                    WallClockSec = $null; Status = 'error'; Error = $_.Exception.Message
                }
            }
            if ($targetName -eq 'Coder' -and -not $coderAborted) {
                if (-not (Test-CoderTemperature -Phase ("after {0} run {1}" -f $promptName, $run))) {
                    $coderAborted = $true
                    break
                }
            }
        }
    }
}

# ------------------------------------------------------------- results

if ($records.Count -gt 0) {
    $records | Export-Csv -LiteralPath $csvPath -NoTypeInformation -Encoding UTF8
    Write-Host ("Per-run CSV: {0}" -f $csvPath)
} else {
    Write-Warning 'No runs were recorded; no CSV written.'
}

$medianRows = @()
$okRecords = @($records | Where-Object { $_.Status -eq 'ok' })
$groups = @($okRecords | Group-Object -Property Target, Prompt)
foreach ($group in $groups) {
    $items = $group.Group
    $genValues = @($items | ForEach-Object { if ($null -ne $_.GenerationTokPerSec) { [double]$_.GenerationTokPerSec } })
    $promptValues = @($items | ForEach-Object { if ($null -ne $_.PromptTokPerSec) { [double]$_.PromptTokPerSec } })
    $promptTokenValues = @($items | ForEach-Object { if ($null -ne $_.PromptTokens) { [double]$_.PromptTokens } })
    $predictedTokenValues = @($items | ForEach-Object { if ($null -ne $_.PredictedTokens) { [double]$_.PredictedTokens } })
    $acceptValues = @($items | ForEach-Object { if ($null -ne $_.AcceptanceRate) { [double]$_.AcceptanceRate } })
    $first = $items[0]
    $medianRows += [PSCustomObject][ordered]@{
        Target                       = $first.Target
        Prompt                       = $first.Prompt
        RunsOk                       = $items.Count
        MedianPromptTokens           = Get-Median $promptTokenValues
        MedianPromptTokPerSec        = Get-Median $promptValues
        MedianPredictedTokens        = Get-Median $predictedTokenValues
        MedianGenerationTokPerSec    = Get-Median $genValues
        MedianAcceptanceRate         = Get-Median $acceptValues
    }
    Write-Host ("Median {0}/{1}: generation {2} tok/s over {3} run(s)" -f $first.Target, $first.Prompt, (Get-Median $genValues), $items.Count)
}

$summary = [ordered]@{
    Metadata = [ordered]@{
        TimestampUtc  = (Get-Date).ToUniversalTime().ToString('o')
        Label         = $Label
        BaseUrl       = $BaseUrl
        Target        = $Target
        PlannerModel  = $PlannerModel
        CoderModel    = $CoderModel
        Runs          = $Runs
        MaxTokens     = $MaxTokens
        Deterministic = [bool]$Deterministic
        Sampling      = $(if ($Deterministic) { 'temperature 0, seed 42' } else { 'production (no temperature or seed sent)' })
        CoderAborted  = [bool]$coderAborted
        AbortTempC    = $AbortTempC
        Machine       = $env:COMPUTERNAME
        PowerShell    = $PSVersionTable.PSVersion.ToString()
    }
    Medians  = $medianRows
    RunsFile = [System.IO.Path]::GetFileName($csvPath)
}
$summary | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $jsonPath -Encoding UTF8
Write-Host ("Summary JSON: {0}" -f $jsonPath)

if ($coderAborted) {
    Write-Warning 'Coder target was aborted by the thermal guard. Partial results were written.'
    exit 2
}
