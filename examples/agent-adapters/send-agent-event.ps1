[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Agent,

    [Parameter(Mandatory = $true)]
    [string]$Session,

    [Parameter(Mandatory = $true)]
    [ValidateSet("running", "waiting", "done", "error", "idle", "closed")]
    [string]$State,

    [string]$Title = "",
    [string]$Detail = "",
    [string]$Cwd = "",
    [string]$Instance = "",
    [uint64]$Seq = 0,
    [uint32]$SessionPid = 0,
    [int64]$Hwnd = 0,
    [string]$DeepLink = "",
    [string]$DockExe = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Resolve-DockExe([string]$ExplicitPath) {
    if ($ExplicitPath -and (Test-Path -LiteralPath $ExplicitPath)) {
        return (Resolve-Path -LiteralPath $ExplicitPath).Path
    }
    $candidates = @(
        (Join-Path $PSScriptRoot "..\..\FeatherDock.exe"),
        (Join-Path $PSScriptRoot "..\..\outputs\pending\FeatherDock.exe"),
        (Join-Path $PSScriptRoot "..\..\target\x86_64-pc-windows-gnu\release\featherdock.exe"),
        (Join-Path $PSScriptRoot "..\..\target\release\featherdock.exe"),
        (Join-Path $env:LOCALAPPDATA "Programs\FeatherDock\FeatherDock.exe"),
        (Join-Path $env:LOCALAPPDATA "FeatherDock\FeatherDock.exe")
    )
    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate) {
            return (Resolve-Path -LiteralPath $candidate).Path
        }
    }
    $running = @(Get-Process -Name "FeatherDock" -ErrorAction SilentlyContinue) |
        ForEach-Object { try { $_.Path } catch {} } |
        Where-Object { $_ -and (Test-Path -LiteralPath $_) } |
        Select-Object -First 1
    if ($running) { return $running }
    foreach ($cmdName in @("FeatherDock.exe", "featherdock.exe")) {
        $cmd = Get-Command $cmdName -ErrorAction SilentlyContinue
        if ($cmd) { return $cmd.Source }
    }
    throw "Could not locate FeatherDock.exe. Pass -DockExe explicitly."
}

function Quote-WinArg([string]$Arg) {
    if ($null -eq $Arg -or $Arg -eq "") { return '""' }
    if ($Arg -notmatch '[\s"\\]') { return $Arg }
    $escaped = $Arg -replace '(\\*)"', '$1$1\"'
    $escaped = $escaped -replace '(\\+)$', '$1$1'
    return '"' + $escaped + '"'
}

$exe = Resolve-DockExe $DockExe
$now = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
$payload = [ordered]@{
    agent      = $Agent
    session_id = $Session
    state      = $State
    updated_at = $now
}
if ($SessionPid -gt 0) { $payload["pid"] = [uint32]$SessionPid }
if ($Title)    { $payload["title"] = $Title }
if ($Detail)   { $payload["detail"] = $Detail }
if ($Cwd)      { $payload["cwd"] = $Cwd }
if ($Instance) { $payload["instance"] = $Instance }
if ($Seq -gt 0) { $payload["seq"] = [uint64]$Seq }
if ($Hwnd -ne 0) { $payload["hwnd"] = [int64]$Hwnd }
if ($DeepLink) { $payload["deep_link"] = $DeepLink }

$json = $payload | ConvertTo-Json -Compress -Depth 4
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $exe
$psi.Arguments = "--agent-event --json " + (Quote-WinArg $json)
$psi.UseShellExecute = $false
$psi.CreateNoWindow = $true
$proc = [System.Diagnostics.Process]::Start($psi)
$proc.WaitForExit()
$code = $proc.ExitCode

switch ($code) {
    0 { exit 0 }
    2 { throw "FeatherDock rejected invalid agent-event arguments/JSON (exit code 2)." }
    3 {
        Write-Warning "FeatherDock is not running (exit code 3)."
        exit 3
    }
    4 { throw "FeatherDock WM_COPYDATA delivery failed or payload was rejected (exit code 4)." }
    default { exit $code }
}
