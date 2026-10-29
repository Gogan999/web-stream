# Connecting from outside your home

At home, a phone or computer on the same Wi-Fi reaches the PC directly.
From anywhere else, two things are needed:

- **The web page.** The easiest way is a
  [Cloudflare Tunnel](cloudflare-tunnel.md): the page, pairing and
  signaling come through Cloudflare with a certificate browsers trust, and
  nothing needs forwarding for them. Without a tunnel, the router forwards
  TCP 8443 to the PC instead, and browsers warn about the host's own
  certificate.
- **The stream.** Video, audio and input always go straight to the PC, so
  the router passes these ports to it:

| Port | Protocol | Used for |
|---|---|---|
| 8443 | UDP | The stream: video, audio and input |
| 8444 | TCP | Fallback for networks that block UDP |
| 8443 | TCP | The web page, pairing and signaling (HTTPS); only without a tunnel |

## Forward the ports

The first-run setup in the host app (also under **Setup** at the bottom of
the window) does this for you where it can.

- **Automatically (UPnP):** tick **Let the router forward them (UPnP)**. If
  it says the router forwards the ports, you're done. Only these ports go
  to this PC (with a tunnel on, not TCP 8443). The forwards are leased for an hour and renewed
  while the app runs, and removed when you quit or uninstall it (or within
  the hour if the PC is switched off). A router that only accepts permanent
  forwards is refused.
- **By hand:** if no router answers (UPnP is off on many routers), open the
  router's settings page and look for "Port forwarding", "Virtual server"
  or "NAT". Add the ports above, each to the same port on this PC's
  address (the window shows it, for example `192.168.1.20`). Your router
  maker's website has instructions for your model.

Windows Firewall also has to allow the host on the **Public** profile as
well as Private. The installer adds these rules; if you run a build from
source, allow `ws-host-gui` when Windows asks. A firewall block can look
like a network problem, because connecting from the PC itself still works.

## Test the ports

Press **Test the ports** in the host app. With a tunnel on, it first says
whether the tunnel is up. It learns the public address (from the router or
the address typed in Settings) and connects to it from this PC. Without a
tunnel it checks that the certificate answering on TCP 8443 is this host's
and not, say, the router's admin page.

- **✔** means a phone away from home can reach this PC.
- **"not a public address" / CGNAT:** your internet provider shares one
  address between several customers, so port forwarding can't work. Ask
  them for a public IPv4 address (some do it for free), or install
  [Tailscale](https://tailscale.com/download) on both the PC and the phone
  and open the PC's Tailscale address.
- **"no answer from inside your network":** many routers can't test
  themselves. Do the real test instead: turn Wi-Fi off on your phone and
  open the address the test shows. If the page opens, it works. To pair a
  phone that way, type the address under **Settings > Public addresses**
  so the QR code can use it.

UDP can't be tested from inside your network; if pairing works but the
stream never starts, the UDP port isn't forwarded or the firewall blocks
it.

The public address only goes into the pairing QR code once the test has
found this host there, or when you type it in Settings. The router's own
report isn't trusted, since any device on the network can answer UPnP.

## A trusted certificate

Out of the box the host makes its own (self-signed) certificate, and each
browser warns about it once. That is fine at home, where you can compare
the fingerprint the host app shows with the browser's. Away from home it is
a risk: on a network you don't control, someone could show the browser a
different certificate, and clicking past that warning would let them pair.
Through a [Cloudflare Tunnel](cloudflare-tunnel.md) there is no warning to
click past, and the address stays the same when your home IP changes. A
certificate from elsewhere can be passed with `--cert` and `--key` under
**More options** (see the [host options reference](../dev/host-options.md)).

## How the browser finds the stream

The host advertises the address in the browser's address bar (resolving a
DNS name) as a media address, alongside its LAN addresses. Through a
Cloudflare Tunnel that name is Cloudflare's, so the host offers the public
address it learns from Cloudflare's STUN server instead. If browsers
reach the host by some other name, add its public address under
**Settings > Public addresses** (`--public-ip`). Behind a reverse proxy
that rewrites the Host header, pass the proxy's address with
`--allow-origin https://name`, or the browser's requests are refused as
cross-site.
