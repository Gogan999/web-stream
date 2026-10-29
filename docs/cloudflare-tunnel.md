# Cloudflare Tunnel

A Cloudflare Tunnel serves the web page from Cloudflare, with Cloudflare's
certificate, so browsers never show a certificate warning and the HTTPS port
(TCP 8443) needs no forwarding. The host runs `cloudflared` (installed next
to it), which connects out to Cloudflare and passes the page, pairing and
signaling back to the host on this PC.

Video, game audio, the mic and input do **not** go through the tunnel:
Cloudflare Tunnel only carries HTTP and WebSockets, and streaming over TCP
can't drop late frames (see `tunnel/evaluation.md` in the project notes).
They still go straight to this PC, so the router still forwards:

- **UDP 8443**: the stream.
- **TCP 8444**: the fallback for networks that block UDP (optional, but
  some hotel, school and phone networks need it).

The host learns its public address from Cloudflare's STUN server
(`stun.cloudflare.com`) every five minutes and offers it to the browser for
the stream, since the tunnel's address is Cloudflare's, not this PC's.

## Two kinds of tunnel

| | Named | Quick |
|---|---|---|
| Account | A Cloudflare account with a domain on it | None |
| Address | Fixed, e.g. `https://alex.example.com` | New random `https://….trycloudflare.com` on every start |
| Pairing | Browsers stay paired | Browsers pair again after every host restart (the address changes) |
| Setup | A token pasted once | Nothing |
| Cloudflare's promise | A normal service | For testing only, no uptime promise |

Pick **Settings > Cloudflare Tunnel** in the tray app (flags: `--tunnel
named` or `--tunnel quick`).

## Setting up a named tunnel (once per PC)

Whoever owns the Cloudflare domain does this, for each PC that hosts:

1. In the [Cloudflare dashboard](https://dash.cloudflare.com), open
   **Zero Trust > Networks > Tunnels** and **Create a tunnel**. Choose
   **Cloudflared**, and name it after the PC.
2. On the install step, copy the token: the long string after
   `cloudflared service install` (or `--token`). Don't run the install
   command; the host runs cloudflared itself.
3. Under **Public hostnames**, add one:
   - **Subdomain / domain**: e.g. `alex` on `example.com`.
   - **Service**: type `HTTPS`, URL `localhost:8443` (the host's port).
   - **Additional application settings > TLS > No TLS Verify**: on. The
     host's own certificate is self-signed, and cloudflared only reaches it
     on the same PC.
4. Send the token to the PC's owner privately (it lets anyone run that
   tunnel), together with the address.

On the PC, in the tray app's **Settings**:

1. **Cloudflare Tunnel**: Named (token).
2. **Tunnel token**: paste it. It is saved in its own file in the data
   folder (`cloudflare-tunnel-token.txt`, readable only by this user), never
   in the settings file; the service gets its own copy when installed.
3. **Tunnel address** (optional): e.g. `alex.example.com`. Left empty, the
   host uses the name cloudflared reports from the dashboard.
4. Save. Once cloudflared connects, the window shows the address under
   **Open in the browser**, and the pairing QR code uses it.

Forward UDP 8443 and TCP 8444 on the router (or tick UPnP, which then asks
only for those two).

## Things to know

- Cloudflare decrypts the page and signaling at its edge, like any site
  behind it, so it could in principle see pairing codes and the offer. The
  stream itself is end-to-end encrypted (DTLS-SRTP) between the browser and
  this PC. If that trade isn't acceptable, leave the tunnel off and use the
  self-signed certificate at home, or Tailscale.
- `cloudflared` is restarted when it stops (waiting 2 s, doubling to a
  minute). Its errors show in the tray app's log.
- The host still listens on TCP 8443 for browsers on the same network
  (with the self-signed certificate). It no longer needs to be forwarded.
- Requests through the tunnel all come from `127.0.0.1`, so the per-address
  connection cap doesn't apply to loopback; the overall cap (64) still does.
- The service only runs the `cloudflared.exe` in Program Files
  (`--cloudflared` is refused there).
- The DuckDNS / Let's Encrypt certificate this replaces is gone. A service
  installed with the old `--duckdns` flags still starts (they are dropped
  with a warning); turn the tunnel on in Settings and reinstall the service
  to use it.
