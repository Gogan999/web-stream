# Hosting web-stream on your PC

This puts your Windows PC on your phone or another computer's browser:
the screen, sound, mouse, keyboard and controllers. About 10 minutes.

## 1. Download and install

1. Download **web-stream-setup-X.Y.Z.exe** from the
   [latest release](https://github.com/Gogan999/web-stream/releases/latest).
2. Run it. Windows warns because the program isn't signed by a company:
   - **Edge or Chrome** may say the file "isn't commonly downloaded". Click
     **...** (or the arrow) next to it, then **Keep**, then **Show more**,
     then **Keep anyway**.
   - **"Windows protected your PC"** (SmartScreen): click **More info**, then
     **Run anyway**.
   - Then **Yes** on the administrator prompt.
3. Leave the checkboxes as they are unless you know you want otherwise, and
   click **Install**. "Allow use at the login screen" is only needed to
   unlock the PC remotely; you can add it later from the app's settings.

If your antivirus removes or blocks `ws-host-gui.exe`, that is a false
alarm on a new, unsigned program: restore it from the antivirus's
quarantine and add an exception for `C:\Program Files\web-stream`. In
Windows Security that is **Virus & threat protection > Manage settings >
Exclusions > Add an exclusion > Folder**.

## 2. First-run setup

The **web-stream host** window opens on "Set up this PC". It also sits in
the tray (bottom right, near the clock); closing the window keeps it
running there.

**At home only?** Skip to "Pair your phone".

**Away from home too?** Your router has to pass three ports to this PC:
TCP 8443, UDP 8443 and TCP 8444.

- Tick **Let the router forward them (UPnP)**. If it says the router
  forwards the ports, you're done. Only those three ports go to this PC,
  and they close again when you quit the app or uninstall it.
- If no router answers (UPnP is off on many routers), forward them on the
  router's own settings page instead: look for "Port forwarding",
  "Virtual server" or "NAT", and add the three ports above, each to the
  same port on this PC's address (the window shows it, e.g.
  192.168.1.20). Your router maker's site has a page on it.

Then press **Test the ports**:

- ✔ means a phone away from home can reach this PC.
- **"not a public address" / CGNAT**: your internet provider shares one
  address between customers, and forwarding can't work. Ask them for a
  public IPv4 address (some do it for free), or use
  [Tailscale](https://tailscale.com/download) on both the PC and the phone
  and open the PC's Tailscale address.
- **"no answer from inside your network"**: many routers can't test
  themselves. Do the real test: turn Wi-Fi off on your phone, pair it (next
  step) with the public address chosen under the QR code, and see whether
  the page opens.

Optional: a free name from [duckdns.org](https://www.duckdns.org) (sign in,
make a subdomain, copy the token) goes in **Settings > DuckDNS name** and
**DuckDNS token**. The host then keeps the name pointing at your home and
gets a proper certificate, so browsers stop warning and the address
doesn't change.

## 3. Pair your phone

1. Press **Pair a phone**. A QR code appears.
2. Scan it with the phone's camera and open the link.
3. The browser warns about the certificate the first time (unless you set
   up DuckDNS). On iPhone: **Show Details > visit this website > Visit
   Website**. On Chrome: **Advanced > Proceed**.
4. The page pairs by itself and connects. On iPhone, **Share > Add to Home
   Screen** keeps it one tap away.

The QR code works once, for two minutes; press **Pair a phone** again for
another device. **Forget paired browsers** in the window unpairs them all.

## Problems

- **The page doesn't open away from home**: run **Test the ports** again
  and see above. Some mobile networks and hotel Wi-Fi block unusual ports;
  Tailscale gets round that.
- **No sound from the browser's mic on the PC**: install Steam (it brings
  the microphone driver) and run the installer again, or install
  [VB-CABLE](https://vb-audio.com/Cable/).
- **Controllers don't work in games**: run the installer again with the
  gamepad driver ticked.
- **Something else**: **Open data folder** in the window has
  `ws-host-gui.log`; send it to whoever set you up.

## Uninstalling

Settings > Apps > **web-stream host** > Uninstall. It removes the program,
the firewall rules, any router forwards it made, the service and starting
with Windows, and asks whether to delete your settings and paired devices.
The gamepad and microphone drivers stay; uninstall them from Apps too if
nothing else uses them.
