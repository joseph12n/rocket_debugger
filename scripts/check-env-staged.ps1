# Falla si hay un fichero .env en staging: sus claves nunca deben viajar en un commit.
#
# Se ejecuta desde el hook pre-commit de lefthook. El chequeo va por nombre y no
# por contenido porque gitleaks ya cubre el contenido: esto corta el caso obvio
# antes de que llegue a escanear.
$bad = git diff --cached --name-only --diff-filter=ACM | Select-String -Pattern '(^|/)\.env'
if ($bad) {
    Write-Host 'Error: .env en staging. Quitalo del commit:' -ForegroundColor Red
    $bad | ForEach-Object { Write-Host "  $_" }
    exit 1
}
