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

The install script builds the host, copies the web client next to it and adds
**web-stream host** to the Start menu. Start it from there. It is a tray app:
the window shows what the host is doing, and closing it leaves the host
running in the tray (click the icon to bring the window back; Quit in its
menu, or the Quit button, stops the host).

The window shows:

- whether the host is running, the addresses to open in the browser (click
  one to copy it) and the certificate fingerprint to compare with the
  browser's;
- the connected browser, with **Disconnect**;
- **Pair a browser**, which opens pairing for two minutes. The code to type
  in the browser appears here in large digits, and the window comes forward
  by itself when a browser asks for one. **Forget paired browsers** removes
  every paired browser;
- **Settings**: port, ICE-TCP fallback, public addresses, the web client
  folder, the display to stream first, codec, size, frame rate, bitrate cap,
  quality, game audio, the browser mic device and letting the browser change
  this PC's resolution. **More options** takes any other `ws-host` flag
  (`--cert a.pem --key b.pem`, `--ip`, `--hostname`; quote paths with
  spaces). **Save and restart** applies them; a browser that is streaming is
  disconnected and can reconnect. **Copy command line** gives the same host
  as a `ws-host` command;
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

### The console version

`ws-host.exe` is the same host without a window, for a console or a script:

```
cargo build --release -p ws-host
target\release\ws-host.exe --pair --web-root web
```

`ws-host` (or `ws-host serve`) starts everything: the HTTPS server with the
web client, pairing and signaling on TCP 8443, WebRTC media on UDP 8443 and
the ICE-TCP fallback on TCP 8444. It serves the web client from `web\` next
to the executable only (the install script copies it there; pass
`--web-root web` when running from a checkout) and keeps its certificate and
paired browsers in `%LOCALAPPDATA%\web-stream`. Don't run it and the tray app
at the same time; they use the same ports.

The console takes a few commands: `pair` opens pairing for two minutes (the
code to type in the browser is printed here), `drop` ends the session,
`unpair-all` forgets every paired browser, `quit` exits. `--pair` opens
pairing at startup. `ws-host --help` lists the options: ports, public IPs for
port forwarding, a real certificate, the starting display and stream
settings, and turning the mic or game audio off. The tray app's settings
are the same options. A paired browser that hasn't connected for 30 days is
unpaired (`--forget-after DAYS`). Behind a reverse proxy that rewrites the
Host header, pass its address with `--allow-origin https://name` or the
browser's requests are refused as cross-site.

## Connecting from outside your network

1. On the router, forward TCP 8443, UDP 8443 and TCP 8444 to this PC, to the
   same port numbers.
2. Allow `ws-host-gui` (and `ws-host`, if you use it) through Windows
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
estimate, up to `--kbps` or the bitrate chosen in the browser. If the browser
can't decode `--codec`, the host falls back to H.264 and says so in the
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

Browser gamepads appear as Xbox 360 controllers by default (`GamepadKind`
in `ws-input` switches to DualShock 4). Rumble is supported on Xbox 360 pads.

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
`--mic-device` (`ws-host audio-devices` lists them). If no device matches,
the browser shows an error listing the playback devices the host can see and
unticks Mic. `--no-mic` turns the mic off on the host whatever the browser
asks.

## Game audio

Loopback capture follows the default output device, including when it changes
mid-session; `--audio-device NAME` captures a named output instead. Any device
format is accepted; it is converted to 48 kHz stereo and sent as 10 ms Opus
packets at 128 kb/s (`--audio-kbps` changes that, 32 to 510).

## Elevated windows and the secure desktop

Input is injected as the logged-in user, so Windows blocks it into windows
running as administrator and on the UAC and lock screens. The planned SYSTEM
helper covers those; until then, run games without elevation.
