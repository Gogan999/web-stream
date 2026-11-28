# Linux host

On Linux the host runs headless as a systemd service: there is no tray app.
You pair devices and change settings from a terminal or from the host's own
web page.

**You need:** a 64-bit PC (x86_64 or aarch64) with systemd and glibc 2.35 or
newer (Ubuntu 22.04, Debian 12, Fedora 36 or later), and a GPU the host can
capture and encode with. GPU libraries are loaded when the host starts, so
the program installs and runs (with signaling, pairing and the settings page)
even where capture is not available yet.

## Install

In a terminal on the PC:

```
curl -fsSL https://github.com/Gogan999/web-stream/releases/latest/download/web-stream-install.sh | sh
```

It asks for your password through `sudo` when you are not root. The script:

1. finds the latest release in its `manifest.json` and downloads the tarball
   for this PC's architecture;
2. checks its size and SHA-256 against the manifest;
3. installs `web-stream` and `cloudflared` to `/usr/local/lib/web-stream`
   (and links `/usr/local/bin/web-stream`);
4. runs `web-stream service install`, which creates the `web-stream` system
   user, a udev rule and a `modules-load.d` entry for `/dev/uinput`, the
   `web-stream.service` unit, and starts the service.

**Updating:** run the same line again. It stops the service, replaces the
program and starts it again; when the installed version is already the
latest it does nothing. `WEB_STREAM_REINSTALL=1` installs anyway.

**Uninstalling:**

```
curl -fsSL https://github.com/Gogan999/web-stream/releases/latest/download/web-stream-install.sh | sh -s -- --uninstall
```

This removes the program, the unit, the udev rule and the user, and keeps
`/var/lib/web-stream` (the certificate, paired devices and settings) so a
reinstall keeps working with paired devices. Add `--purge` to remove that
too.

**From source:** `cargo build --release -p ws-host --bin web-stream`, then
`sudo target/release/web-stream service install` copies the program to
`/usr/local/lib/web-stream` and does step 4.

## Pair a device

```
sudo web-stream pair
```

opens pairing for two minutes and prints the addresses with the pairing link,
a QR code to scan with a phone, and, when a browser asks for one, the code to
type. It returns once a device has paired.

The first time, the browser warns that the connection isn't private: the
host uses its own certificate. See
[Getting started](getting-started.md#2-pair-your-phone).

`sudo web-stream unpair-all` forgets every paired device.

## The settings page

```
sudo web-stream settings
```

prints `https://localhost:8443/admin#key=…`. Open it in a browser on the PC.
The key works like a password: anyone on this PC with it can pair devices,
so it is readable only by root. The page shows:

- **Status:** the addresses, the certificate fingerprint, the Cloudflare
  Tunnel, the connected device and UPnP;
- **Pair a device:** opens pairing and shows a QR code for each address and
  the code a browser asks for;
- **Paired devices,** with **Unpair** for each;
- **Settings:** ports, extra UDP ports, public addresses and host names,
  other allowed origins, UPnP, the Cloudflare Tunnel (named with its token, or
  quick), when unused devices are unpaired, the video (display, codec, size,
  frame rate, bitrate, encoder effort), game audio and its device, the
  microphone device, the gamepad type, the clipboard and receiving files.

**Save and restart** saves the settings to `/var/lib/web-stream/settings.json`
and restarts both halves of the host; a connected browser reconnects by
itself.

From another computer, either forward the port over SSH and open the same
address there:

```
ssh -L 8443:localhost:8443 you@the-pc
```

or pair the computer first and open `https://<the PC>:8443/admin` on it: a
paired device can open the settings after confirming. Turn **Paired devices
may open these settings** off to allow only the PC itself.

## Headless PCs

The host captures what the GPU sends to a display. A PC without a monitor
usually has no display at all, so plug in an **HDMI (or DisplayPort) dummy
plug**: a cheap adapter that tells the GPU a monitor is there. Pick one that
offers the resolution you want to stream (4K plugs also offer 1080p).

Set the desktop to sign in automatically if you want to reach it after a
reboot without a monitor: the host streams whatever is on the screen,
including the login screen.

## Matching the host resolution

With **Let the browser change the resolution** on, the browser's **Match
host resolution** switches the streamed display to the largest mode that
fits its screen, as on Windows. The host asks the desktop, as the signed-in
user, through whichever it offers: GNOME (Mutter, as a temporary
configuration), KDE Plasma (`kscreen-doctor`), wlroots compositors such as
sway, Hyprland or labwc (output management), or X11 RandR. Displays to the
right of or below the changed one move with its edge. The original mode and
layout come back when the browser asks, disconnects, or the host stops; a
`HEADLESS-*` output of a wlroots compositor takes the browser's exact size.
Without one of these (or with nobody signed in) the browser is told the
resolution can't be changed.

## Running in the foreground

```
web-stream serve [--upnp] [host options]
```

runs the whole host in one process as you, with the settings saved in
`~/.local/share/web-stream` and the [host options](../dev/host-options.md)
given. It prints its settings page address and takes `pair`, `unpair-all`,
`drop` and `quit` on its console. Use it for trying things out; the service
is what keeps the network-facing part unprivileged.

## Security

Whoever is paired with the host can do anything on the PC that the person at
its keyboard can. The service keeps that power away from everyone else:

- **Two processes.** The service starts as root and runs capture, encoding,
  input (`/dev/uinput`) and audio. It starts everything that reads the
  network (HTTPS and TLS, signaling, the peer connection's DTLS, SRTP and data
  channels, the settings page, cloudflared, UPnP) as a child process running
  as the `web-stream` system user, with no supplementary groups, no
  capabilities and `no_new_privs`, connected to the root side by a socket
  pair it inherits. That user can write only its own folder,
  `/var/lib/web-stream/network` (the certificate, paired devices and the
  settings key).
- **The root side accepts only what a browser could send**: input, control
  messages, files it asked for, size-limited and checked, and settings that
  it checks itself. The settings cannot name a web root, a data folder, a
  certificate file, another cloudflared or an upload folder. Received files
  go to the signed-in user's `~/Downloads/web-stream`, written as that user
  (never as root), or to `/var/lib/web-stream/uploads` when nobody is signed
  in.
- **Pairing needs root.** `web-stream pair` talks to the service over
  `/run/web-stream/control.sock`, which only root can open. The settings page
  opens either with the key from `web-stream settings` (also root only),
  which works only from the PC itself and never through the Cloudflare
  Tunnel, or for a paired device that confirms.
- **The unit is hardened:** `NoNewPrivileges`, `ProtectSystem=strict` with
  only `/var/lib/web-stream` and `/home` writable (received files, written
  as the signed-in user), no kernel tunables, modules or logs, a system call filter, only the DRM,
  NVIDIA, ALSA and uinput devices, and the capability bounding set
  `CAP_SYS_ADMIN` (reading the screen through KMS without being DRM master),
  `CAP_SYS_NICE` (capture thread priority), `CAP_SETUID`, `CAP_SETGID` and
  `CAP_CHOWN` (starting the network side and the clipboard and display
  helpers as their users, writing received files as the signed-in user, handing the network
  side its folder) and `CAP_DAC_READ_SEARCH` (reaching the signed-in user's
  audio and display sockets). `systemd-analyze security web-stream` rates it about 3.7.
- **Restarts:** a network side that crashes is started again after 1 s,
  doubling to a minute while it keeps failing; systemd restarts the service
  the same way.

## Troubleshooting

- **Logs:** `journalctl -u web-stream -f`. `sudo web-stream service status`
  shows whether it runs.
- **The page doesn't open:** check the service runs and nothing else uses
  port 8443 (`sudo ss -ltnp | grep 8443`). A firewall must let TCP 8443 and
  8444 and UDP 8443 in, for example `sudo ufw allow 8443:8444/tcp` and
  `sudo ufw allow 8443/udp`.
- **"no capture device" in the log:** the GPU sends nothing to a display.
  Plug in a monitor or a dummy plug and check the desktop is on it.
- **Keyboard, mouse or gamepads do nothing:** `ls -l /dev/uinput` must exist;
  `sudo modprobe uinput` loads it (the installer sets it to load at boot).
- **Something needs more access than the unit allows:** add it with
  `sudo systemctl edit web-stream`, for example
  `CapabilityBoundingSet=CAP_DAC_OVERRIDE` or `DeviceAllow=/dev/… rw`
  under `[Service]`, then `sudo systemctl restart web-stream`, and report
  it so the unit can allow it by default.
- **The settings key leaked:** delete `/var/lib/web-stream/network/admin-token`
  and restart the service; it makes a new one, and pages signed in with the
  old one are signed out.
- **Start over:** uninstall with `--purge` and install again; every device
  has to pair again.
