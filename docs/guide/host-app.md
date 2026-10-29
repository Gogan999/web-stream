# The host app

**web-stream host** is the program that runs on the PC being streamed. It
is a tray app: the window shows what the host is doing, and closing it
leaves the host running in the system tray. Click the tray icon to bring
the window back; **Quit** in the tray menu, or the **Quit** button, stops
the host.

Only one copy of the app runs at a time. Starting it again brings the
existing window forward.

While the [Windows service](windows-service.md) is installed, the app
never hosts on its own: the service does, and the app, run as
administrator, attaches to it and shows it in this window.

## The window

- **Status:** whether the host is running, the addresses to open in a
  browser (click one to copy it), and the certificate fingerprint to
  compare with the browser's.
- **Connected browser:** who is connected, with **Disconnect**.
- **Pair a browser:** opens pairing for two minutes and shows a QR code
  with a one-time pairing link for a phone's camera. A browser can instead
  ask for a code, which appears here in large digits; the window comes
  forward by itself when that happens. **Forget paired browsers** removes
  every paired browser.
- **Send file...:** offers files to the connected browser (or drop files
  onto the window). See [Sharing files](#sharing-files).
- **Settings:** see below.
- **Windows service:** installs, updates and removes the service
  that also streams the sign-in screen and UAC prompts. See
  [Sign-in screen and administrator programs](windows-service.md).
- **Start with Windows:** starts the app in the tray when you sign in.
  **Start hosting when the app opens** (on by default) starts the host with
  it.
- **Recent activity.** The full log is `ws-host-gui.log` in the data
  folder, which **Open data folder** opens.
- **Setup** brings back the first-run setup (port forwarding, port test and
  pairing).

## Settings

The settings cover:

| Area | Settings |
|---|---|
| Network | Port, ICE-TCP fallback and its port, public addresses, extra host names for the certificate, DuckDNS name and token |
| Pairing | How long an unused browser stays paired (30 days by default) |
| Video | Display to stream first, codec, size, frame rate, bitrate cap, quality |
| Audio | Game audio quality and the device it comes from, the device the browser mic plays into |
| Input | Gamepad type (Xbox 360 or DualShock 4) |
| Permissions | Let the browser change this PC's resolution, send files to this PC, and update and restart the host |
| More options | Any other host option, as command-line flags (see the [host options reference](../dev/host-options.md)); quote paths that contain spaces |

**Save and restart** applies the changes (with the Windows service
installed, **Save and apply to the service** applies them to the service). A browser that is streaming is
disconnected and can reconnect straight away. While hosting, the window also
lists the ports to forward on the router.

Settings are saved in `%LOCALAPPDATA%\web-stream\gui-settings.json`, next to
the certificate and the paired browsers.

## Video quality

The encoder follows the connection's measured bandwidth, up to the bitrate
cap or the bitrate chosen in the browser. The codec is the one set here
unless the browser picks one. If the browser can't decode it, or the GPU
can't encode it (AV1 needs an RTX 40 series card or newer), the host falls
back to H.264 and says so in the browser.

**Quality** sets how hard the encoder works on each frame:

- **Balanced** (default): sharp in fast motion for a few milliseconds of
  extra encode time.
- **Speed:** the fastest encode. Use it on older GPUs or at very high
  resolutions or frame rates.
- **Clarity:** the most detail per bit.

**Let the browser change this PC's resolution** allows the browser's
**Match host resolution** option to switch the streamed display to the
largest resolution that fits the browser's screen. It is off by default
because it changes this PC's screen. The change is temporary: it is never
saved as the display's setting, and the host restores the resolution when
the browser disconnects or the host quits. A reboot always clears it.

## Sharing files

**Send file...** (or dropping files on the window) offers files to the
connected browser. The browser lists them under **Files** and downloads one
when you click **Save** there. Shared files are listed with their progress
and **Stop sharing**, and are forgotten when the browser disconnects.

With **let the browser send files to this PC** turned on in Settings, the
browser can send files the other way. They are saved in
`Downloads\web-stream` (or the folder set there), linked from the window.

## Where things are kept

| What | Where |
|---|---|
| Program | `C:\Program Files\web-stream` |
| Settings, certificate, paired browsers, logs | `%LOCALAPPDATA%\web-stream` |
| Files received from the browser | `Downloads\web-stream` |

Run the app normally, not as administrator. If starting fails with "access
denied" on the data folder, an older build ran as administrator and left
files only an administrator can open: run **web-stream host** as
administrator once (it repairs the folder for your account), or delete
`%LOCALAPPDATA%\web-stream` and pair again.
