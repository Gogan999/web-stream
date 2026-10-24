<#
.SYNOPSIS
    Builds the installer from target\release\ws-host-gui.exe (build that
    first) and installer\redist (fetch-redist.ps1).

.PARAMETER Version
    The version it installs, e.g. 0.2.0 or 0.2.0-rc.1.
#>
param([Parameter(Mandatory)][string]$Version)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ($Version -notmatch '^(\d+)\.(\d+)\.(\d+)(-[0-9A-Za-z.-]+)?$') {
    throw "not a version: $Version"
}
$numeric = "$($Matches[1]).$($Matches[2]).$($Matches[3])"

$iscc = Get-Command iscc -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Source
if (-not $iscc) {
    $iscc = Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'
}
if (-not (Test-Path $iscc)) {
    Write-Host 'Installing Inno Setup'
    choco install innosetup --version=6.7.1 --no-progress -y | Out-Host
    $iscc = Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'
}
# The version the script is written and tested for.
$innoVersion = (Get-Item $iscc).VersionInfo.ProductVersion
if (-not $innoVersion.StartsWith('6.')) { throw "Inno Setup 6 needed, found $innoVersion" }
Write-Host "Inno Setup $innoVersion"

& $iscc "/DAppVersion=$Version" "/DAppNumericVersion=$numeric" (Join-Path $PSScriptRoot 'web-stream.iss')
if ($LASTEXITCODE -ne 0) { throw "ISCC failed ($LASTEXITCODE)" }
