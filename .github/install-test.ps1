# CI: install.ps1 end to end on Windows, with a backup folder.
#   .github\install-test.ps1 <nuage.exe> <backup folder>
# The same steps as install-test.sh: install as a scheduled task, health, first backup in the
# folder, `nuage apps create`, a run without options keeps the backup setting, uninstall
# keeps the backups, purge.
param([string]$Binary, [string]$Backup)
$ErrorActionPreference = 'Stop'
$Port = 8091
$EnvFile = Join-Path $env:ProgramData 'Nuage\nuage.env'
$Log = Join-Path $env:ProgramData 'Nuage\nuage.log'
$Binary = (Resolve-Path $Binary).Path

function Step($m) { Write-Host "`n### $m" -ForegroundColor Cyan }
function Fail($m) {
    Write-Host "FAILED: $m" -ForegroundColor Red
    if (Test-Path $Log) { Get-Content -Tail 80 $Log }
    exit 1
}
function Healthy {
    try { Invoke-WebRequest -UseBasicParsing "http://127.0.0.1:$Port/api/health" -TimeoutSec 2 | Out-Null; $true } catch { $false }
}
function Clear-Options {
    foreach ($k in 'NUAGE_BACKUP_DIR', 'NUAGE_PORT', 'NUAGE_PG_PORT', 'NUAGE_ACTION') { Remove-Item "env:$k" -ErrorAction SilentlyContinue }
}

Step "install, backups to $Backup"
$env:CI = '1'
$env:NUAGE_BINARY = $Binary
$env:NUAGE_PORT = "$Port"
$env:NUAGE_PG_PORT = '0'
$env:NUAGE_BACKUP_DIR = $Backup
& .\install.ps1
if (-not (Healthy)) { Fail 'the service does not answer' }
if (-not (Select-String -Path $EnvFile -SimpleMatch "NUAGE_BACKUP_DIR=$Backup" -Quiet)) { Fail "NUAGE_BACKUP_DIR is not in $EnvFile" }
$bytes = [IO.File]::ReadAllBytes($EnvFile)
if ($bytes[0] -eq 0xEF) { Fail 'the settings file starts with a byte order mark' }

Step 'the first backup lands in the folder'
$manifest = Join-Path $Backup 'manifest.json'
for ($i = 0; $i -lt 90; $i++) {
    if (Test-Path $manifest) { break }
    Start-Sleep -Seconds 1
}
if (-not (Test-Path (Join-Path $Backup 'manifest.json'))) { Fail "no backup in $Backup after 90 s" }
Get-ChildItem $Backup

Step 'nuage apps create, with the management token the installer generated'
$admin = ((Select-String -Path $EnvFile -Pattern '^NUAGE_ADMIN_TOKEN=(.*)$').Matches[0].Groups[1].Value)
$env:NUAGE_URL = "http://127.0.0.1:$Port"
$env:NUAGE_ADMIN_TOKEN = $admin
$lines = & $Binary apps create essai CI
$token = ($lines | Select-String -Pattern '^NUAGE_TOKEN=(.*)$').Matches[0].Groups[1].Value
if (-not $token) { Fail "no token: $lines" }
& $Binary check "http://127.0.0.1:$Port" $token
if ($LASTEXITCODE -ne 0) { Fail 'the new token does not connect' }

Step 'run again without options: the backup setting stays, the service restarts'
Clear-Options
$again = & .\install.ps1 *>&1 | Out-String
Write-Host $again
if ($again -notmatch 'Backups: kept as they are') { Fail 'the backup setting was not kept' }
if (-not (Healthy)) { Fail 'the service does not answer after the upgrade' }
if (-not ((& $Binary apps list) -match '^essai')) { Fail 'the application is gone' }

Step 'uninstall: the service stops, the backups and the settings stay'
$env:NUAGE_ACTION = 'uninstall'
& .\install.ps1
Start-Sleep -Seconds 2
if (Healthy) { Fail 'still answering after uninstall' }
if (-not (Test-Path (Join-Path $Backup 'manifest.json'))) { Fail 'the backups were removed' }
if (-not (Test-Path $EnvFile)) { Fail 'the settings were removed' }

Step 'purge: the data and the settings go, the backups stay'
$env:NUAGE_ACTION = 'purge'
& .\install.ps1
if (Test-Path $EnvFile) { Fail 'the settings are still there' }
if (-not (Test-Path (Join-Path $Backup 'manifest.json'))) { Fail 'the backups were removed' }
Write-Host "`nall good" -ForegroundColor Green
