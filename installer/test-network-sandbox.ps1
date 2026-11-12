<#
.SYNOPSIS
    CI check of the network host's sandbox, run as SYSTEM like the service:
    its folder gets the low integrity label (existing files too), and a
    process with its token can write there and bind sockets but not write
    anywhere else. Needs administrator rights (a CI runner has them).

.PARAMETER Exe
    The built ws-host-gui.exe.
#>
param([Parameter(Mandatory)][string]$Exe)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$Exe = (Resolve-Path $Exe).Path
$Scratch = Join-Path $env:RUNNER_TEMP 'sandbox-test'
if (-not $env:RUNNER_TEMP) { $Scratch = Join-Path $env:TEMP 'sandbox-test' }
Remove-Item $Scratch -Recurse -Force -ErrorAction SilentlyContinue
$Report = Join-Path $Scratch 'report.txt'
$Task = 'web-stream-sandbox-test'

# A scheduled task as SYSTEM is how CI becomes SYSTEM.
schtasks /create /f /tn $Task /sc once /st 00:00 /ru SYSTEM /rl HIGHEST `
    /tr "`"$Exe`" service sandbox-test `"$Scratch`"" | Out-Null
schtasks /run /tn $Task | Out-Null
$deadline = (Get-Date).AddSeconds(120)
while ((Get-Date) -lt $deadline) {
    if ((Test-Path $Report) -and (Select-String -Path $Report -Pattern '^RESULT' -Quiet)) { break }
    Start-Sleep -Seconds 2
}
schtasks /delete /f /tn $Task | Out-Null

if (-not (Test-Path $Report)) { throw 'the test left no report' }
Get-Content $Report | Write-Host
$ChildLog = Join-Path $Scratch 'child.log'
if (Test-Path $ChildLog) { Write-Host '--- child.log'; Get-Content $ChildLog | Write-Host }
$SystemDir = Join-Path $env:WINDIR 'System32\config\systemprofile\AppData\Local\web-stream\network'
Write-Host '--- icacls (labels)'
icacls $SystemDir | Write-Host
if (-not (Select-String -Path $Report -Pattern '^RESULT sandbox ok' -Quiet)) {
    throw 'the sandbox test failed'
}
