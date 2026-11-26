<#
.SYNOPSIS
    CI check of the one-line installer: installs, updates over a running host, and no-ops when current.

.PARAMETER Setup
    The installer the local release mirror serves (target\installer\web-stream-setup-*.exe).
#>
param([Parameter(Mandatory)][string]$Setup)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$App = Join-Path $env:ProgramFiles 'web-stream'
$UninstallKey = 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\{918435C9-8423-4488-9252-A4724B1CB18E}_is1'
$Script = Join-Path $PSScriptRoot 'web-stream-install.ps1'
$Port = 8765
$Failures = @()
$Mesa = @{
    Url    = 'https://github.com/pal1000/mesa-dist-win/releases/download/25.2.5/mesa3d-25.2.5-release-msvc.7z'
    Sha256 = '24B082CD3B66115A7DDB355E69FA0332DD22C751A392AB61EA3F5C9F6C912B88'
    Files  = @('opengl32.dll', 'libgallium_wgl.dll')
}

function Check([bool]$ok, [string]$what) {
    if ($ok) { Write-Host "ok   $what" } else { Write-Host "FAIL $what" -ForegroundColor Red; $script:Failures += $what }
}

function Status([string]$path) {
    & curl.exe -k -s -o NUL -w '%{http_code}' --max-time 5 "https://127.0.0.1:8443$path"
}

function Wait-Host {
    foreach ($i in 1..60) {
        if ((Status '/api/session') -eq '401') { return $true }
        Start-Sleep -Seconds 1
    }
    $false
}

function Host-Ids { @(Get-Process -Name 'ws-host-gui' -ErrorAction SilentlyContinue | ForEach-Object Id) }

function Invoke-OneLiner {
    $env:WEB_STREAM_RELEASES = "http://127.0.0.1:$Port"
    $out = Get-Content -Raw $Script | Invoke-Expression *>&1 | Out-String
    Write-Host $out
    $out
}

# Local release mirror laid out like GitHub's download URLs.
$version = [regex]::Match((Split-Path -Leaf $Setup), 'web-stream-setup-(.+)\.exe$').Groups[1].Value
$root = Join-Path $env:RUNNER_TEMP 'release-mirror'
Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
$latest = New-Item -ItemType Directory -Force (Join-Path $root 'latest\download')
$download = New-Item -ItemType Directory -Force (Join-Path $root "download\v$version")
$name = "web-stream-setup-$version.exe"
Copy-Item $Setup (Join-Path $download $name)
$file = Get-Item (Join-Path $download $name)
@{
    product = 'web-stream-host'; version = $version; notes = 'CI'
    files   = @(@{ name = $name; size = $file.Length; sha256 = (Get-FileHash -Algorithm SHA256 $file).Hash.ToLower() })
} | ConvertTo-Json -Depth 4 | Set-Content -Encoding utf8 (Join-Path $latest 'manifest.json')
$server = Start-Process -PassThru -WindowStyle Hidden -FilePath python -ArgumentList '-m', 'http.server', $Port, '--bind', '127.0.0.1', '--directory', $root
try {
    foreach ($i in 1..30) {
        try { Invoke-WebRequest -UseBasicParsing "http://127.0.0.1:$Port/latest/download/manifest.json" | Out-Null; break } catch { Start-Sleep 1 }
    }

    # Software OpenGL next to the app before setup starts it (CI has no GPU).
    New-Item -ItemType Directory -Force $App | Out-Null
    $archive = Join-Path $env:RUNNER_TEMP 'mesa.7z'
    if (-not (Test-Path $archive) -or (Get-FileHash -Algorithm SHA256 $archive).Hash -ne $Mesa.Sha256) {
        Invoke-WebRequest -Uri $Mesa.Url -OutFile $archive -UseBasicParsing
    }
    if ((Get-FileHash -Algorithm SHA256 $archive).Hash -ne $Mesa.Sha256) { throw 'Mesa archive hash mismatch' }
    $mesaOut = Join-Path $env:RUNNER_TEMP 'mesa'
    & 7z x -y "-o$mesaOut" $archive ($Mesa.Files | ForEach-Object { "x64/$_" }) | Out-Null
    foreach ($f in $Mesa.Files) { Copy-Item (Join-Path $mesaOut "x64\$f") $App }
    $env:GALLIUM_DRIVER = 'llvmpipe'

    # Fresh install.
    Invoke-OneLiner | Out-Null
    $item = Get-ItemProperty -Path $UninstallKey -ErrorAction SilentlyContinue
    $installed = if ($item -and $item.PSObject.Properties['DisplayVersion']) { $item.DisplayVersion } else { $null }
    Check ($installed -eq $version) "the one-liner installs $version ($installed)"
    Check (Wait-Host) 'the one-liner starts the host'
    $before = Host-Ids
    Check ($before.Count -eq 1) "one host app is running ($($before.Count))"

    # Already current: nothing happens.
    $out = Invoke-OneLiner
    Check ($out -match 'already installed') 'a current install is left alone'
    Check (@(Compare-Object $before (Host-Ids)).Count -eq 0) 'the running host is untouched'

    # Update over the running host: it quits and comes back.
    $env:WEB_STREAM_REINSTALL = '1'
    Invoke-OneLiner | Out-Null
    Remove-Item Env:WEB_STREAM_REINSTALL
    Check (Wait-Host) 'the host answers after the update'
    $after = Host-Ids
    Check ($after.Count -eq 1 -and $before -notcontains $after[0]) 'the update restarts the running host'

    # A bad download is refused.
    $manifestPath = Join-Path $latest 'manifest.json'
    $m = Get-Content -Raw $manifestPath | ConvertFrom-Json
    $m.files[0].sha256 = '0' * 64
    $m | ConvertTo-Json -Depth 4 | Set-Content -Encoding utf8 $manifestPath
    $env:WEB_STREAM_REINSTALL = '1'
    $refused = $false
    try { Invoke-OneLiner | Out-Null } catch { $refused = $_.Exception.Message -match 'SHA-256' }
    Remove-Item Env:WEB_STREAM_REINSTALL
    Check $refused 'a download that does not match the manifest is refused'
} finally {
    Stop-Process -Name 'ws-host-gui' -Force -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 2
    foreach ($f in $Mesa.Files) { Remove-Item -Force (Join-Path $App $f) -ErrorAction SilentlyContinue }
    $uninstaller = Join-Path $App 'unins000.exe'
    if (Test-Path $uninstaller) {
        Start-Process -FilePath $uninstaller -Wait -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/PURGE'
        foreach ($i in 1..60) { if (-not (Test-Path $App)) { break }; Start-Sleep 1 }
    }
    Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue
}

if ($Failures.Count) { throw "$($Failures.Count) one-line installer checks failed" }
Write-Host 'one-line installer checks passed' -ForegroundColor Green
