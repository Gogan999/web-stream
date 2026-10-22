<#
.SYNOPSIS
    Updates the host to the latest main and restarts it.

.DESCRIPTION
    For a PC already set up with install.ps1. Pulls the latest main into the
    checkout, builds the host (the ws-host-gui tray app) and restarts the
    tray app on the new build.

    The running host keeps serving during the build: its executables are
    renamed to .old first (Windows allows renaming a running program), so
    the build can write new ones. The host is only restarted once the build
    has succeeded, which makes it safe to run from a streaming session: the
    browser drops for a few seconds and reconnects. If the build fails, the
    old executables are put back and the host keeps running.

    The previous build stays as .old next to the new one until the next
    update, and -Rollback switches back to it.

        powershell -ExecutionPolicy Bypass -File scripts\update.ps1

    It installs nothing; run install.ps1 again for new build dependencies or
    drivers.

.PARAMETER Force
    Rebuild and restart even when the build is already the latest main.

.PARAMETER Rollback
    Switch back to the previous build (the .old executables) and restart.

.PARAMETER NoRestart
    Build, but leave the running host alone. The new build starts the next
    time the host is started.

.PARAMETER FromHost
    Used by the tray app when a browser asks for an update: build, but
    leave restarting to the tray app, which started this script and would
    take it down if the script stopped it. Exits with 3 when a new build is
    ready for the tray app to restart on, 0 when there was nothing to do.
#>
[CmdletBinding()]
param(
    [switch]$Force,
    [switch]$Rollback,
    [switch]$NoRestart,
    [switch]$FromHost
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$Exes = @('ws-host-gui.exe')
# update.ps1 -FromHost: a new build is ready, restart on it.
$RestartExit = 3

function Write-Step([string]$Message) {
    Write-Host "==> $Message" -ForegroundColor Cyan
}

function Update-SessionPath {
    $machine = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $user = [Environment]::GetEnvironmentVariable('Path', 'User')
    $env:Path = "$machine;$user;$(Join-Path $HOME '.cargo\bin')"
}

function Invoke-Git {
    $out = & git -C $repo @args
    if ($LASTEXITCODE -ne 0) { throw "git $($args -join ' ') failed." }
    $out
}

function Get-HostProcess([string]$Name) {
    $path = Join-Path $bin "$Name.exe"
    $oldPath = "$path.old"
    @(Get-Process -Name $Name -ErrorAction SilentlyContinue | Where-Object {
        $_.Path -and ($_.Path -ieq $path -or $_.Path -ieq $oldPath)
    })
}

# Swaps each exe with its .old copy. Works while the host runs from either.
function Switch-Builds {
    foreach ($exe in $Exes) {
        $cur = Join-Path $bin $exe
        $old = "$cur.old"
        $tmp = "$cur.swap"
        if (-not (Test-Path $old)) { continue }
        if (Test-Path $tmp) { Remove-Item -Force $tmp }
        if (Test-Path $cur) { Move-Item $cur $tmp }
        Move-Item $old $cur
        if (Test-Path $tmp) { Move-Item $tmp $old }
    }
    $commit = Join-Path $bin 'built-commit.txt'
    $oldCommit = "$commit.old"
    if (Test-Path $oldCommit) {
        $c = if (Test-Path $commit) { Get-Content $commit } else { $null }
        Copy-Item -Force $oldCommit $commit
        if ($c) { Set-Content -Path $oldCommit -Value $c } else { Remove-Item $oldCommit }
    }
}

# Puts the .old build back after a failed set-aside or build.
function Restore-Build {
    foreach ($exe in $Exes) {
        $cur = Join-Path $bin $exe
        if (Test-Path "$cur.old") { Move-Item -Force "$cur.old" $cur }
    }
    if (Test-Path "$commitFile.old") { Move-Item -Force "$commitFile.old" $commitFile }
}

# The web-stream service runs its own copy of the host from Program Files.
# Copying the new build there takes administrator rights: one UAC prompt,
# which a browser connected through the service can answer.
function Update-Service {
    if (-not (Get-Service -Name 'web-stream' -ErrorAction SilentlyContinue)) { return }
    $exe = Join-Path $bin 'ws-host-gui.exe'
    if ($NoRestart) {
        Write-Host 'The service keeps running the old build until you click "Update the service" in the tray app.'
        return
    }
    Write-Step 'Updating the web-stream service (asks for administrator rights)'
    try {
        $p = Start-Process -FilePath $exe -ArgumentList 'service', 'update', '--quiet' -Verb RunAs -Wait -PassThru
        $code = $p.ExitCode
    } catch {
        $code = 'declined'
    }
    if ($null -eq $code) {
        Write-Host 'Could not tell whether the service was updated; check with: sc query web-stream' -ForegroundColor Yellow
    } elseif ($code -ne 0) {
        Write-Host "The service was not updated ($code). Click `"Update the service`" in the tray app." -ForegroundColor Yellow
    }
}

# The console host (ws-host.exe) is retired: the tray app does everything
# it did. Removes what earlier builds left, unless it still runs.
function Remove-ConsoleHost {
    $console = Join-Path $bin 'ws-host.exe'
    if (-not (Test-Path $console) -and -not (Test-Path "$console.old")) { return }
    if (Get-HostProcess 'ws-host') {
        Write-Host 'The retired console host (ws-host.exe) is still running. Quit it; the tray app replaces it.' -ForegroundColor Yellow
        return
    }
    foreach ($f in $console, "$console.old") {
        if (Test-Path $f) { Remove-Item -Force $f -ErrorAction SilentlyContinue }
    }
}

function Restart-Host {
    Update-Service
    Remove-ConsoleHost
    if ($FromHost) {
        # The tray app that started this script restarts itself.
        return
    }
    $gui = Join-Path $bin 'ws-host-gui.exe'
    $running = Get-HostProcess 'ws-host-gui'
    # With the service hosting, the tray app would only fight it for the ports.
    if (Get-Service -Name 'web-stream' -ErrorAction SilentlyContinue) {
        if ($running) {
            Write-Host 'The tray app is running alongside the web-stream service; they use the same ports. Quit the tray app.' -ForegroundColor Yellow
        }
        return
    }
    if ($NoRestart) {
        if ($running) { Write-Host 'The host keeps running the old build until it is restarted.' }
        return
    }
    if ($running) {
        Write-Step 'Restarting the host'
        $running | Stop-Process -Force
        $running | Wait-Process -Timeout 15 -ErrorAction SilentlyContinue
        # A copy still running holds the single-instance lock, and the new one would exit.
        if (Get-HostProcess 'ws-host-gui') { throw 'The old host did not stop. Quit it from the tray and start it again.' }
    } else {
        Write-Step 'Starting the host'
    }
    $admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)
    if ($admin) {
        # Never start the host elevated: Explorer starts it as the normal user.
        Start-Process -FilePath explorer.exe -ArgumentList "`"$gui`""
    } else {
        # Back to the tray, as after Start with Windows: the window would cover the stream.
        $start = @{ FilePath = $gui; WorkingDirectory = $bin }
        if ($running) { $start.ArgumentList = '--hidden' }
        Start-Process @start
    }
}

Update-SessionPath
$repo = Split-Path -Parent $PSScriptRoot
if (-not (Test-Path (Join-Path $repo 'Cargo.toml'))) {
    throw "$repo is not a web-stream checkout. Run install.ps1 first."
}
$bin = Join-Path $repo 'target\release'
$commitFile = Join-Path $bin 'built-commit.txt'

if ($Rollback) {
    if (-not (Test-Path (Join-Path $bin 'ws-host-gui.exe.old'))) {
        throw 'There is no previous build to roll back to.'
    }
    Write-Step 'Switching back to the previous build'
    Switch-Builds
    Restart-Host
    $c = if (Test-Path $commitFile) { (Get-Content $commitFile).Trim().Substring(0, 8) } else { 'an unknown commit' }
    Write-Host "Now on the build of $c. Run update.ps1 -Rollback again to undo." -ForegroundColor Green
    return
}

foreach ($tool in 'git', 'cargo') {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        throw "$tool is missing. Run install.ps1 first."
    }
}

# A folder from GitHub's "Download ZIP" has no git history to pull into.
if (-not (Test-Path (Join-Path $repo '.git'))) {
    throw "$repo is not a git clone (a downloaded ZIP has no .git folder), so it can't be updated. Clone it once with: git clone https://github.com/Gogan999/web-stream.git, then run scripts\install.ps1 from the clone."
}
$branch = (Invoke-Git rev-parse --abbrev-ref HEAD).Trim()
if ($branch -ne 'main') {
    throw "The checkout is on $branch, not main. Switch to main (git switch main) and run this again."
}
if (Invoke-Git status --porcelain --untracked-files=no) {
    throw 'The checkout has local changes. Commit or discard them (git status) and run this again.'
}

Write-Step 'Fetching main'
Invoke-Git fetch origin main | Out-Host
$new = @(Invoke-Git log --oneline HEAD..origin/main)
$built = if (Test-Path $commitFile) { (Get-Content $commitFile).Trim() } else { $null }
$head = (Invoke-Git rev-parse HEAD).Trim()
if (-not $new -and $built -eq $head -and -not $Force) {
    Write-Host "Already up to date ($($head.Substring(0, 8)))." -ForegroundColor Green
    return
}
if ($new) {
    Write-Host "New on main:"
    $new | ForEach-Object { Write-Host "  $_" }
    Invoke-Git merge --ff-only origin/main | Out-Host
    $head = (Invoke-Git rev-parse HEAD).Trim()
}

# Keep the current build as .old, so the build can write new executables
# while the host runs and so -Rollback has something to go back to.
Write-Step 'Setting the current build aside'
# Delete the build before last first, so nothing has moved yet if one of
# its executables is still running.
foreach ($exe in $Exes) {
    $old = Join-Path $bin "$exe.old"
    if (-not (Test-Path $old)) { continue }
    try {
        Remove-Item -Force $old
    } catch {
        throw "$old is still in use (a host runs from it). Restart the host and run this again."
    }
}
try {
    foreach ($exe in $Exes) {
        $cur = Join-Path $bin $exe
        if (Test-Path $cur) { Move-Item $cur "$cur.old" }
    }
    if (Test-Path $commitFile) { Move-Item -Force $commitFile "$commitFile.old" }
} catch {
    Restore-Build
    throw "Could not set the current build aside: $_"
}
# Cargo hard-links target\release\<exe> to its copy in deps, so the running
# host also holds that file and the linker could not write it. Rename it
# too; earlier copies are deleted once nothing runs from them.
$deps = Join-Path $bin 'deps'
if (Test-Path $deps) {
    Get-ChildItem -Path $deps -Filter '*.old' | ForEach-Object {
        Remove-Item -Force $_.FullName -ErrorAction SilentlyContinue
    }
    $stamp = Get-Date -Format 'yyyyMMddHHmmss'
    Get-ChildItem -Path $deps -Filter '*.exe' | Where-Object {
        $_.Name -like 'ws_host_gui-*.exe'
    } | ForEach-Object {
        Move-Item $_.FullName (Join-Path $deps "$($_.Name).$stamp.old")
    }
}

Write-Step 'Building the host (release)'
Push-Location $repo
try {
    # --target-dir: a CARGO_TARGET_DIR in the environment would put the build
    # somewhere other than $bin.
    & cargo build --release --target-dir (Join-Path $repo 'target') -p ws-host-gui
    $ok = $LASTEXITCODE -eq 0
} finally {
    Pop-Location
}
if (-not $ok) {
    Restore-Build
    throw 'cargo build failed. The previous build is back in place and the host was left running.'
}
Set-Content -Path $commitFile -Value $head

Restart-Host
if ($FromHost) {
    Write-Host "Built $($head.Substring(0, 8))."
    exit $RestartExit
}
Write-Host "Updated to $($head.Substring(0, 8)). Run update.ps1 -Rollback to go back." -ForegroundColor Green
