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
    [string]$SessionsDir = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$baseDir = if ($SessionsDir) {
    $SessionsDir
} else {
    $root = if ($env:LOCALAPPDATA) { $env:LOCALAPPDATA } elseif ($env:APPDATA) { $env:APPDATA } else { "." }
    Join-Path $root "FeatherDock\agent-sessions"
}

$safeName = ($Agent + "_" + $Session) -replace '[^a-zA-Z0-9._-]', '_'
if (-not $safeName) { $safeName = "session" }
$targetPath = Join-Path $baseDir ($safeName + ".json")

if ($State -eq "closed") {
    # Remove session file cleanly; active events can also push `"state": "closed"` via CLI.
    if (Test-Path -LiteralPath $targetPath) {
        Remove-Item -LiteralPath $targetPath -Force -ErrorAction SilentlyContinue
    }
    exit 0
}

$null = New-Item -ItemType Directory -Path $baseDir -Force

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
$tmpPath = "$targetPath.tmp.$PID.$([Guid]::NewGuid().ToString('N').Substring(0, 8))"
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
[System.IO.File]::WriteAllText($tmpPath, $json, $utf8NoBom)
Move-Item -LiteralPath $tmpPath -Destination $targetPath -Force
