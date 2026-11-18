<#
.SYNOPSIS
    Prepare this PowerShell session for FeatherDock development.
.DESCRIPTION
    Reuse the workspace-local GNU toolchain in ..\Dock-build-tools when present,
    or an installed stable-x86_64-pc-windows-gnu toolchain under RUSTUP_HOME /
    USERPROFILE / the sibling .rustup directory, otherwise use Rust from PATH.
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

function Test-CompleteToolchainBin {
    param([string]$BinDir)
    if ([string]::IsNullOrWhiteSpace($BinDir) -or -not (Test-Path -LiteralPath $BinDir)) {
        return $false
    }
    foreach ($dockComponent in @('cargo.exe', 'rustc.exe', 'rustfmt.exe', 'cargo-clippy.exe')) {
        if (-not (Test-Path -LiteralPath (Join-Path $BinDir $dockComponent))) {
            return $false
        }
    }
    return $true
}

function Add-UniqueCandidate {
    param([System.Collections.Generic.List[string]]$List, [string]$Candidate)
    if ([string]::IsNullOrWhiteSpace($Candidate)) { return }
    $full = [System.IO.Path]::GetFullPath($Candidate)
    if (-not $List.Contains($full)) {
        $List.Add($full)
    }
}

$selectedRustup = $null
$selectedCargo = $null
$selectedBin = $null

if ($dockExplicitRoot) {
    $ToolRoot = [System.IO.Path]::GetFullPath($ToolRoot)
    $dockToolBin = Join-Path $ToolRoot 'rustup\toolchains\stable-x86_64-pc-windows-gnu\bin'
    if (-not (Test-CompleteToolchainBin $dockToolBin)) {
        throw "No complete Windows GNU Rust toolchain found in '$ToolRoot'."
    }
    $selectedRustup = Join-Path $ToolRoot 'rustup'
    $selectedCargo = Join-Path $ToolRoot 'cargo'
    $selectedBin = $dockToolBin
} else {
    $rustupCandidates = New-Object System.Collections.Generic.List[string]
    $localToolRoot = [System.IO.Path]::GetFullPath((Join-Path (Split-Path -Parent $dockRepo) 'Dock-build-tools'))
    Add-UniqueCandidate $rustupCandidates (Join-Path $localToolRoot 'rustup')
    if (-not [string]::IsNullOrWhiteSpace($env:RUSTUP_HOME)) {
        Add-UniqueCandidate $rustupCandidates $env:RUSTUP_HOME
    }
    if (-not [string]::IsNullOrWhiteSpace($env:USERPROFILE)) {
        Add-UniqueCandidate $rustupCandidates (Join-Path $env:USERPROFILE '.rustup')
    }
    if (-not [string]::IsNullOrWhiteSpace($env:CARGO_HOME)) {
        Add-UniqueCandidate $rustupCandidates (Join-Path (Split-Path -Parent $env:CARGO_HOME) '.rustup')
    }
    foreach ($exeName in @('cargo.exe', 'rustup.exe')) {
        $cmd = Get-Command $exeName -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($cmd) {
            $binParent = Split-Path -Parent $cmd.Source
            $homeParent = Split-Path -Parent $binParent
            $rootParent = Split-Path -Parent $homeParent
            Add-UniqueCandidate $rustupCandidates (Join-Path $rootParent '.rustup')
            Add-UniqueCandidate $rustupCandidates (Join-Path $rootParent 'rustup')
        }
    }

    foreach ($rustupCandidate in $rustupCandidates) {
        $candidateBin = Join-Path $rustupCandidate 'toolchains\stable-x86_64-pc-windows-gnu\bin'
        if (Test-CompleteToolchainBin $candidateBin) {
            $selectedRustup = $rustupCandidate
            $selectedBin = $candidateBin
            $parentDir = Split-Path -Parent $rustupCandidate
            foreach ($cargoName in @('cargo', '.cargo')) {
                $cargoCandidate = Join-Path $parentDir $cargoName
                if (Test-Path -LiteralPath $cargoCandidate) {
                    $selectedCargo = $cargoCandidate
                    break
                }
            }
            break
        }
    }
}

if ($selectedBin) {
    if ([string]::IsNullOrWhiteSpace($env:RUSTUP_HOME) -or $dockExplicitRoot -or ($selectedRustup -like '*\Dock-build-tools\rustup')) {
        $env:RUSTUP_HOME = $selectedRustup
    }
    if ($selectedCargo -and ([string]::IsNullOrWhiteSpace($env:CARGO_HOME) -or $dockExplicitRoot -or ($selectedCargo -like '*\Dock-build-tools\cargo'))) {
        $env:CARGO_HOME = $selectedCargo
    }
    $dockPathEntries = @($selectedBin)
    if ($selectedRustup) {
        $rustupParent = Split-Path -Parent $selectedRustup
        if (Test-Path -LiteralPath (Join-Path $rustupParent 'rustup.exe')) {
            $dockPathEntries += $rustupParent
        }
    }
    $dockPathEntries += $env:PATH -split ';' | Where-Object {
        $_ -and $_ -ne $selectedBin
    }
    $env:PATH = $dockPathEntries -join ';'
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
