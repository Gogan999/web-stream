<#
.SYNOPSIS
    Downloads what the installer bundles into installer\redist, checking
    each file against its pinned SHA-256.

.DESCRIPTION
    ViGEmBus (the gamepad driver, BSD-3-Clause, signed by its publisher).
    To move to a new version, change the URL and the hash together, after
    checking the new file's signature.
#>
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$Redist = @(
    @{
        Name   = 'ViGEmBus_1.22.0_x64_x86_arm64.exe'
        Url    = 'https://github.com/nefarius/ViGEmBus/releases/download/v1.22.0/ViGEmBus_1.22.0_x64_x86_arm64.exe'
        Sha256 = '89220A7865076B342892F98865F3499FB7C4CFD673159E89D352C360FD014C6A'
    }
)

$dir = Join-Path $PSScriptRoot 'redist'
New-Item -ItemType Directory -Force -Path $dir | Out-Null
foreach ($r in $Redist) {
    $path = Join-Path $dir $r.Name
    if (-not (Test-Path $path) -or (Get-FileHash -Algorithm SHA256 $path).Hash -ne $r.Sha256) {
        Write-Host "Downloading $($r.Name)"
        Invoke-WebRequest -Uri $r.Url -OutFile $path -UseBasicParsing
    }
    $hash = (Get-FileHash -Algorithm SHA256 $path).Hash
    if ($hash -ne $r.Sha256) {
        Remove-Item -Force $path
        throw "$($r.Name): SHA-256 is $hash, expected $($r.Sha256)"
    }
    Write-Host "$($r.Name) OK"
}
