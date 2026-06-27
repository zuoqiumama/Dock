<#
.SYNOPSIS
    The ONE and ONLY blessed way to produce a runnable FeatherDock.

.DESCRIPTION
    Builds the release binary and leaves EXACTLY ONE executable on disk that you
    are ever meant to run:

        <repo-root>\FeatherDock.exe

    Everything under target\ is disposable build cache. After a successful build
    this script copies the fresh binary to the repo root and then DELETES every
    other featherdock executable (the in-target release copy, the hashed
    deps\featherdock-<hash>.exe copies, any stale debug copy, and the legacy
    release\ folder). So you never again have to guess which .exe is current.

    Rule of thumb: if it isn't  .\FeatherDock.exe  in this folder, don't run it.

.PARAMETER Clean
    Run `cargo clean` first for a fully from-scratch build.

.PARAMETER Run
    Launch FeatherDock.exe after a successful build.

.EXAMPLE
    .\build.ps1
.EXAMPLE
    .\build.ps1 -Run
.EXAMPLE
    .\build.ps1 -Clean -Run
#>
[CmdletBinding()]
param(
    [switch]$Clean,
    [switch]$Run
)

$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
Set-Location -LiteralPath $root

# The single canonical executable. This is the only exe you should ever launch.
$FinalExe   = Join-Path $root 'FeatherDock.exe'
$TargetDir  = Join-Path $root 'target'
$LegacyDir  = Join-Path $root 'release'

function Remove-StrayExes {
    # Delete every featherdock executable anywhere under target\ so the root
    # FeatherDock.exe is the only one left standing. Build-script exes
    # (build-script-build.exe, build_script_build-*.exe) are intentionally NOT
    # matched and are left alone.
    if (-not (Test-Path -LiteralPath $TargetDir)) { return }
    $patterns = @('featherdock.exe', 'featherdock-*.exe')
    foreach ($pat in $patterns) {
        Get-ChildItem -LiteralPath $TargetDir -Recurse -Filter $pat -File -ErrorAction SilentlyContinue |
            ForEach-Object { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue }
    }
}

if ($Clean) {
    Write-Host '==> cargo clean' -ForegroundColor Cyan
    cargo clean
    if ($LASTEXITCODE -ne 0) { throw "cargo clean failed (exit $LASTEXITCODE)" }
}

Write-Host '==> cargo build --release' -ForegroundColor Cyan
cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build --release failed (exit $LASTEXITCODE)" }

# Locate the freshly built release binary. Triple-agnostic (the project pins
# x86_64-pc-windows-gnu in .cargo/config.toml, which adds a nested folder) and
# deliberately ignores the hashed copies under deps\.
$built = Get-ChildItem -LiteralPath $TargetDir -Recurse -Filter 'featherdock.exe' -File -ErrorAction SilentlyContinue |
    Where-Object { $_.DirectoryName -match '[\\/]release$' } |
    Sort-Object LastWriteTime -Descending |
    Select-Object -First 1

if (-not $built) {
    throw "Build reported success but no release featherdock.exe was found under '$TargetDir'."
}

# Place the one canonical exe at the repo root, then wipe all the strays.
Copy-Item -LiteralPath $built.FullName -Destination $FinalExe -Force
Remove-StrayExes

# Kill the legacy release\ folder if it ever reappears (old publish location).
if (Test-Path -LiteralPath $LegacyDir) {
    Remove-Item -LiteralPath $LegacyDir -Recurse -Force
    Write-Host "==> removed legacy release\ folder" -ForegroundColor DarkYellow
}

$sizeKiB = [math]::Round((Get-Item -LiteralPath $FinalExe).Length / 1KB)
Write-Host ''
Write-Host '  Build complete. The ONE executable to run:' -ForegroundColor Green
Write-Host "    $FinalExe  ($sizeKiB KiB)" -ForegroundColor Green
Write-Host ''

if ($Run) {
    Write-Host '==> launching FeatherDock' -ForegroundColor Cyan
    Start-Process -FilePath $FinalExe
}
