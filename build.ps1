<#
.SYNOPSIS
    The ONE and ONLY blessed way to produce a runnable FeatherDock.

.DESCRIPTION
    Builds the release binary and leaves EXACTLY ONE executable on disk that you
    are ever meant to run:

        <repo-root>\FeatherDock.exe

    Everything under the Cargo target directory is disposable build cache. After
    a successful build this script copies the fresh binary to the repo root and
    then DELETES every other featherdock executable (the in-target release copy, the hashed
    deps\featherdock-<hash>.exe copies, any stale debug copy, and the legacy
    release\ folder). So you never again have to guess which .exe is current.

    Rule of thumb: if it isn't  .\FeatherDock.exe  in this folder, don't run it.

.PARAMETER Clean
    Run `cargo clean` first for a fully from-scratch build.

.PARAMETER Run
    Launch FeatherDock.exe after a successful build.

.PARAMETER Verify
    Run the full pre-release gate BEFORE building: cargo fmt --check, clippy
    (-D warnings), and the test suite. Any failure aborts before the release build.

.PARAMETER Stage
    Write the executable to outputs\pending\FeatherDock.exe instead of replacing
    the canonical root executable. Cannot be combined with -Run.

.EXAMPLE
    .\build.ps1
.EXAMPLE
    .\build.ps1 -Run
.EXAMPLE
    .\build.ps1 -Clean -Run
.EXAMPLE
    .\build.ps1 -Verify
.EXAMPLE
    .\build.ps1 -Verify -Stage
#>
[CmdletBinding()]
param(
    [switch]$Clean,
    [switch]$Run,
    [switch]$Verify,
    [switch]$Stage
)

$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
Set-Location -LiteralPath $root
if ($Stage -and $Run) { throw '-Stage cannot be combined with -Run.' }
& (Join-Path $root 'scripts\dev-env.ps1') -Quiet

# The single canonical executable. This is the only exe you should ever launch.
$FinalExe   = Join-Path $root 'FeatherDock.exe'
if ($Stage) {
    $stageDirectory = Join-Path $root 'outputs\pending'
    [void](New-Item -ItemType Directory -Path $stageDirectory -Force)
    $FinalExe = Join-Path $stageDirectory 'FeatherDock.exe'
}
$TargetDir = if ($env:CARGO_TARGET_DIR) {
    if ([System.IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) {
        [System.IO.Path]::GetFullPath($env:CARGO_TARGET_DIR)
    } else {
        [System.IO.Path]::GetFullPath((Join-Path $root $env:CARGO_TARGET_DIR))
    }
} else {
    Join-Path $root 'target'
}
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

if ($Verify) {
    Write-Host '==> Verify: cargo fmt --check' -ForegroundColor Cyan
    cargo fmt --all -- --check
    if ($LASTEXITCODE -ne 0) { throw "cargo fmt --check failed (exit $LASTEXITCODE)" }

    Write-Host '==> Verify: cargo clippy --all-targets --locked -- -D warnings' -ForegroundColor Cyan
    cargo clippy --all-targets --locked -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw "cargo clippy failed (exit $LASTEXITCODE)" }

    Write-Host '==> Verify: cargo test --locked' -ForegroundColor Cyan
    cargo test --locked
    if ($LASTEXITCODE -ne 0) { throw "cargo test failed (exit $LASTEXITCODE)" }

    Write-Host '   Verify passed; proceeding to release build.' -ForegroundColor Green
}

# Keep local build paths out of the shipped binary. Dependency panic-location
# strings would otherwise embed absolute paths like
# C:\Users\<name>\.cargo\registry\..., leaking the Windows username into the
# distributed exe (`strip` removes symbols but not these rodata strings). We
# remap the home and cargo-registry prefixes to neutral names. The prefixes are
# read from the environment, so no username is ever hardcoded in this repo.
# Note: setting RUSTFLAGS overrides .cargo/config.toml's rustflags, so the
# self-contained-link flag is repeated here (the linker setting still applies).
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$env:RUSTFLAGS = "-C link-self-contained=yes --remap-path-prefix=$env:USERPROFILE=/home --remap-path-prefix=$cargoHome=/cargo"

Write-Host '==> cargo build --release --locked' -ForegroundColor Cyan
cargo build --release --locked
if ($LASTEXITCODE -ne 0) { throw "cargo build --release --locked failed (exit $LASTEXITCODE)" }

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

# Publish to the selected root or staging destination; non-staged builds clean strays.
Copy-Item -LiteralPath $built.FullName -Destination $FinalExe -Force
# Sandboxed workspaces can inherit a Low mandatory label onto newly built EXEs.
# Windows then starts even an Explorer-launched Dock at Low integrity, which
# blocks normal user-cache writes and communication with Explorer's tray. Give
# only the shipped executable the normal desktop-app level; no elevation needed.
$artifactAclTool = Join-Path $env:SystemRoot 'System32\icacls.exe'
$prevErrorAction = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
try {
    $artifactAclOutput = (& $artifactAclTool $FinalExe /setintegritylevel M /q 2>&1) -join "`n"
    $artifactAclExit = $LASTEXITCODE
} finally {
    $ErrorActionPreference = $prevErrorAction
}
if ($artifactAclExit -ne 0) {
    $labelInfo = (& $artifactAclTool $FinalExe 2>&1) -join "`n"
    $groupInfo = (& (Join-Path $env:SystemRoot 'System32\whoami.exe') /groups 2>&1) -join "`n"
    $hasLowOrUntrustedFileLabel = $labelInfo -match 'Mandatory Label\\(Low|Untrusted) Mandatory Level'
    $isMediumOrHigherProcess = $groupInfo -match 'S-1-16-(8192|12288|16384)\b'
    if ($hasLowOrUntrustedFileLabel -or -not $isMediumOrHigherProcess) {
        throw "Could not set Medium integrity on FeatherDock.exe: $artifactAclOutput"
    }
    Write-Host '==> icacls /setintegritylevel M skipped (no WRITE_OWNER); verified Medium+ process & non-Low file label' -ForegroundColor DarkYellow
}
if (-not $Stage) { Remove-StrayExes }

# Kill the legacy release\ folder if it ever reappears (old publish location).
if (-not $Stage -and (Test-Path -LiteralPath $LegacyDir)) {
    Remove-Item -LiteralPath $LegacyDir -Recurse -Force
    Write-Host "==> removed legacy release\ folder" -ForegroundColor DarkYellow
}

$sizeKiB = [math]::Round((Get-Item -LiteralPath $FinalExe).Length / 1KB)
Write-Host ''
if ($Stage) {
    Write-Host '  Verified build staged; the running Dock and root executable were not changed:' -ForegroundColor Green
} else {
    Write-Host '  Build complete. The ONE executable to run:' -ForegroundColor Green
}
Write-Host "    $FinalExe  ($sizeKiB KiB)" -ForegroundColor Green
Write-Host ''

if ($Run) {
    Write-Host '==> launching FeatherDock' -ForegroundColor Cyan
    Start-Process -FilePath $FinalExe
}
