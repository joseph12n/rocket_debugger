<#
.SYNOPSIS
  Gate local de calidad: lo mismo que exige CI antes de subir un PR.

.DESCRIPTION
  Corre en este orden y para en el primer fallo:
    1. cargo fmt --all -- --check
    2. cargo clippy --all-targets -- -D warnings
    3. cargo test --all
  El job de Windows de CI no corre tests, solo `cargo build --release --all`;
  con -Release se replica tambien ese paso.

.PARAMETER Quick
  Omite los tests: util al iterar sobre comentarios o formato.

.PARAMETER Release
  Anade la compilacion release que hace el job de Windows.

.EXAMPLE
  pwsh scripts/gate.ps1
  pwsh scripts/gate.ps1 -Quick
#>
[CmdletBinding()]
param(
    [switch]$Quick,
    [switch]$Release
)

$ErrorActionPreference = 'Stop'

# cargo no siempre esta en el PATH del proceso (sesion nueva, IDE, CI).
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
    # El sandbox del harness bloquea la escritura en %TEMP% para los binarios del
    # workspace: `tempfile` y los tests fallan con EACCES. Un temp dentro del
    # workspace evita el falso rojo; en CI no cambia nada.
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
