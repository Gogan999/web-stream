<#
.SYNOPSIS
    Installs what web-stream needs to build on Windows, then builds the host.

.DESCRIPTION
    Installs, through winget and only when missing:
      - Visual Studio 2022 Build Tools with the C++ tools and a Windows SDK
        (the MSVC linker and the C compiler for the bundled C libraries)
      - CMake (builds libopus and aws-lc)
      - Rust through rustup (stable, msvc)
      - Git, only when the repository has to be cloned
      - ViGEmBus, the gamepad driver, with -GamepadDriver
      - Steam's Streaming Microphone driver, from Steam's own folder, with
        -MicDriver (needs Steam; nothing is downloaded)
    Then builds the host (the ws-host-gui tray app) and adds a "web-stream
    host" shortcut to the Start menu.

    Run it from a checkout:
        powershell -ExecutionPolicy Bypass -File scripts\install.ps1
    or on a PC without the repository, which clones it into -Dir:
        & ([scriptblock]::Create((irm https://raw.githubusercontent.com/Gogan999/web-stream/main/scripts/install.ps1)))
    Options go at the end of either line, e.g. -GamepadDriver.

    Installing the Build Tools asks for administrator rights once (UAC).
    Running the script again is safe; anything already present is skipped.

.PARAMETER Dir
    Where to clone the repository when the script is not run from a checkout.

.PARAMETER GamepadDriver
    Also install the ViGEmBus driver for virtual gamepads.

.PARAMETER MicDriver
    Also install the Steam Streaming Microphone driver that Steam keeps in
    its folder, so the browser mic shows up in Windows without VB-CABLE.
    Skipped when VB-CABLE or the Steam microphone is already installed.

.PARAMETER Run
    Start the tray app after the build.
#>
[CmdletBinding()]
param(
    [string]$Dir = (Join-Path $HOME 'web-stream'),
    [switch]$GamepadDriver,
    [switch]$MicDriver,
    [switch]$Run
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RepoUrl = 'https://github.com/Gogan999/web-stream.git'
# winget's "already installed" result.
$WingetAlreadyInstalled = -1978335189
$VcComponent = 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64'
$SdkComponent = 'Microsoft.VisualStudio.Component.Windows11SDK.22621'
$VsWhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$VsSetup = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\setup.exe'

function Write-Step([string]$Message) {
    Write-Host "==> $Message" -ForegroundColor Cyan
}

function Update-SessionPath {
    $machine = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $user = [Environment]::GetEnvironmentVariable('Path', 'User')
    $env:Path = "$machine;$user;$(Join-Path $HOME '.cargo\bin')"
}

function Test-Command([string]$Name) {
    [bool](Get-Command $Name -ErrorAction SilentlyContinue)
}

function Install-WingetPackage([string]$Id, [string[]]$Extra = @()) {
    Write-Step "Installing $Id"
    $wingetArgs = @('install', '--exact', '--id', $Id, '--source', 'winget',
        '--accept-package-agreements', '--accept-source-agreements') + $Extra
    # Out-Host keeps winget's output off the pipeline of the calling function.
    & winget @wingetArgs | Out-Host
    if ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne $WingetAlreadyInstalled) {
        throw "winget could not install $Id (exit code $LASTEXITCODE)."
    }
    Update-SessionPath
}

function Get-VsWithCpp {
    if (-not (Test-Path $VsWhere)) { return $null }
    & $VsWhere -latest -products * -requires $VcComponent -property installationPath
}

function Test-WindowsSdk {
    $kits = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\Lib'
    [bool](Get-ChildItem -Path $kits -Filter kernel32.lib -Recurse -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '[\\/]um[\\/]x64[\\/]' } | Select-Object -First 1)
}

function Install-BuildTools {
    if ((Get-VsWithCpp) -and (Test-WindowsSdk)) {
        Write-Step 'Visual Studio C++ tools and Windows SDK found'
        return
    }

    $existing = $null
    if (Test-Path $VsWhere) {
        $existing = & $VsWhere -latest -products * -property installationPath
    }
    if ($existing) {
        # Visual Studio is there but lacks the C++ tools or the SDK: add them.
        Write-Step "Adding the C++ tools and Windows SDK to $existing"
        $p = Start-Process -FilePath $VsSetup -Wait -PassThru -Verb RunAs -ArgumentList @(
            'modify', '--installPath', "`"$existing`"",
            '--add', $VcComponent, '--add', $SdkComponent,
            '--passive', '--norestart')
        if ($p.ExitCode -ne 0 -and $p.ExitCode -ne 3010) {
            throw "The Visual Studio installer failed (exit code $($p.ExitCode))."
        }
    } else {
        Install-WingetPackage 'Microsoft.VisualStudio.2022.BuildTools' @('--override',
            '--wait --passive --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended')
    }

    if (-not ((Get-VsWithCpp) -and (Test-WindowsSdk))) {
        throw 'The C++ Build Tools or Windows SDK are still missing. Reboot and run this script again.'
    }
}

function Install-CMake {
    if (Test-Command cmake) {
        Write-Step 'CMake found'
        return
    }
    Install-WingetPackage 'Kitware.CMake' @('--silent')
    if (-not (Test-Command cmake)) {
        # The installer adds CMake to the machine PATH; fall back to its default folder.
        $env:Path += ";$(Join-Path $env:ProgramFiles 'CMake\bin')"
    }
    if (-not (Test-Command cmake)) { throw 'CMake was installed but is not on PATH.' }
}

function Install-Rust {
    if (-not (Test-Command rustup)) {
        Install-WingetPackage 'Rustlang.Rustup' @('--silent')
    }
    if (-not (Test-Command rustup)) { throw 'rustup was installed but is not on PATH.' }
    Write-Step 'Installing the stable Rust toolchain'
    & rustup toolchain install stable --profile minimal --component rustfmt --component clippy
    if ($LASTEXITCODE -ne 0) { throw 'rustup could not install the stable toolchain.' }
}

function Test-MicCable {
    [bool](Get-PnpDevice -Class MEDIA -ErrorAction SilentlyContinue | Where-Object {
        $_.FriendlyName -match 'VB-Audio Virtual Cable|Steam Streaming Microphone'
    })
}

# Installs Steam's signed virtual microphone the way Sunshine installs Steam
# Streaming Speakers: DiInstallDriverW on the INF in Steam's drivers folder.
function Install-SteamMic {
    if (Test-MicCable) {
        Write-Step 'A virtual mic (VB-CABLE or Steam Streaming Microphone) is already installed'
        return $true
    }
    $inf = Join-Path ${env:CommonProgramFiles(x86)} 'Steam\drivers\Windows10\x64\SteamStreamingMicrophone.inf'
    if (-not (Test-Path $inf)) {
        Write-Host "Steam's microphone driver is not on this PC ($inf). Install Steam, or VB-CABLE." -ForegroundColor Yellow
        return $false
    }
    Write-Step 'Installing the Steam Streaming Microphone driver'
    $code = @'
Add-Type -Namespace WebStream -Name NewDev -MemberDefinition '[DllImport("newdev.dll", CharSet = CharSet.Unicode, SetLastError = true)] public static extern bool DiInstallDriverW(IntPtr parent, string inf, uint flags, out bool reboot);'
$reboot = $false
if (-not [WebStream.NewDev]::DiInstallDriverW([IntPtr]::Zero, 'INF_PATH', 0, [ref]$reboot)) {
    exit [Runtime.InteropServices.Marshal]::GetLastWin32Error()
}
'@
    # An elevated process does not inherit this one's environment, so the path
    # goes into the script itself, as a single-quoted string.
    $code = $code.Replace('INF_PATH', $inf.Replace("'", "''"))
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($code))
    $p = Start-Process -FilePath powershell -Wait -PassThru -Verb RunAs -WindowStyle Hidden `
        -ArgumentList @('-NoProfile', '-EncodedCommand', $encoded)
    if ($p.ExitCode -ne 0) {
        Write-Host "Installing Steam's microphone driver failed (Windows error $($p.ExitCode))." -ForegroundColor Yellow
        return $false
    }
    Write-Host 'Installed. Check that your speakers or headphones are still the default output.'
    return $true
}

function Get-RepoRoot {
    if ($PSScriptRoot) {
        $root = Split-Path -Parent $PSScriptRoot
        if (Test-Path (Join-Path $root 'Cargo.toml')) { return $root }
    }
    if (Test-Path (Join-Path $Dir 'Cargo.toml')) { return $Dir }

    if (-not (Test-Command git)) { Install-WingetPackage 'Git.Git' @('--silent') }
    Write-Step "Cloning $RepoUrl into $Dir"
    & git clone $RepoUrl $Dir | Out-Host
    if ($LASTEXITCODE -ne 0) { throw 'git clone failed.' }
    return $Dir
}

# Brings an existing checkout of main up to date, so running the script
# again builds the latest version. Other branches and local changes are left
# alone.
function Update-Checkout($root) {
    if (-not (Test-Command git) -or -not (Test-Path (Join-Path $root '.git'))) { return }
    $branch = (& git -C $root rev-parse --abbrev-ref HEAD).Trim()
    $dirty = & git -C $root status --porcelain --untracked-files=no
    if ($branch -ne 'main' -or $dirty) {
        Write-Host "Building $branch as it is (not updating: not a clean main checkout)." -ForegroundColor Yellow
        return
    }
    Write-Step 'Updating the checkout'
    & git -C $root pull --ff-only | Out-Host
    if ($LASTEXITCODE -ne 0) {
        Write-Host 'Could not update the checkout; building it as it is.' -ForegroundColor Yellow
    }
}

if (-not (Test-Command winget)) {
    throw 'winget is missing. Install "App Installer" from the Microsoft Store, then run this script again.'
}

Update-SessionPath
$repo = Get-RepoRoot
Install-BuildTools
Install-CMake
Install-Rust
if ($GamepadDriver) { Install-WingetPackage 'ViGEm.ViGEmBus' @('--silent') }
$micReady = if ($MicDriver) { Install-SteamMic } else { Test-MicCable }

if (Get-Process -Name ws-host-gui, ws-host -ErrorAction SilentlyContinue) {
    throw 'The host is running. Quit it (tray icon, Quit) and run this script again.'
}

Update-Checkout $repo

Write-Step 'Building the host (release)'
Push-Location $repo
try {
    & cargo build --release -p ws-host-gui
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed.' }
} finally {
    Pop-Location
}

$bin = Join-Path $repo 'target\release'
$gui = Join-Path $bin 'ws-host-gui.exe'

# The web client is built into the host now, and the console host
# (ws-host.exe) is retired; old copies next to the tray app would only
# confuse.
$oldWeb = Join-Path $bin 'web'
if (Test-Path $oldWeb) { Remove-Item -Recurse -Force $oldWeb }
foreach ($old in 'ws-host.exe', 'ws-host.exe.old') {
    $f = Join-Path $bin $old
    if (Test-Path $f) { Remove-Item -Force $f -ErrorAction SilentlyContinue }
}

Write-Step 'Adding web-stream host to the Start menu'
$shortcutPath = Join-Path ([Environment]::GetFolderPath('Programs')) 'web-stream host.lnk'
$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut($shortcutPath)
$shortcut.TargetPath = $gui
$shortcut.WorkingDirectory = $bin
$shortcut.Description = 'Stream this PC to a browser'
$shortcut.Save()

Write-Host ''
Write-Host "Built $gui" -ForegroundColor Green
Write-Host 'Start "web-stream host" from the Start menu. It runs in the tray; closing'
Write-Host 'its window keeps it running, and Quit in the tray menu stops it.'
Write-Host 'Allow it through Windows Firewall (Private and Public) when Windows asks.'
if (-not $GamepadDriver) {
    Write-Host 'Gamepads need ViGEmBus: run this script again with -GamepadDriver.'
}
if (-not $micReady) {
    Write-Host 'The browser mic needs a virtual mic: run this script again with -MicDriver'
    Write-Host 'if Steam is installed, or install VB-CABLE; see docs\guide\audio-and-gamepads.md.'
}

if ($Run) {
    $admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)
    if ($admin) {
        # Never start the host elevated from an administrator shell: Explorer
        # starts it as the normal user, like the Start menu shortcut does.
        Start-Process -FilePath explorer.exe -ArgumentList "`"$gui`""
    } else {
        Start-Process -FilePath $gui -WorkingDirectory $bin
    }
}
