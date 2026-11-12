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

## 2. Pair your phone

The **web-stream host** window opens by itself the first time, with a QR
code already showing. The app also lives in the system tray (bottom right,
near the clock); closing the window keeps it running there.

1. Scan the QR code with the phone's camera and open the link. (Later, press
   **Pair a device** for a new one.)
2. The first time, the phone warns that the connection isn't private. That
   is expected: the host uses its own certificate. It does not happen
   through a [Cloudflare Tunnel](cloudflare-tunnel.md).
   - **iPhone:** **Show Details > visit this website > Visit Website**.
   - **Chrome:** **Advanced > Proceed**.
   - Only if you want to check: tap the warning's certificate details. The
     host window shows the first and last 8 hex digits of the certificate's
     fingerprint under the QR code.
3. The page pairs by itself and connects.

On iPhone, **Share > Add to Home Screen** puts a web-stream icon on the
home screen. It opens full screen and connects straight away, because the
paired browser connects automatically (turn that off under **Settings >
Host > Connect automatically** on the page). In Chrome on Android,
**Install app** does the same. If the home-screen icon asks to pair again,
iOS kept its cookies apart from Safari: open the QR link from the icon
itself, or pair once more. Each host you use needs its own icon.

**Using it away from home too?** If you were sent a Cloudflare Tunnel token,
open **Settings**, set **Cloudflare Tunnel** to **Named (token)**, paste it
in **Tunnel token** and save: the window then shows your own address (like
`https://alex.example.com/`). Then follow
[Connecting from outside your home](remote-access.md) for the stream's
ports, and come back.

The QR code works once, for two minutes. Press **Pair a device** again for
each extra device. **Forget paired browsers** in the window unpairs all of
them.

**On a laptop, or without a camera:** open the address shown in the host
window, press **Request code** and type the 6-digit code the host window
shows.

## 3. Away from home (optional)

At home on the same Wi-Fi nothing more is needed. From elsewhere, open
**Remote access (away from home)** in the host window. If you were sent a
Cloudflare Tunnel token, open **Settings**, set **Cloudflare Tunnel** to
**Named (token)**, paste it in **Tunnel token** and save: the window then
shows your own address (like `https://alex.example.com/`). Then follow
[Connecting from outside your home](remote-access.md) for the stream's
ports.

## If someone sent you a link

You need nothing installed. Open the link, accept the certificate warning
described above (once), and ask whoever runs the PC to press **Pair a
device** and show you the QR code or the 6-digit code. After that, open the
same link or the home-screen icon any time. If the warning ever comes back
for a browser that was already paired, don't click past it unless the host
owner says the PC's certificate changed.

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
