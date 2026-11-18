<#
.SYNOPSIS
  Sube la rama actual y deja el PR en verde: push, alta si no existe y espera CI.

.DESCRIPTION
  El ciclo completo de un cambio, para que la calidad quede visible en GitHub:
    1. exige arbol limpio y rama distinta de la base;
    2. push con upstream (o --force-with-lease con -Force, tras un rebase);
    3. crea el PR si la rama aun no tiene uno (titulo = ultimo commit);
    4. espera las comprobaciones (`gh pr checks --watch`) e imprime el estado.
  Con -Merge lo aterriza en la base con merge rebase, que conserva los commits y
  no rompe las cadenas de PRs apilados.

.PARAMETER Title
  Titulo del PR nuevo. Por defecto, el asunto del ultimo commit.

.PARAMETER BodyFile
  Fichero Markdown con el cuerpo del PR nuevo.

.PARAMETER Base
  Rama destino. Por defecto, main.

.PARAMETER Force
  Fuerza el push con --force-with-lease (solo tras rebase).

.PARAMETER NoWait
  No espera a las comprobaciones: solo sube y asegura el PR.

.PARAMETER Merge
  Si todo esta en verde, hace merge rebase en la base.

.EXAMPLE
  pwsh scripts/pr.ps1
  pwsh scripts/pr.ps1 -Title "chore: limpieza" -BodyFile C:\tmp\pr.md -Merge
#>
[CmdletBinding()]
param(
    [string]$Title,
    [string]$BodyFile,
    [string]$Base = 'main',
    [switch]$Force,
    [switch]$NoWait,
    [switch]$Merge
)

$ErrorActionPreference = 'Stop'

function Fail([string]$Message) {
    Write-Host "FALLO: $Message" -ForegroundColor Red
    exit 1
}

if (-not (Get-Command gh -ErrorAction SilentlyContinue)) { Fail 'gh no esta en PATH' }

$branch = (git rev-parse --abbrev-ref HEAD).Trim()
if ($branch -eq $Base) { Fail "estas en ${Base}: crea una rama para el cambio" }
if (git status --porcelain) { Fail 'hay cambios sin commitear' }

Write-Host "==> push $branch" -ForegroundColor Cyan
$pushArgs = @('push', '--set-upstream', 'origin', $branch)
if ($Force) { $pushArgs = @('push', '--force-with-lease', '--set-upstream', 'origin', $branch) }
git @pushArgs
if ($LASTEXITCODE -ne 0) { Fail 'el push fallo' }

$open = gh pr list --head $branch --json number,url,state | ConvertFrom-Json |
    Where-Object { $_.state -eq 'OPEN' } | Select-Object -First 1

if (-not $open) {
    if (-not $Title) { $Title = (git log -1 --pretty=%s).Trim() }
    if ($BodyFile) {
        if (-not (Test-Path $BodyFile)) { Fail "no existe $BodyFile" }
        $bodyFileResolved = (Resolve-Path $BodyFile).Path
    } else {
        $body = (git log -1 --pretty=%b).Trim()
        if (-not $body) { $body = "Cubre: ver el commit $((git rev-parse --short HEAD).Trim())" }
        $bodyFileResolved = Join-Path ([System.IO.Path]::GetTempPath()) "pr-$([guid]::NewGuid().ToString('n')).md"
        Set-Content -Path $bodyFileResolved -Value $body -Encoding UTF8
    }
    Write-Host "==> alta del PR ($Title)" -ForegroundColor Cyan
    $url = gh pr create --base $Base --title $Title --body-file $bodyFileResolved
    if ($LASTEXITCODE -ne 0) { Fail 'gh pr create fallo' }
    $number = ($url -replace '.*/pull/', '').Trim()
} else {
    $number = $open.number
    Write-Host "==> PR ##$number ya existe: $($open.url)" -ForegroundColor Cyan
}

if (-not $NoWait) {
    Write-Host "==> esperando comprobaciones del PR ##$number" -ForegroundColor Cyan
    gh pr checks $number --watch --interval 10
    $checksExit = $LASTEXITCODE
    gh pr checks $number
    if ($checksExit -ne 0) { Fail "PR ##$number con comprobaciones en rojo" }
}

if ($Merge) {
    Write-Host "==> merge rebase de ##$number en $Base" -ForegroundColor Cyan
    gh pr merge $number --rebase
    if ($LASTEXITCODE -ne 0) { Fail 'el merge fallo (base desactualizada: rebase y reintenta)' }
}

Write-Host "OK: PR ##$number ($branch -> $Base)" -ForegroundColor Green
