$bad = git diff --cached --name-only --diff-filter=ACM | Select-String -Pattern '(^|/)\.env'
if ($bad) {
    Write-Host 'Error: .env en staging. Quitalo del commit:' -ForegroundColor Red
    $bad | ForEach-Object { Write-Host "  $_" }
    exit 1
}
