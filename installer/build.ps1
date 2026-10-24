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

# The compiler itself, not whatever ISCC is on PATH: on CI runners that is
# Chocolatey's shim, whose file version is the shim's.
$iscc = Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'
if (-not (Test-Path $iscc)) {
    Write-Host 'Installing Inno Setup'
    choco install innosetup --version=6.7.1 --no-progress -y | Out-Host
    if (-not (Test-Path $iscc)) { throw "Inno Setup is not at $iscc" }
}
# The version the script is written and tested for. ISCC.exe's version
# resource reads 0.0.0.0, so ask the compiler: its banner names it.
$banner = try { & $iscc '/?' 2>&1 | Out-String } catch { "$_" }
if ($banner -notmatch 'Inno Setup 6') { throw "Inno Setup 6 needed, $iscc says: $banner" }
Write-Host ($banner -split "`n" | Where-Object { $_.Trim() } | Select-Object -First 1)

& $iscc "/DAppVersion=$Version" "/DAppNumericVersion=$numeric" (Join-Path $PSScriptRoot 'web-stream.iss')
if ($LASTEXITCODE -ne 0) { throw "ISCC failed ($LASTEXITCODE)" }
