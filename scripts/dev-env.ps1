<#
.SYNOPSIS
    Prepare this PowerShell session for FeatherDock development.
.DESCRIPTION
    Reuse the workspace-local GNU toolchain in ..\Dock-build-tools when present,
    otherwise use Rust from PATH or the standard per-user Cargo bin directory.
    Changes apply only to this process; no registry or PowerShell profile edits.
.EXAMPLE
    .\scripts\dev-env.ps1
    cargo test --locked
#>
[CmdletBinding()]
param(
    [string]$ToolRoot,
    [switch]$Quiet
)

$ErrorActionPreference = 'Stop'
$dockRepo = Split-Path -Parent $PSScriptRoot
$dockExplicitRoot = -not [string]::IsNullOrWhiteSpace($ToolRoot)
if (-not $dockExplicitRoot) {
    $ToolRoot = Join-Path (Split-Path -Parent $dockRepo) 'Dock-build-tools'
}
$ToolRoot = [System.IO.Path]::GetFullPath($ToolRoot)
$dockToolBin = Join-Path $ToolRoot 'rustup\toolchains\stable-x86_64-pc-windows-gnu\bin'

if ((Test-Path -LiteralPath (Join-Path $dockToolBin 'cargo.exe')) -and
    (Test-Path -LiteralPath (Join-Path $dockToolBin 'rustc.exe'))) {
    foreach ($dockComponent in @('rustfmt.exe', 'cargo-clippy.exe')) {
        if (-not (Test-Path -LiteralPath (Join-Path $dockToolBin $dockComponent))) {
            throw "Missing Rust component: $dockComponent in '$dockToolBin'."
        }
    }
    $env:RUSTUP_HOME = Join-Path $ToolRoot 'rustup'
    $env:CARGO_HOME = Join-Path $ToolRoot 'cargo'
    $dockPathEntries = @($dockToolBin)
    if (Test-Path -LiteralPath (Join-Path $ToolRoot 'rustup.exe')) {
        $dockPathEntries += $ToolRoot
    }
    $dockPathEntries += $env:PATH -split ';' | Where-Object {
        $_ -and $_ -ne $dockToolBin -and $_ -ne $ToolRoot
    }
    $env:PATH = $dockPathEntries -join ';'
} elseif ($dockExplicitRoot) {
    throw "No complete Windows GNU Rust toolchain found in '$ToolRoot'."
} else {
    # A newly installed standard Rust toolchain may not be in this shell's PATH.
    $dockCargoBin = if ($env:CARGO_HOME) {
        Join-Path $env:CARGO_HOME 'bin'
    } else {
        Join-Path $env:USERPROFILE '.cargo\bin'
    }
    if (-not (Get-Command cargo.exe -CommandType Application -ErrorAction SilentlyContinue)) {
        if (Test-Path -LiteralPath (Join-Path $dockCargoBin 'cargo.exe')) {
            $env:PATH = $dockCargoBin + ';' + $env:PATH
        } else {
            throw 'Rust was not found. Install Rust with the Windows GNU toolchain, or provide -ToolRoot.'
        }
    }
}

# The bundled GNU linker needs a writable temp directory. Keep its temporary
# files and test fixtures in ignored outputs, so cargo clean cannot remove it.
$dockTemp = Join-Path $dockRepo 'outputs\tool-temp'
[void](New-Item -ItemType Directory -Path $dockTemp -Force)
$env:TEMP = $dockTemp
$env:TMP = $dockTemp
$env:TMPDIR = $dockTemp

if (-not $Quiet) {
    & rustc --version
    if ($LASTEXITCODE -ne 0) { throw 'Rust compiler is unavailable.' }
    & cargo --version
    if ($LASTEXITCODE -ne 0) { throw 'Cargo is unavailable.' }
    Write-Host 'FeatherDock development environment is ready in this PowerShell session.' -ForegroundColor Green
}
