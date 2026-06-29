<#
.SYNOPSIS
    Sample FeatherDock's resource use so performance regressions are catchable before a
    release, in a repeatable format.

.DESCRIPTION
    Takes two CPU snapshots `-Seconds` apart for every running FeatherDock process (the
    dock + its watchdog) and reports, per process:
        CPU%        average single-core-equivalent CPU over the window
        WS(MB)      working set
        Private(MB) private bytes
        Handles     open handle count
        Threads     thread count

    The script samples whatever state the dock is in right now, so drive the scenario
    yourself and label the run. README claims ~0% idle CPU; this is how you check it.

.PARAMETER Label
    A name for the scenario being sampled (idle / hover / drawer / preview).

.PARAMETER Seconds
    Length of the CPU sampling window. Longer = steadier numbers.

.EXAMPLE
    # Leave the dock untouched:
    .\scripts\perf-smoke.ps1 -Label idle

.EXAMPLE
    # Rest the mouse on the dock, then (without moving it) run:
    .\scripts\perf-smoke.ps1 -Label hover -Seconds 8

.EXAMPLE
    # Open the app drawer, then run:
    .\scripts\perf-smoke.ps1 -Label drawer
#>
[CmdletBinding()]
param(
    [string]$Label = 'idle',
    [int]$Seconds = 5
)

$procs = Get-Process -Name 'FeatherDock', 'featherdock' -ErrorAction SilentlyContinue
if (-not $procs) {
    Write-Host 'FeatherDock is not running; start it first.' -ForegroundColor Yellow
    exit 1
}

$cores = [Environment]::ProcessorCount
$startCpu = @{}
foreach ($p in $procs) { $startCpu[$p.Id] = $p.CPU }

Write-Host "Sampling '$Label' for $Seconds s across $cores cores..." -ForegroundColor Cyan
Start-Sleep -Seconds $Seconds

$rows = foreach ($p in (Get-Process -Id ($procs.Id) -ErrorAction SilentlyContinue)) {
    $cpu0 = $startCpu[$p.Id]
    $delta = if ($null -ne $cpu0) { $p.CPU - $cpu0 } else { 0 }
    $pct = if ($Seconds -gt 0) { [math]::Round(($delta / $Seconds / $cores) * 100, 2) } else { 0 }
    [pscustomobject]@{
        Scenario      = $Label
        ProcessId     = $p.Id
        Name          = $p.ProcessName
        'CPU%'        = $pct
        'WS(MB)'      = [math]::Round($p.WorkingSet64 / 1MB, 2)
        'Private(MB)' = [math]::Round($p.PrivateMemorySize64 / 1MB, 2)
        Handles       = $p.HandleCount
        Threads       = $p.Threads.Count
    }
}

$rows | Format-Table -AutoSize
