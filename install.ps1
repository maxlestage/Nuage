# Nuage DB - installs a server on Windows 10, 11 or Server (x64; ARM64 runs it too), in one
# command, from a PowerShell opened as administrator:
#
#   irm https://raw.githubusercontent.com/maxlestage/Nuage/master/install.ps1 | iex
#
# It sets up: nuage.exe (C:\Program Files\Nuage), the data (C:\ProgramData\Nuage\data), the
# settings and secrets in C:\ProgramData\Nuage\nuage.env (generated once, kept on upgrades;
# readable by administrators only), the backups where you choose, a scheduled task that
# starts Nuage DB with Windows (even before anyone signs in) and restarts it if it stops, and
# a firewall rule for the local network. Run it again to upgrade, or to change where the
# backups go: data and secrets stay.
#
# Backups (an empty data directory is restored from them at startup). Without one of these,
# the installer asks:
#   $env:NUAGE_BACKUP_DIR = 'E:\Nuage'        a folder on ANOTHER disk: an external disk, a NAS
#                                             share (\\nas\backup\nuage)
#   $env:NUAGE_BACKUP_S3_BUCKET = 'bucket'    S3-compatible storage, with NUAGE_BACKUP_S3_REGION,
#                                             NUAGE_BACKUP_S3_ENDPOINT (not AWS),
#                                             NUAGE_BACKUP_S3_ACCESS_KEY_ID and
#                                             NUAGE_BACKUP_S3_SECRET_ACCESS_KEY
#   $env:NUAGE_BACKUP = 'none'                no backup (not recommended)
#
# Other options: NUAGE_PORT (8080), NUAGE_PG_PORT (5432, 0: off), NUAGE_VERSION (a release,
# default the latest), NUAGE_BINARY (install this nuage.exe instead of downloading one).
#
#   $env:NUAGE_BACKUP_DIR = 'E:\Nuage'; irm https://raw.githubusercontent.com/maxlestage/Nuage/master/install.ps1 | iex
#
# Remove (the data, the backups and the settings stay):  $env:NUAGE_ACTION = 'uninstall'; irm ... | iex
# Remove the data and the settings too (not the backups): $env:NUAGE_ACTION = 'purge'; irm ... | iex

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$Repo = 'maxlestage/Nuage'
$Target = 'x86_64-pc-windows-msvc'
$BinDir = Join-Path $env:ProgramFiles 'Nuage'
$Bin = Join-Path $BinDir 'nuage.exe'
$ConfDir = Join-Path $env:ProgramData 'Nuage'
$Data = Join-Path $ConfDir 'data'
$EnvFile = Join-Path $ConfDir 'nuage.env'
$Log = Join-Path $ConfDir 'nuage.log'
$Runner = Join-Path $BinDir 'run.cmd'
$Task = 'Nuage DB'
$Rule = 'Nuage DB'

function Say($m) { Write-Host "==> $m" -ForegroundColor Cyan }
function Warn($m) { Write-Host "warning: $m" -ForegroundColor Yellow }
function Die($m) { Write-Host "error: $m" -ForegroundColor Red; throw $m }

$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) { Die 'run it from a PowerShell opened as administrator (right click, then Run as administrator)' }

$Action = if ($args.Count -gt 0) { $args[0] } elseif ($env:NUAGE_ACTION) { $env:NUAGE_ACTION } else { 'install' }

# --- the settings file -------------------------------------------------------------------
function Read-Lines { if (Test-Path $EnvFile) { @(Get-Content -LiteralPath $EnvFile) } else { @() } }
function Write-Lines($lines) {
    # UTF-8 without a byte order mark, as the server and every other tool read it.
    [IO.File]::WriteAllText($EnvFile, (($lines -join "`r`n") + "`r`n"), (New-Object Text.UTF8Encoding($false)))
}
function Get-Var($k) {
    $v = $null
    foreach ($l in Read-Lines) { if ($l -match "^$k=(.*)$") { $v = $Matches[1] } }
    $v
}
function Set-Var($k, $v) {
    $out = New-Object Collections.Generic.List[string]
    $done = $false
    foreach ($l in Read-Lines) {
        if ($l -match "^$k=") { if (-not $done) { $out.Add("$k=$v"); $done = $true } } else { $out.Add($l) }
    }
    if (-not $done) { $out.Add("$k=$v") }
    Write-Lines $out
}
function Remove-Var($k) { Write-Lines @(Read-Lines | Where-Object { $_ -notmatch "^$k=" }) }
$S3Vars = 'NUAGE_BACKUP_S3_BUCKET', 'NUAGE_BACKUP_S3_REGION', 'NUAGE_BACKUP_S3_ENDPOINT', 'NUAGE_BACKUP_S3_PREFIX',
    'NUAGE_BACKUP_S3_ACCESS_KEY_ID', 'NUAGE_BACKUP_S3_SECRET_ACCESS_KEY'

# Only administrators and the system read the data and the settings (its secrets).
function Protect($path) {
    & icacls $path /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' /Q | Out-Null
    if ($LASTEXITCODE -ne 0) { Die "cannot set the permissions of $path" }
}

function Stop-Nuage {
    if (Get-ScheduledTask -TaskName $Task -ErrorAction SilentlyContinue) {
        Stop-ScheduledTask -TaskName $Task -ErrorAction SilentlyContinue
    }
    Get-Process -Name nuage -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $Bin } |
        Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 1
}

# --- uninstall ---------------------------------------------------------------------------
if ($Action -eq 'uninstall' -or $Action -eq 'purge') {
    $backupDir = Get-Var 'NUAGE_BACKUP_DIR'
    Stop-Nuage
    Unregister-ScheduledTask -TaskName $Task -Confirm:$false -ErrorAction SilentlyContinue
    Get-NetFirewallRule -DisplayName "$Rule*" -ErrorAction SilentlyContinue | Remove-NetFirewallRule
    Remove-Item -LiteralPath $BinDir -Recurse -Force -ErrorAction SilentlyContinue
    if ($Action -eq 'purge') {
        Remove-Item -LiteralPath $ConfDir -Recurse -Force -ErrorAction SilentlyContinue
        Say 'Nuage DB is removed, with its data and its key.'
    } else {
        Say "Nuage DB is removed. The data ($Data) and the key ($EnvFile) are kept: reinstall to use them again."
    }
    if ($backupDir) { Say "The backups in $backupDir are left in place (delete them yourself if you want)." }
    return
}
if ($Action -ne 'install') { Die "unknown command '$Action' (install, uninstall or purge)" }

$Port = if ($env:NUAGE_PORT) { $env:NUAGE_PORT } else { '8080' }
$PgPort = if ($env:NUAGE_PG_PORT) { $env:NUAGE_PG_PORT } else { '5432' }
if ($Port -notmatch '^\d+$') { Die "NUAGE_PORT=$Port is not a port number" }
if ($PgPort -notmatch '^\d+$') { Die "NUAGE_PG_PORT=$PgPort is not a port number" }

# --- the binary --------------------------------------------------------------------------
$Tmp = Join-Path ([IO.Path]::GetTempPath()) ("nuage-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $Tmp | Out-Null
try {
    if ($env:NUAGE_BINARY) {
        Copy-Item -LiteralPath $env:NUAGE_BINARY (Join-Path $Tmp 'nuage.exe')
    } else {
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        $base = if ($env:NUAGE_VERSION) { "https://github.com/$Repo/releases/download/$($env:NUAGE_VERSION)" } else { "https://github.com/$Repo/releases/latest/download" }
        Say "Downloading Nuage DB ($Target)"
        $zip = Join-Path $Tmp 'nuage.zip'
        Invoke-WebRequest -UseBasicParsing "$base/nuage-$Target.zip" -OutFile $zip
        $want = ((Invoke-WebRequest -UseBasicParsing "$base/nuage-$Target.zip.sha256").Content -split '\s+')[0].ToLower()
        $got = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
        if ($want -ne $got) { Die "the download is corrupted (sha256 $got, expected $want)" }
        Expand-Archive -LiteralPath $zip -DestinationPath $Tmp -Force
    }
    $new = Join-Path $Tmp 'nuage.exe'
    $version = & $new --version
    if ($LASTEXITCODE -ne 0) { Die 'this binary does not run here' }
    if (-not ((& $new --help) -match '--env-file')) {
        Die "this binary ($version) is older than 0.4.2: the service needs 'nuage --env-file' (set NUAGE_VERSION to a newer release)"
    }
    Stop-Nuage
    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
    Copy-Item -LiteralPath $new $Bin -Force
} finally {
    Remove-Item -LiteralPath $Tmp -Recurse -Force -ErrorAction SilentlyContinue
}
Say "Installed $(& $Bin --version) in $Bin"

# --- data and settings -------------------------------------------------------------------
New-Item -ItemType Directory -Force -Path $ConfDir, $Data | Out-Null
Protect $ConfDir

$firstInstall = $false
if (-not (Test-Path $EnvFile)) {
    $firstInstall = $true
    $key = & $Bin genkey
    $token = 'nuage_' + (& $Bin genkey).Substring(0, 40)
    $password = (& $Bin genkey).Substring(0, 24)
    $adminToken = & $Bin genkey
    $lines = @(
        '# Nuage DB settings (read by the scheduled task "Nuage DB"). Every variable: see the README.',
        '# Keep a copy of NUAGE_ENCRYPTION_KEY somewhere safe, away from this computer: without it',
        '# the data and its backups cannot be read, by anyone.',
        "PORT=$Port",
        'NUAGE_BIND=0.0.0.0',
        '# The PostgreSQL protocol: any PostgreSQL driver, user any name, password a token.',
        "NUAGE_PG_PORT=$PgPort",
        'NUAGE_PG_BIND=0.0.0.0',
        "NUAGE_DATA_DIR=$Data",
        'NUAGE_NAME=nuage',
        "NUAGE_ENCRYPTION_KEY=$key",
        "NUAGE_TOKEN=$token",
        "NUAGE_ADMIN_PASSWORD=$password",
        '# The management API: `nuage apps create <name>` makes an application with it.',
        "NUAGE_ADMIN_TOKEN=$adminToken",
        '# Page cache of each application, in MiB; auto: an eighth of the memory (8 to 512 MiB).',
        'NUAGE_CACHE_MB=auto',
        'NUAGE_PUBLIC_STATUS=false',
        '# Backups: run the installer again with NUAGE_BACKUP_DIR or NUAGE_BACKUP_S3_* to change.'
    )
    if ($PgPort -eq '0') { $lines = $lines | Where-Object { $_ -notmatch '^NUAGE_PG_PORT=' } }
    Write-Lines $lines
} else {
    Say "Keeping the settings and secrets of $EnvFile"
    $p = Get-Var 'PORT'; if ($p) { $Port = $p }
    $p = Get-Var 'NUAGE_PG_PORT'; $PgPort = if ($p) { $p } else { '0' }
}

# --- backups -----------------------------------------------------------------------------
$interactive = [Environment]::UserInteractive -and -not $env:CI -and $Host.Name -eq 'ConsoleHost'

function Backup-ToDir($dir) {
    if (-not [IO.Path]::IsPathRooted($dir)) { Die "NUAGE_BACKUP_DIR=${dir}: give a full path (E:\Nuage, \\nas\backup\nuage)" }
    $parent = Split-Path -Parent $dir
    # Only the last folder is made: a disk that is not plugged in must not get its folder
    # made on this computer's own disk instead.
    if ($parent -and -not (Test-Path -LiteralPath $parent)) {
        Die "$parent does not exist: is the disk plugged in? (only the last folder of NUAGE_BACKUP_DIR is created)"
    }
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    if (-not $dir.StartsWith('\\')) { Protect $dir }
    if ((Split-Path -Qualifier $dir -ErrorAction SilentlyContinue) -eq (Split-Path -Qualifier $Data)) {
        Warn "the backup folder $dir is on the same disk as the data ($Data): if this disk fails, both are lost."
        Warn 'prefer another disk (an external disk, a NAS share) or S3-compatible storage.'
    }
    if ($dir.StartsWith('\\')) {
        Warn "the service runs as SYSTEM: give this computer's account ($env:COMPUTERNAME`$) write access to the share."
    }
    foreach ($k in $S3Vars) { Remove-Var $k }
    Set-Var 'NUAGE_BACKUP_DIR' $dir
    Say "Backups go to the folder $dir"
}

function Backup-ToS3($bucket, $region, $endpoint, $prefix, $access, $secret) {
    if (-not $access -or -not $secret) { Die 'S3 backups need NUAGE_BACKUP_S3_ACCESS_KEY_ID and NUAGE_BACKUP_S3_SECRET_ACCESS_KEY' }
    Remove-Var 'NUAGE_BACKUP_DIR'
    Set-Var 'NUAGE_BACKUP_S3_BUCKET' $bucket
    Set-Var 'NUAGE_BACKUP_S3_REGION' $(if ($region) { $region } else { 'us-east-1' })
    if ($endpoint) { Set-Var 'NUAGE_BACKUP_S3_ENDPOINT' $endpoint } else { Remove-Var 'NUAGE_BACKUP_S3_ENDPOINT' }
    Set-Var 'NUAGE_BACKUP_S3_PREFIX' $(if ($prefix) { $prefix } else { 'nuage/' })
    Set-Var 'NUAGE_BACKUP_S3_ACCESS_KEY_ID' $access
    Set-Var 'NUAGE_BACKUP_S3_SECRET_ACCESS_KEY' $secret
    Say "Backups go to the bucket $bucket"
}

function No-Backup {
    foreach ($k in @('NUAGE_BACKUP_DIR') + $S3Vars) { Remove-Var $k }
    Warn 'no backup: if this disk fails, the data is lost. Run the installer again with NUAGE_BACKUP_DIR to add one.'
}

function Ask($question, $default) {
    $a = Read-Host $question
    if ($a) { $a } else { $default }
}

if ($env:NUAGE_BACKUP_DIR) {
    Backup-ToDir $env:NUAGE_BACKUP_DIR
} elseif ($env:NUAGE_BACKUP_S3_BUCKET) {
    Backup-ToS3 $env:NUAGE_BACKUP_S3_BUCKET $env:NUAGE_BACKUP_S3_REGION $env:NUAGE_BACKUP_S3_ENDPOINT `
        $env:NUAGE_BACKUP_S3_PREFIX $env:NUAGE_BACKUP_S3_ACCESS_KEY_ID $env:NUAGE_BACKUP_S3_SECRET_ACCESS_KEY
} elseif ($env:NUAGE_BACKUP -eq 'none') {
    No-Backup
} elseif (Get-Var 'NUAGE_BACKUP_DIR') {
    $dir = Get-Var 'NUAGE_BACKUP_DIR'
    Say "Backups: kept as they are, to the folder $dir"
    if (-not (Test-Path -LiteralPath $dir)) { Warn "the backup folder $dir is not there: is the disk plugged in?" }
} elseif (Get-Var 'NUAGE_BACKUP_S3_BUCKET') {
    Say "Backups: kept as they are, to the bucket $(Get-Var 'NUAGE_BACKUP_S3_BUCKET')"
} elseif ($interactive) {
    Write-Host ''
    Say 'Where should the backups go? Nuage DB restores an empty data directory from them at startup.'
    $disks = (Get-PSDrive -PSProvider FileSystem | Where-Object { $_.Root -ne "$($env:SystemDrive)\" } | ForEach-Object { $_.Root }) -join ' '
    Write-Host "    1) a folder on another disk: an external disk, a NAS share$(if ($disks) { " (disks: $disks)" })"
    Write-Host '    2) S3-compatible storage: AWS S3, Cloudflare R2, Scaleway, Backblaze B2, MinIO...'
    Write-Host '    3) none for now (not recommended)'
    switch (Ask 'Your choice [1/2/3]' '1') {
        '1' {
            $dir = Ask 'Folder (full path, e.g. E:\Nuage)' ''
            if (-not $dir) { Die 'no folder given: run the installer again' }
            Backup-ToDir $dir
        }
        '2' {
            $bucket = Ask 'Bucket' ''
            if (-not $bucket) { Die 'no bucket given: run the installer again' }
            $region = Ask 'Region [us-east-1]' 'us-east-1'
            $endpoint = Ask 'Endpoint (empty for AWS; e.g. https://<account>.r2.cloudflarestorage.com)' ''
            $prefix = Ask 'Prefix [nuage/]' 'nuage/'
            $access = Ask 'Access key id' ''
            $secure = Read-Host 'Secret access key (hidden)' -AsSecureString
            $secret = [Runtime.InteropServices.Marshal]::PtrToStringAuto([Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure))
            Backup-ToS3 $bucket $region $endpoint $prefix $access $secret
        }
        default { No-Backup }
    }
} else {
    No-Backup
}

# --- the service: a scheduled task at startup, as SYSTEM ---------------------------------
# A small runner, so the log lands in a file.
[IO.File]::WriteAllText($Runner, "@echo off`r`n`"$Bin`" --env-file `"$EnvFile`" >> `"$Log`" 2>&1`r`n", (New-Object Text.UTF8Encoding($false)))
$taskAction = New-ScheduledTaskAction -Execute 'cmd.exe' -Argument "/d /c `"`"$Runner`"`""
$trigger = New-ScheduledTaskTrigger -AtStartup
$principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable `
    -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName $Task -Action $taskAction -Trigger $trigger -Principal $principal -Settings $settings `
    -Description 'Nuage DB: the database server (https://github.com/maxlestage/Nuage)' -Force | Out-Null
Start-ScheduledTask -TaskName $Task

# The local network may reach it (the firewall asks no one for a task that runs as SYSTEM).
Get-NetFirewallRule -DisplayName "$Rule*" -ErrorAction SilentlyContinue | Remove-NetFirewallRule
New-NetFirewallRule -DisplayName $Rule -Direction Inbound -Protocol TCP -LocalPort $Port -Action Allow -Profile Private, Domain | Out-Null
if ($PgPort -ne '0') {
    New-NetFirewallRule -DisplayName "$Rule (PostgreSQL)" -Direction Inbound -Protocol TCP -LocalPort $PgPort -Action Allow -Profile Private, Domain | Out-Null
}

# --- check -------------------------------------------------------------------------------
$running = $false
for ($i = 0; $i -lt 30; $i++) {
    try {
        Invoke-WebRequest -UseBasicParsing "http://127.0.0.1:$Port/api/health" -TimeoutSec 2 | Out-Null
        $running = $true
        break
    } catch { Start-Sleep -Seconds 1 }
}
if (-not $running) { Die "the server does not answer: Get-Content -Tail 50 '$Log'" }

$ip = (Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue |
    Where-Object { $_.IPAddress -notlike '127.*' -and $_.IPAddress -notlike '169.254.*' } | Select-Object -First 1).IPAddress
$url = "http://$(if ($ip) { $ip } else { 'localhost' }):$Port"
$backups = if (Get-Var 'NUAGE_BACKUP_DIR') { "the folder $(Get-Var 'NUAGE_BACKUP_DIR')" }
    elseif (Get-Var 'NUAGE_BACKUP_S3_BUCKET') { "the bucket $(Get-Var 'NUAGE_BACKUP_S3_BUCKET')" }
    else { 'none (run again with NUAGE_BACKUP_DIR to add one)' }
Write-Host ''
Say "Nuage DB is running: $url"
Write-Host "    console:      $url/admin"
Write-Host "    SQL:          POST $url/v1/sql   (Authorization: Bearer <token>)"
if ($PgPort -ne '0') { Write-Host "    PostgreSQL:   postgresql://app:<token>@$(if ($ip) { $ip } else { 'localhost' }):$PgPort/nuage" }
Write-Host "    backups:      $backups"
Write-Host "    settings:     $EnvFile"
Write-Host "    data:         $Data"
Write-Host "    logs:         Get-Content -Wait '$Log'"
Write-Host "    service:      the scheduled task '$Task' (Task Scheduler), started with Windows"
Write-Host "    new app:      `$env:NUAGE_URL='$url'; `$env:NUAGE_ADMIN_TOKEN='<NUAGE_ADMIN_TOKEN of $EnvFile>'; & '$Bin' apps create <name>"
if ($firstInstall) {
    Write-Host ''
    Write-Host "    application token:  $token"
    Write-Host "    console password:   $password"
    Write-Host ''
    Warn "copy $EnvFile (its NUAGE_ENCRYPTION_KEY) somewhere safe, away from this computer, now:"
    Warn 'without that key nobody can read the data or its backups.'
}
