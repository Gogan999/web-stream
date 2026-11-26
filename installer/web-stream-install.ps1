<#
.SYNOPSIS
    Installs or updates web-stream host in one step.

.DESCRIPTION
    Run in PowerShell:
        irm https://github.com/Gogan999/web-stream/releases/latest/download/web-stream-install.ps1 | iex
    Downloads the latest release's setup, checks its size and SHA-256 against the release
    manifest, and runs it silently. Running it again updates in place.
#>

function Get-InstalledVersion([string]$Key) {
    $item = Get-ItemProperty -Path $Key -ErrorAction SilentlyContinue
    if ($item -and $item.PSObject.Properties['DisplayVersion']) { $item.DisplayVersion } else { $null }
}

function Install-WebStream {
    Set-StrictMode -Version Latest
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $repo = if ($env:WEB_STREAM_RELEASES) { $env:WEB_STREAM_RELEASES.TrimEnd('/') } else { 'https://github.com/Gogan999/web-stream/releases' }
    $uninstallKey = 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\{918435C9-8423-4488-9252-A4724B1CB18E}_is1'

    if ([Environment]::OSVersion.Version.Major -lt 10 -or -not [Environment]::Is64BitOperatingSystem) {
        throw 'web-stream host needs 64-bit Windows 10 or 11.'
    }

    Write-Host 'Checking the latest web-stream host release...'
    $manifest = Invoke-RestMethod -UseBasicParsing -Uri "$repo/latest/download/manifest.json"
    if ($manifest.product -ne 'web-stream-host' -or $manifest.version -notmatch '^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$') {
        throw 'The release manifest is not a web-stream host release.'
    }
    $version = $manifest.version
    $name = "web-stream-setup-$version.exe"
    $file = @($manifest.files) | Where-Object { $_.name -eq $name } | Select-Object -First 1
    if (-not $file -or $file.sha256 -notmatch '^[0-9a-fA-F]{64}$') {
        throw "The release $version lists no valid $name."
    }

    $installed = Get-InstalledVersion $uninstallKey
    if ($installed -eq $version -and -not $env:WEB_STREAM_REINSTALL) {
        Write-Host "web-stream host $version is already installed and up to date."
        return
    }

    $dir = Join-Path ([IO.Path]::GetTempPath()) ("web-stream-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $dir | Out-Null
    try {
        $setup = Join-Path $dir $name
        Write-Host "Downloading web-stream host $version..."
        Invoke-WebRequest -UseBasicParsing -Uri "$repo/download/v$version/$name" -OutFile $setup
        if ((Get-Item $setup).Length -ne [int64]$file.size) {
            throw "The download is $((Get-Item $setup).Length) bytes, the release says $($file.size)."
        }
        $hash = (Get-FileHash -Algorithm SHA256 -Path $setup).Hash
        if ($hash -ne $file.sha256.ToUpperInvariant()) {
            throw 'The download does not match the release SHA-256.'
        }

        if ($installed) { Write-Host "Updating from $installed (approve the administrator prompt)..." }
        else { Write-Host 'Installing (approve the administrator prompt)...' }
        $p = Start-Process -FilePath $setup -Verb RunAs -PassThru `
            -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/RELAUNCH'
        $null = $p.Handle
        $p.WaitForExit()
        if ($p.ExitCode -ne 0) { throw "Setup exited with code $($p.ExitCode)." }
        Write-Host "web-stream host $version is installed. It is in the tray, near the clock." -ForegroundColor Green
    } finally {
        Remove-Item -Recurse -Force -Path $dir -ErrorAction SilentlyContinue
    }
}

Install-WebStream
