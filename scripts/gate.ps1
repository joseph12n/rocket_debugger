

[CmdletBinding()]
param(
    [switch]$Quick,
    [switch]$Release
)

$ErrorActionPreference = 'Stop'

$cargo = if ($env:CARGO) { $env:CARGO } else { 'cargo' }
if (-not (Get-Command $cargo -ErrorAction SilentlyContinue)) {
    $candidate = Join-Path $HOME '.cargo\bin\cargo.exe'
    if (Test-Path $candidate) {
        $cargo = $candidate
    } else {
        Write-Host "cargo no esta en PATH ni en $candidate" -ForegroundColor Red
        exit 2
    }
}

function Invoke-Step {
    param([string]$Name, [string[]]$CargoArgs)
    Write-Host "==> $Name" -ForegroundColor Cyan
    & $cargo @CargoArgs
    if ($LASTEXITCODE -ne 0) {
        Write-Host "FALLO: $Name (exit $LASTEXITCODE)" -ForegroundColor Red
        exit $LASTEXITCODE
    }
}

Push-Location (Split-Path -Parent $PSScriptRoot)
try {
    $localTemp = Join-Path (Get-Location) 'target\tmp'
    New-Item -ItemType Directory -Force -Path $localTemp | Out-Null
    $env:TEMP = $localTemp
    $env:TMP = $localTemp

    Invoke-Step 'fmt' @('fmt', '--all', '--', '--check')
    Invoke-Step 'clippy' @('clippy', '--all-targets', '--', '-D', 'warnings')
    if (-not $Quick) { Invoke-Step 'tests' @('test', '--all') }
    if ($Release) { Invoke-Step 'build release' @('build', '--release', '--all') }
} finally {
    Pop-Location
}

Write-Host 'GATE OK' -ForegroundColor Green
