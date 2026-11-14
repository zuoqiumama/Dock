<#
.SYNOPSIS
Example Track B adapter for Codex CLI / GUI notify hooks -> FeatherDock.

.DESCRIPTION
Reads a Codex hook JSON payload (passed as $args[0] or piped via stdin),
assigns a monotonic sequence number per instance, pushes live status via
`send-agent-event.ps1`, and updates the session file when Dock is not running.
#>
[CmdletBinding()]
param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$HookArgs
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$rawJson = if ($HookArgs -and $HookArgs.Count -gt 0) {
    $HookArgs -join " "
} elseif (-not [Console]::IsInputRedirected) {
    ""
} else {
    [Console]::In.ReadToEnd()
}

if ([string]::IsNullOrWhiteSpace($rawJson)) {
    exit 0
}

function Get-JsonProp([object]$Obj, [string]$Name, $Default = $null) {
    if ($null -eq $Obj) { return $Default }
    $prop = $Obj.PSObject.Properties[$Name]
    if ($null -ne $prop -and $null -ne $prop.Value -and "$($prop.Value)" -ne "") {
        return $prop.Value
    }
    return $Default
}

$evt = $rawJson | ConvertFrom-Json
$eventType = [string](Get-JsonProp $evt "type" "")
$threadId = [string](Get-JsonProp $evt "thread-id" (Get-JsonProp $evt "session_id" "codex-default"))
$cwd = [string](Get-JsonProp $evt "cwd" "")

$state = switch -Wildcard ($eventType.ToLowerInvariant()) {
    "*complete*" { "done" }
    "*done*"     { "done" }
    "*wait*"     { "waiting" }
    "*approval*" { "waiting" }
    "*error*"    { "error" }
    "*fail*"     { "error" }
    "*close*"    { "closed" }
    "*exit*"     { "closed" }
    default      { "running" }
}

$rawTitle = Get-JsonProp $evt "title" $null
$title = if ($rawTitle) {
    [string]$rawTitle
} elseif ($cwd) {
    Split-Path -Leaf $cwd
} else {
    "Codex"
}

$rawDetail = Get-JsonProp $evt "last-assistant-message" (Get-JsonProp $evt "detail" $null)
$detail = if ($rawDetail) {
    ([string]$rawDetail).Trim()
} else {
    $eventType
}
$rawPid = Get-JsonProp $evt "pid" 0
[uint32]$sessionPid = 0
[void][uint32]::TryParse("$rawPid", [ref]$sessionPid)
if ($detail.Length -gt 120) {
    $detail = $detail.Substring(0, 120)
}

# Monotonic timestamp-based sequence guarantees ordering within the same second.
$seq = [uint64]([DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds())
$instance = "codex-hook-$threadId"

$sendScript = Join-Path $PSScriptRoot "send-agent-event.ps1"
$fileScript = Join-Path $PSScriptRoot "write-session-file.ps1"

try {
    & $sendScript `
        -Agent "codex" `
        -Session $threadId `
        -Title $title `
        -Detail $detail `
        -Cwd $cwd `
        -State $state `
        -Instance $instance `
        -Seq $seq `
        -SessionPid $sessionPid
    if ($LASTEXITCODE -eq 3) {
        & $fileScript `
            -Agent "codex" `
            -Session $threadId `
            -Title $title `
            -Detail $detail `
            -Cwd $cwd `
            -State $state `
            -Instance $instance `
            -Seq $seq `
            -SessionPid $sessionPid
    }
} catch {
    # Fallback to atomic session file if WM_COPYDATA wasn't available.
    & $fileScript `
        -Agent "codex" `
        -Session $threadId `
        -Title $title `
        -Detail $detail `
        -Cwd $cwd `
        -State $state `
        -Instance $instance `
        -Seq $seq `
        -SessionPid $sessionPid
}
