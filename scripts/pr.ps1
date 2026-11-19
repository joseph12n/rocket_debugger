

[CmdletBinding()]
param(
    [string]$Title,
    [string]$BodyFile,
    [string]$Base = 'main',
    [int]$MaxLines = 400,
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
if ($branch -notmatch '^(feat|fix|refactor|perf|docs|test|build|ci|chore)/[a-z0-9]+(-[a-z0-9]+)*$') {
    Fail "rama '$branch': usa <tipo>/<slug-en-ingles>, p.ej. feat/exec-broker, fix/redact-apostrophe"
}
if (git status --porcelain) { Fail 'hay cambios sin commitear' }

git fetch -q origin $Base
if ($LASTEXITCODE -ne 0) { Fail "no se pudo actualizar origin/${Base}" }

$removed = @{}
$added = New-Object System.Collections.Generic.List[string]
$file = ''
foreach ($line in (git diff --unified=0 "origin/${Base}...HEAD")) {
    if ($line -like '+++ b/*') { $file = $line.Substring(6); continue }
    if ($line -like '--- a/*') { continue }
    if ($file -match '^(docs|fixtures)/' -or $file -match '\.md$' -or $file -eq 'Cargo.lock') { continue }
    if ($line.StartsWith('+') -and -not $line.StartsWith('+++')) {
        $text = $line.Substring(1)
        if ($text.Trim()) { $added.Add($text) }
    } elseif ($line.StartsWith('-') -and -not $line.StartsWith('---')) {
        $text = $line.Substring(1)
        if (-not $text.Trim()) { continue }
        if ($removed.ContainsKey($text)) { $removed[$text]++ } else { $removed[$text] = 1 }
    }
}
$changed = 0
foreach ($text in $added) {
    if ($removed.ContainsKey($text) -and $removed[$text] -gt 0) {
        $removed[$text]--
    } else {
        $changed++
    }
}
foreach ($text in $removed.Keys) { $changed += $removed[$text] }
if ($changed -gt $MaxLines) {
    Fail "el PR cambia $changed lineas de codigo (tope $MaxLines): parte el trabajo en PRs mas pequenos"
}
Write-Host "==> diff: $changed/$MaxLines lineas cambiadas" -ForegroundColor Cyan

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
    Write-Host "==> fast-forward de ##$number en $Base" -ForegroundColor Cyan
    git merge-base --is-ancestor "origin/${Base}" HEAD
    if ($LASTEXITCODE -ne 0) {
        Fail "la rama no esta al dia con ${Base}: rebase y vuelve a subirla"
    }
    git push origin "HEAD:refs/heads/${Base}"
    if ($LASTEXITCODE -ne 0) { Fail 'el push a la base fallo' }
    gh pr view $number --json state --jq .state | Out-Null
}

Write-Host "OK: PR ##$number ($branch -> $Base)" -ForegroundColor Green
