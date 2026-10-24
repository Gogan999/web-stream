<#
.SYNOPSIS
    CI check of the installer: installs it silently (twice, as an upgrade
    would), starts the host and asks it for its web client, then uninstalls
    it and checks that nothing is left behind.

.PARAMETER Setup
    The installer to test (target\installer\web-stream-setup-*.exe).
#>
param([Parameter(Mandatory)][string]$Setup)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$App = Join-Path $env:ProgramFiles 'web-stream'
$Exe = Join-Path $App 'ws-host-gui.exe'
$RunKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$UninstallKey = 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\{918435C9-8423-4488-9252-A4724B1CB18E}_is1'
$Shortcut = Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\web-stream host.lnk'
$Data = Join-Path $env:LOCALAPPDATA 'web-stream'
$Failures = @()

function Check([bool]$ok, [string]$what) {
    if ($ok) { Write-Host "ok   $what" } else { Write-Host "FAIL $what" -ForegroundColor Red; $script:Failures += $what }
}

function Get-RunValue {
    (Get-ItemProperty -Path $RunKey -Name 'web-stream host' -ErrorAction SilentlyContinue).'web-stream host'
}

function Get-FirewallRules {
    @(Get-NetFirewallRule -DisplayName 'web-stream host' -ErrorAction SilentlyContinue)
}

function Install([string]$log) {
    $p = Start-Process -FilePath $Setup -Wait -PassThru -ArgumentList @(
        '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/TASKS="autostart"', "/LOG=`"$log`"")
    if ($p.ExitCode -ne 0) {
        Get-Content $log -Tail 40
        throw "setup exited with $($p.ExitCode)"
    }
}

function Status([string]$path) {
    # -k: the host's certificate is self-signed.
    & curl.exe -k -s -o NUL -w '%{http_code}' --max-time 5 "https://127.0.0.1:8443$path"
}

Install (Join-Path $env:RUNNER_TEMP 'setup1.log')
# Again over the first: an upgrade.
Install (Join-Path $env:RUNNER_TEMP 'setup2.log')

Check (Test-Path $Exe) 'the tray app is in Program Files'
Check (Test-Path (Join-Path $App 'licenses\ViGEmBus-LICENSE.txt')) 'third-party licences are installed'
Check (Test-Path $Shortcut) 'Start menu shortcut'
Check ((Get-FirewallRules).Count -eq 2) 'two firewall rules (tray app and service copy)'
$run = Get-RunValue
Check ($run -eq "`"$Exe`" --hidden") "start with Windows points at the installed app ($run)"
Check (Test-Path $UninstallKey) 'listed in Apps'

# The host the installed app runs.
Start-Process -FilePath $Exe -ArgumentList '--hidden'
$session = ''
foreach ($i in 1..60) {
    $session = Status '/api/session'
    if ($session -eq '401') { break }
    Start-Sleep -Seconds 1
}
Check ($session -eq '401') "the host answers, unpaired (/api/session: $session)"
Check ((Status '/') -eq '200') 'the host serves the web client'
Stop-Process -Name 'ws-host-gui' -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 2

# Uninstall. The uninstaller runs a copy of itself from the temp folder and
# returns at once, so wait for the folder to go.
Start-Process -FilePath (Join-Path $App 'unins000.exe') -Wait -ArgumentList @(
    '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/PURGE')
foreach ($i in 1..60) {
    if (-not (Test-Path $App) -and -not (Test-Path $UninstallKey)) { break }
    Start-Sleep -Seconds 1
}
Check (-not (Test-Path $App)) 'Program Files folder removed'
Check (-not (Test-Path $UninstallKey)) 'removed from Apps'
Check (-not (Test-Path $Shortcut)) 'shortcut removed'
Check ((Get-FirewallRules).Count -eq 0) 'firewall rules removed'
Check ($null -eq (Get-RunValue)) 'start with Windows removed'
Check (-not (Test-Path $Data)) 'settings removed (/PURGE)'
Check ($null -eq (Get-Service -Name 'web-stream' -ErrorAction SilentlyContinue)) 'no service left'

if ($Failures.Count) { throw "$($Failures.Count) installer checks failed" }
Write-Host 'installer checks passed' -ForegroundColor Green
