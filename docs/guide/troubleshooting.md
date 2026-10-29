# Troubleshooting

When something goes wrong, the host app's log usually says why. **Open data
folder** in the host app opens the folder with `ws-host-gui.log`; send it to
whoever set you up.

## Connecting

**The page doesn't open away from home.** Run **Test the ports** in the host
app and see [Connecting from outside your home](remote-access.md#test-the-ports).
Some mobile networks and hotel Wi-Fi block unusual ports; Tailscale gets
around that.

**Pairing works but the stream never starts.** The UDP port (8443) isn't
forwarded on the router, or Windows Firewall blocks it on the Public
profile. The stream can fall back to TCP 8444 if that is forwarded.

**The browser warns about the certificate.** Expected the first time with
the host's own certificate; compare the fingerprint the host app shows.
Away from home, use a [Cloudflare Tunnel](cloudflare-tunnel.md), whose
address has a certificate browsers trust.
A *new* warning for a browser that was already paired means the host's
certificate changed, for example after installing the Windows service. If
you don't know why it changed, don't click past it.

**The host doesn't start: "access denied" on the data folder.** See
[Where things are kept](host-app.md#where-things-are-kept).

## Picture and input

**The stream freezes at the sign-in screen, a UAC prompt or the lock
screen.** Windows keeps normal programs out of those. Install the
[Windows service](windows-service.md).

**Clicks and keys don't reach a program running as administrator.** Same
cause and fix as above.

**The picture is soft or stutters.** Try **Auto-tune (test)** in the
browser's settings, or lower the resolution or frame rate. The stats overlay
(Ctrl+Alt+Shift+S) names the problem: a slow or lossy connection, slow
decoding on the device, or slow encoding on the PC.

**Esc doesn't reach the host.** In a window the browser keeps Esc. Use
fullscreen (the button, not F11), or Ctrl+Alt+Shift+E.

## Audio and controllers

**The browser's mic doesn't reach the PC.** Install Steam (it brings the
microphone driver) and run the installer again, or install
[VB-CABLE](https://vb-audio.com/Cable/). See
[Microphone](audio-and-gamepads.md#microphone).

**The browser hears its own mic.** A virtual mic cable is set as the PC's
default output. Set your speakers or headphones as the default again.

**Controllers don't work in games.** Run the installer again with the
gamepad driver ticked. See [Controllers](audio-and-gamepads.md#controllers-vigembus).

## Installing

**Windows or the browser blocks the download.** The program isn't signed
by a company; see [Getting started](getting-started.md#1-install-the-host)
for the clicks.

**The antivirus removes `ws-host-gui.exe`.** A false alarm on a new,
unsigned program. Restore it and add an exclusion for
`C:\Program Files\web-stream`.
