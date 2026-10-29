# Getting started

This guide puts your Windows PC in a browser on your phone or another
computer: its screen, sound, mouse, keyboard and controllers. It takes about
ten minutes and needs no technical knowledge.

**You need:** a Windows 10 or 11 PC with an NVIDIA graphics card, and a
phone or computer with Chrome, Edge or Safari.

## 1. Install the host

1. Download **web-stream-setup-X.Y.Z.exe** from the
   [latest release](https://github.com/Gogan999/web-stream/releases/latest).
2. Run it. Windows warns you because the program is not signed by a
   company. This is expected:
   - **Edge or Chrome** may say the file "isn't commonly downloaded". Click
     **...** (or the arrow) next to it, then **Keep**, then **Show more**,
     then **Keep anyway**.
   - **"Windows protected your PC"** (SmartScreen): click **More info**,
     then **Run anyway**.
   - Click **Yes** on the administrator prompt.
3. Leave the options as they are and click **Install**. "Allow use at the
   login screen" is only needed to unlock the PC remotely; you can turn it
   on later from the app's settings (see
   [Sign-in screen and administrator programs](windows-service.md)).

If your antivirus removes or blocks `ws-host-gui.exe`, it is a false alarm
on a new, unsigned program. Restore it from the antivirus quarantine and add
an exception for `C:\Program Files\web-stream`. In Windows Security that is
**Virus & threat protection > Manage settings > Exclusions > Add an
exclusion > Folder**.

## 2. First-run setup

The **web-stream host** window opens on **Set up this PC**. The app also
lives in the system tray (bottom right, near the clock); closing the window
keeps it running there.

- **Only using it at home?** Skip to step 3.
- **Using it away from home too?** If you were sent a Cloudflare Tunnel
  token, open **Settings**, set **Cloudflare Tunnel** to **Named (token)**,
  paste it in **Tunnel token** and save: the window then shows your own
  address (like `https://alex.example.com/`). Then follow
  [Connecting from outside your home](remote-access.md) for the stream's
  ports, and come back.

## 3. Pair your phone

1. Press **Pair a phone**. A QR code appears.
2. Scan it with the phone's camera and open the link.
3. The first time, the browser warns about the certificate (not through a
   [Cloudflare Tunnel](cloudflare-tunnel.md)).
   - **iPhone:** **Show Details > visit this website > Visit Website**.
   - **Chrome:** **Advanced > Proceed**.
4. The page pairs by itself and connects.

On iPhone, **Share > Add to Home Screen** keeps it one tap away.

The QR code works once, for two minutes. Press **Pair a phone** again for
each extra device. **Forget paired browsers** in the window unpairs all of
them.

## Next steps

- [Using the browser client](browser-client.md): controls, hotkeys, phones
  and tablets.
- [The host app](host-app.md): settings, sharing files, pairing.
- [Microphone, game audio and controllers](audio-and-gamepads.md).
- [Troubleshooting](troubleshooting.md) if something doesn't work.

## Uninstalling

**Settings > Apps > web-stream host > Uninstall**. This removes the program,
its firewall rules, any router forwards it made, the Windows service and
the start-with-Windows entry. It asks whether to delete your settings and
paired devices too. The gamepad and microphone drivers stay installed;
remove them from **Apps** as well if nothing else uses them.
