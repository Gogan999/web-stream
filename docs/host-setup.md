# Host setup (Windows)

`scripts\install.ps1` installs the build dependencies (Visual Studio Build
Tools with the C++ tools and Windows SDK, CMake, Rust) through winget and
builds the host; add `-GamepadDriver` for ViGEmBus and `-MicDriver` for the
Steam Streaming Microphone (see below):

```
powershell -ExecutionPolicy Bypass -File scripts\install.ps1
```

On a PC without the repository it clones it into `%USERPROFILE%\web-stream`:

```
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/Gogan999/web-stream/main/scripts/install.ps1)))
```

## Running the host

The install script updates the checkout (when it is a clean `main`), builds
the host with the web client built into it and adds **web-stream host** to
the Start menu. Start it from there. It is a tray app:
the window shows what the host is doing, and closing it leaves the host
running in the tray (click the icon to bring the window back; Quit in its
menu, or the Quit button, stops the host).

The window shows:

- whether the host is running, the addresses to open in the browser (click
  one to copy it) and the certificate fingerprint to compare with the
  browser's;
- the connected browser, with **Disconnect**, and **Send file...** to offer
  files to it (or drop files on the window). The browser shows them under
  **Files** and saves one when you click **Save** there. Shared files are
  listed with their progress and **Stop sharing**, and forgotten when the
  browser disconnects. With "let the browser send files to this PC" on in
  Settings, the browser can send files the other way; they land in
  `Downloads\web-stream` (or the folder set there), linked from here;
- **Pair a browser**, which opens pairing for two minutes. The code to type
  in the browser appears here in large digits, and the window comes forward
  by itself when a browser asks for one. **Forget paired browsers** removes
  every paired browser;
- **Settings**: port, ICE-TCP fallback and its port, public addresses,
  host names for the certificate, how long an unused browser stays paired,
  the web client folder, the display to stream first, codec, size, frame
  rate, bitrate cap, quality, game audio and the device it comes from, the
  browser mic device, the gamepad type and letting the browser change this
  PC's resolution. While hosting, the window lists the ports to forward on
  the router. **More options** takes any other host option
  (`--cert a.pem --key b.pem`, `--ip`, `--allow-origin`; quote paths with
  spaces; the list is below). **Save and restart** applies them; a browser
  that is streaming is disconnected and can reconnect;
- **Windows service**: installs, pairs, updates and removes the service
  that also streams the sign-in screen and UAC prompts (see below);
- **Start with Windows** starts the app in the tray when you sign in, and
  **start hosting when the app opens** (on by default) starts the host with
  it;
- recent activity. The full log is `ws-host-gui.log` in the data folder,
  which **Open data folder** opens.

Settings are saved in `%LOCALAPPDATA%\web-stream\gui-settings.json`, next
to the certificate and the paired browsers. Only one copy of the app runs;
starting it again brings its window forward.

Windows Firewall asks about `ws-host-gui.exe` the first time it listens.
Allow Private and Public networks (see below).

If starting fails with "access denied" on the data folder, an older build ran
the host as administrator and left files only an administrator can open.
Run web-stream host as administrator once (it repairs the folder for your
account), or delete `%LOCALAPPDATA%\web-stream` and pair again. Run the host
normally, not as administrator.

### More options

The tray app is the host's only program; the console version (`ws-host.exe`)
is retired, and `install.ps1` and `update.ps1` delete copies earlier builds
left. Everything it did is in the window: pairing, **Disconnect**,
**Forget paired browsers**, sharing files, and the options below, which go
in **More options** when the window has no field for them.

The host serves the web client built into it (`--web-root DIR` serves one
from a folder instead, for web development) on TCP 8443, with WebRTC media
on UDP 8443 and the ICE-TCP fallback on TCP 8444, and keeps its certificate
and paired browsers in `%LOCALAPPDATA%\web-stream`.

```
--port P             HTTPS (TCP) and media (UDP) port, default 8443
--ice-tcp-port P     ICE-TCP fallback port, default port+1
--no-ice-tcp         don't listen for ICE-TCP
--ip IP              local address for media (repeatable; default: every LAN IPv4)
--public-ip IP       public address that forwards to the host (repeatable)
--hostname NAME      extra name or IP for the self-signed certificate (repeatable)
--cert FILE --key FILE   PEM certificate instead of a self-signed one
--data-dir DIR       certificate and paired devices
--forget-after DAYS  unpair a browser unused for this long, default 30
--allow-origin URL   extra origin allowed to use the API (repeatable)
--web-root DIR       serve the web client from DIR instead of the one built in
--display N  --codec h264|h265|av1  --size WxH  --fps F  --kbps K
--quality speed|balanced|clarity
--mic-device NAME  --no-mic  --no-audio  --audio-device NAME
--audio-kbps K  --audio-frame-ms 10|20  --gamepad xbox360|ds4
--allow-resolution-change  --allow-upload  --upload-dir DIR  --allow-remote-update
```

A paired browser that hasn't connected for 30 days is unpaired
(`--forget-after DAYS`). Behind a reverse proxy that rewrites the Host
header, pass its address with `--allow-origin https://name` or the
browser's requests are refused as cross-site.

For development there are two examples in `crates/host`: a headless host
that takes `pair`, `drop`, `unpair-all`, `send <path>` and `quit` on stdin
(`cargo run -p ws-host --example test-host -- --pair`; the browser tests
use it), and `capture-test`, which encodes a display to a file and prints
frame rate, drops and latency
(`cargo run --release -p ws-host --example capture-test -- --help`).

## Updating

To update to the latest `main`, run

```
powershell -ExecutionPolicy Bypass -File scripts\update.ps1
```

It pulls `main`, builds while the host keeps running and restarts the tray
app on the new build only once the build has succeeded, so it also works
from a streaming session (the browser reconnects after a few seconds). If
the build fails, the host keeps running the old build. The previous build is
kept, and `scripts\update.ps1 -Rollback` switches back to it. It needs a
clean `main` git clone (a folder from GitHub's Download ZIP can't be
updated; clone the repository instead) and installs nothing; run `install.ps1` again for new
build dependencies or drivers.

To update from a browser, turn on "let the browser update and restart this
host" in the tray app's settings (`--allow-remote-update`). The browser's
Settings then has **Update host** (under Other), which asks for confirmation
and runs the same `update.ps1` on the host. The stream keeps running during
the build. Once it has built, the tray app starts the new build in the tray
and exits; the stream drops for a few seconds and the browser reconnects.
The browser shows the result if there was nothing to update or the build
failed, and the script's output is in `update.log` in
`%LOCALAPPDATA%\web-stream`. Only paired browsers can reach it, it is off by
default, and the browser can't choose what is built: it always builds `main`
from the clone the host runs from. The service can't restart itself, so it
refuses it.

Building `main` means trusting everyone who can merge to it, and the GitHub
account behind it. To build only what you have signed yourself, list your SSH
signing key in `.git\web-stream-allowed-signers` in the clone (git's
[allowed signers](https://git-scm.com/docs/git-config#Documentation/git-config.txt-gpgsshallowedSignersFile)
format, for example `you@example.com ssh-ed25519 AAAA...`). From then on
`update.ps1`, from the PC or the browser, builds the newest tag named
`host-*` on `main` whose SSH signature verifies against that file, and
nothing else: an OpenPGP or X.509 signature is refused, and so is a tag
object whose own name isn't the `host-*` name it was pushed under. A
checkout that has moved past the newest release (a hand `git pull`) is
refused until it is reset to a signed release.
Release a build by signing a tag on `main` with that key and pushing it:

```
git tag -s host-2026-10-22 -m "host release"   # with gpg.format=ssh and user.signingkey set
git push origin host-2026-10-22
```

The file is inside `.git`, so nothing pulled from GitHub can change it.

## Connecting from outside your network

1. On the router, forward TCP 8443, UDP 8443 and TCP 8444 to this PC, to the
   same port numbers.
2. Allow `ws-host-gui` through Windows
   Firewall on those ports for the Public profile as well as Private.
   Localhost works without this, which can make a firewall block look like
   a network problem.
3. Open the host by its WAN address or a DDNS name that points at it, for
   example `https://home.example.net:8443/`.

The host advertises the address in the browser's URL (resolving a DDNS name)
as a media address. `--public-ip` adds more, for when browsers reach the
host by some other name. If pairing works but the stream never starts, a
media port is not forwarded or the firewall is blocking it.

Capture, game audio, the mic and input start when a browser connects and
stop when it leaves; dropping the session releases every held key and
unplugs the virtual gamepads. The encoder follows the connection's bandwidth
estimate, up to `--kbps` or the bitrate chosen in the browser. The codec is
`--codec` unless the browser picks one (Codec in its settings). If the
browser can't decode it, or this PC's GPU can't encode it (AV1 needs an RTX
40 series or newer), the host falls back to H.264 and says so in the
browser.

`--quality` (or Quality in the browser) sets how hard NVENC works on each
frame. The default is `balanced` on purpose: NVENC preset P4 at up to
1080p60 and P3 up to 1440p60, stepping down above that. That costs a few
milliseconds of encode time over `speed` (P1, the old fixed setting) and
gives a much sharper picture in fast motion at the same bitrate. Use
`speed` on an older GPU or at very high resolutions or frame rates;
`clarity` gives the most detail per bit.

`--allow-resolution-change` lets the browser switch the streamed display's
resolution to the largest one that fits its screen ("Match host resolution"
in the browser). It is off by default because it changes this PC's screen.
The change is temporary: it is never saved as the display's setting, and
the host puts the resolution back when the browser disconnects or when the
host quits (including after a crash in the media code). If the host process
is killed or its console window closed, Windows should still revert it, but
Microsoft doesn't document that; a reboot always clears it.

Two drivers are needed on the host for gamepads and the microphone. Keyboard,
mouse and game audio need nothing extra.

## Gamepads: ViGEmBus

Virtual Xbox 360 and DualShock 4 controllers are created through the ViGEmBus
driver. Install the latest signed release from
https://github.com/nefarius/ViGEmBus/releases and reboot if asked.

Without it the host still streams; gamepad messages are dropped and the log
says "ViGEmBus driver is not installed". ViGEmBus is no longer developed but
its last release works on Windows 10 and 11.

Browser gamepads appear as Xbox 360 controllers by default; `--gamepad ds4`
(or Gamepads in the tray app's settings) plugs in DualShock 4 controllers
instead. Rumble is supported on Xbox 360 pads.

## Microphone: a virtual audio cable

Windows has no way for a program to add a microphone on its own; it takes an
audio driver. The host plays the browser mic into a render device whose other
end Windows shows as a microphone. With `--mic-device auto` (the default) it
uses the first of these it finds:

1. VB-CABLE's **CABLE Input**, which games see as **CABLE Output**.
2. Steam's **Speakers (Steam Streaming Microphone)**, which games see as
   **Microphone (Steam Streaming Microphone)**.

### Steam Streaming Microphone (no download)

Steam ships a signed virtual microphone driver for Remote Play in
`%CommonProgramFiles(x86)%\Steam\drivers\Windows10\x64`. If Steam is
installed, `scripts\install.ps1 -MicDriver` installs it from there (one UAC
prompt), the same way Sunshine installs Steam Streaming Speakers. If you have
used Steam Remote Play on this PC it may already be there: look for
"Steam Streaming Microphone" under Sound, video and game controllers in
Device Manager.

After installing, check that your speakers or headphones are still the
default output in Sound settings. In each game or chat app, pick
**Microphone (Steam Streaming Microphone)** as the microphone. If its
playback side, **Speakers (Steam Streaming Microphone)**, is disabled in the
Sound control panel, enable it.

### VB-CABLE

VB-Audio Virtual Cable (https://vb-audio.com/Cable/) is donationware. Its
licence does not allow putting it in another program's installer, so the
script does not install it.

1. Install VB-CABLE and reboot.
2. Leave **CABLE Input** as a playback device but do *not* make it the
   default output.
3. In each game or chat app, pick **CABLE Output** as the microphone.

### Either way

In the browser, tick **Mic**; the host plays nothing until a browser asks.
Game audio capture follows the default output, so a mic cable must never be
the default output or the browser hears its own mic; the host logs a warning
if it is. Another cable works too: pass part of its playback device name to
**Browser mic** in the settings (`--mic-device`; the settings window lists them). If no device matches,
the browser shows an error listing the playback devices the host can see and
unticks Mic. `--no-mic` turns the mic off on the host whatever the browser
asks.

## Game audio

Loopback capture follows the default output device, including when it changes
mid-session; `--audio-device NAME` captures a named output instead. Any device
format is accepted; it is converted to 48 kHz stereo and sent as 10 ms Opus
packets at 128 kb/s. `--audio-kbps` (32 to 510) and `--audio-frame-ms 20`
change that on the host (Audio quality in the tray app's settings window);
Audio in the browser's Settings overrides both for that browser. 20 ms frames sound noticeably better at the same bitrate and
reach the browser 10 ms later.

## Sign-in screen, UAC prompts and programs run as administrator

Run normally (the tray app), the host runs as you, and Windows
keeps it out of three places: programs running as administrator ignore its
input, and UAC prompts and the lock and sign-in screens are on a separate,
protected desktop it can neither capture nor type into. The stream freezes
there until the desktop comes back.

To reach those too, install the host as a Windows service: in the tray
app's settings, under **Windows service**, click **Install the service**
and accept the administrator prompt. It takes the saved settings, except
the web client folder, receiving files and updates from the browser, which
the service refuses (see below); **Reinstall with these settings** changes
them later. It copies the host to `C:\Program Files\web-stream`, registers
the **web-stream** service (starts with Windows) and starts it. A message
says how it went.
The service then keeps a host running on the PC's screen, as SYSTEM, from
the sign-in screen on: before anyone signs in, while the PC is locked,
after signing out, and when switching users. It follows Windows onto the
UAC and lock screens and types into programs running as administrator.

- **The tray app stops its own host** when installing, and doesn't start it
  while the service is installed: they use the same ports. It keeps running
  in the tray as the service's front end.
- **Pair again.** The service has its own certificate (the browser warns
  about a new one) and its own paired browsers. Click **Pair through the
  service** (an administrator prompt), request a code in the browser, and
  type the code the service console window shows. **Service console**
  opens the same window without pairing: **Disconnect the browser**,
  **Share files...** and **Unpair every browser** for the service's host.
- **Updating:** `scripts\update.ps1` updates the service too (one UAC
  prompt). By hand: build, then **Update the service** in the tray app.
- **Logs and data** are in
  `C:\Windows\System32\config\systemprofile\AppData\Local\web-stream`:
  `host.log` (the host) and `service.log` (the service starting and
  restarting it).
- **Uninstall** (an administrator prompt) removes the service and the
  Program Files copy, and leaves its data. Click **Start** in the tray app
  to host from it again.
- **Ctrl+Alt+Del:** a browser can't send it as keys, so use the
  **Ctrl+Alt+Del** button in the browser's top bar (or in **Keys** on a
  phone). The service asks Windows for it. Windows only takes it from a
  service when the policy *Disable or enable software Secure Attention
  Sequence* (Computer Configuration, Administrative Templates, Windows
  Components, Windows Logon Options) is enabled for **Services**, which is
  the registry value `SoftwareSASGeneration` = 1 under
  `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System`. It is
  off by default; `service.log` says when it is off and a request comes.
  Most PCs don't need Ctrl+Alt+Del to sign in: a click or a key opens the
  lock screen.

What this changes for security:

- Whoever is paired with the service can do anything on this PC, including
  answering UAC prompts. Pairing still needs the code shown on the host, and
  only an administrator can see it: the service's host is controlled only
  through a pipe that takes an elevated prompt, so a program running as a
  normal user can't open pairing.
- The host runs as SYSTEM, which is effectively full control of the PC.
  Its privileges are removed (it can't load drivers, debug other processes
  or take ownership of files), but it keeps the SYSTEM account itself:
  anyone who got code running in it through a bug could still write to
  System32 and the registry, or install a service of their own. Treat a
  bug in the network-facing host as a full compromise of the PC.
- What runs as SYSTEM is the copy in Program Files, which only
  administrators can change, never the build folder in your checkout.
- `--cert` and `--key` are copied into the service's data folder when you
  install, so a user who can write where they came from can't swap them.
- The service picks a new random name for its control pipe each time it
  starts the host and keeps it, with the host's process id, in its data
  folder. The service console connects only if that process
  answers, so a program running as a normal user that takes the name
  can't pose as the host.
- While the host restarts (signing out, switching users) the ports are
  briefly free, and any local program could listen on them. A browser that
  connects then sees a certificate other than the one it trusts and warns.
- The service refuses `--web-root` (a folder a user could change would run
  their code in a browser that controls the PC), `--data-dir`, and receiving
  files from the browser (`--allow-upload`), which would write as SYSTEM into
  a folder a user controls. Sending files from the host still works through
  the service console.
