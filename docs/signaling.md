# Signaling, pairing and transport contract

The host agent embeds everything the browser talks to: one HTTPS listener
serves the web client, the pairing API and the signaling WebSocket, and the
host itself is the WebRTC endpoint (ICE-lite), so no separate signaling or
TURN server is needed.

## Ports (defaults, all configurable)

| Port      | Proto | Use                                              |
|-----------|-------|--------------------------------------------------|
| 8443      | TCP   | HTTPS: web client, `/api/*`, `/ws`               |
| 8443      | UDP   | WebRTC media and data channels (all sessions)    |
| 8444      | TCP   | ICE-TCP fallback for networks that block UDP     |

For access from the internet, forward these three ports to the host, to the
same port numbers. Each offer carries host candidates for the LAN
addresses, for every `--public-ip`, and for the address the browser used to
reach the host (the `Host` header of its `/ws` request, resolved if it is a
DDNS name). Through a Cloudflare Tunnel (a `/ws` request carrying
`CF-Connecting-IP`) that name is Cloudflare's, so it is left out and the
public address the host learns over STUN is offered instead; TCP 8443 then
needs no forward. The public ones are advertised as `typ host`, not `srflx`,
because str0m drops non-host local candidates in ICE-lite mode. Checks sent
to them arrive through the port forward on the LAN address and are answered
from there.

## TLS

The host generates a self-signed certificate on first run and prints its
SHA-256 fingerprint; compare it once when the browser warns. A real
certificate (for example from a DDNS name) can be configured instead.

## Pairing and auth (HTTP, same origin)

Pairing is closed by default. The host user opens it from the host (tray or
console, `SignalingHandle::open_pairing`) for two minutes; only then does
`/api/pair/start` work, and a successful pairing closes it again. Nobody on
the network can start a pairing or guess codes at an unattended host.

All `POST` bodies are JSON and must send `Content-Type: application/json`.
The host rejects any request whose `Origin` header is not the host's own
origin. The device credential is an `HttpOnly; Secure; SameSite=Strict`
cookie named `__Host-ws_device`, so page script never sees it and no
sibling subdomain can set it.

| Request | Body | Responses |
|---------|------|-----------|
| `GET /api/session` | | `200 {"paired":true,"device":"<name>"}` or `401 {"paired":false}` |
| `POST /api/pair/start` | `{"device_name":"Chrome on laptop"}` | `200 {"pairing_id":"…","expires_in":120}`; host shows a 6-digit code. `403 {"error":"pairing_closed"}` until the host user opens pairing. `423 {"error":"locked"}`. `409 {"error":"busy"}` while another browser's code is pending (the first request keeps it until it is confirmed, burned or expires). `429 {"error":"rate_limited"}` |
| `POST /api/pair/confirm` | `{"pairing_id":"…","code":"123456"}` | `200 {"paired":true}` + cookie. `403 {"error":"bad_code","attempts_left":2}`. `410 {"error":"expired"}`. `423 {"error":"locked"}` |
| `POST /api/pair/link` | `{"link":"…","device_name":"iPhone"}` | `200 {"paired":true}` + cookie. `403 {"error":"pairing_closed"}` if pairing is closed or the link is wrong or used |
| `POST /api/unpair` | `{}` | `200`, revokes this device and clears the cookie |

Opening pairing also makes a pairing link: 32 random bytes the host UI
shows as a QR code of `https://<host>:<port>/#pair=<link>`. The fragment
never reaches a server; the web client takes it off the address, posts it
to `/api/pair/link` and connects. It pairs one browser, once, while pairing
is open; a used link, a confirmed code or the window closing ends it. A
wrong link doesn't count towards the lockout (it can't be guessed), and a
lockout from wrong codes stops codes but not the link, so a stranger
guessing codes can't take the QR code away from the host user. The link is
never logged.

A code is valid for 120 seconds and 3 attempts. After 5 wrong codes in
total, pairing locks (also across restarts) until the host user opens it
again. A paired device that is unused for 30 days is revoked.

## Limits

At most 64 connections at once and 8 per source IP. TLS handshakes must
finish in 10 s, request headers in 10 s (which also closes idle keep-alive
connections), and whole requests in 10 s. HTTP/1.1 only. API bodies are
capped at 4 KiB and WebSocket messages and frames at 64 KiB. The host pings
the signaling socket every 15 s and drops it after 45 s of silence.

## Signaling WebSocket: `wss://<host>:8443/ws`

Requires the `__Host-ws_device` cookie (sent automatically) and a same-origin
`Origin`. Unauthenticated upgrades get HTTP 401. Only one session is live: a
new connection replaces the old one, which receives `bye` with reason
`replaced`.

Every frame is a JSON text message with a `type` field. The Rust types are
`HostSignal` and `ClientSignal` in `crates/proto/src/signaling.rs`, which is
the source of truth if this page and the code ever disagree.

Host to browser:

```jsonc
{"type":"welcome","ice_servers":[]}          // RTCConfiguration.iceServers; usually empty
{"type":"offer","sdp":"v=0…"}                 // sent right after welcome, and after a restart
{"type":"error","code":"bad_answer","message":"…"}
{"type":"bye","reason":"replaced"}            // or "host_shutdown", "revoked"
```

Browser to host:

```jsonc
{"type":"answer","sdp":"v=0…"}
{"type":"candidate","candidate":"candidate:…","sdpMid":"0","sdpMLineIndex":0}
{"type":"restart"}                            // ask for a fresh offer (for example after ICE failure)
{"type":"handoff"}                            // connected and `control` is open: close this socket
```

### Handoff

The socket is only needed to set up the connection. Once the peer
connection is up and the `control` channel is open, the browser sends
`handoff`; the host closes the socket (code 1000, reason `handoff`), and
from then on only the peer connection carries anything, so the web page is
the one thing outside it. The session stays the live one: a new connection
still replaces it, and an unpair or "drop session" still ends it. The host
then sends `bye` on `control` instead (`{"type":"bye","reason":"replaced"}`,
same reasons) and closes the peer connection a moment later.

Without a socket, recovering from a failed connection takes a new one: the
browser drops the failed peer connection, opens `/ws` again (up to 3 times),
and answers the fresh offer. The host ends a handed-off session whose peer
connection stays down for 20 seconds. So a browser that simply closes its
tab shows as connected on the host until then, where a socket close used
to end the session at once.

The host puts all of its candidates in the offer, so it never trickles.
Browser candidates are optional (the host learns the browser's address from
its connectivity checks) but harmless to send.

## What the offer contains

The host is always the offerer.

- **Video** (`sendonly` from the host): H.264 (packetization-mode 1), with
  NACK, PLI and transport-cc feedback, plus the `playout-delay` header
  extension set to 0/0 so Chromium renders each frame as soon as it decodes.
  The browser should also set `receiver.jitterBufferTarget = 0`.
- **Audio** (`sendrecv`): Opus 48 kHz, game audio from the host and the
  browser microphone back. The web client sets this transceiver's direction
  to `sendrecv` before `createAnswer` and adds or removes the mic later with
  `transceiver.sender.replaceTrack(micTrack | null)`, so no new offer is
  needed.
- **Data channels**, opened by the host (handle them in `ondatachannel` by
  label):

| Label          | Ordered | Reliability          | Use                                   |
|----------------|---------|----------------------|---------------------------------------|
| `control`      | yes     | reliable             | display list/switch, stats, settings  |
| `input`        | yes     | reliable             | key and button presses                |
| `input-motion` | no      | `maxRetransmits: 0`  | mouse motion, gamepad axes            |
| `file`         | yes     | reliable             | file transfer both ways (see `docs/protocol.md`) |

Message formats on these channels belong to the components that use them.

## Frame dropping

Video is never queued behind old data. The host answers a retransmission
request (NACK) only for packets sent within the last 50 ms, so on a slow path
a lost packet is not resent late; the browser asks for a refresh (PLI) and the
encoder sends an intra refresh instead. If the encoder hands the transport a
new frame before the previous one went out, the older one is discarded and a
refresh is requested, since later frames would reference it.

## TURN

The host is itself the media endpoint, so a TURN relay built into the host
would add nothing: any browser that can reach the relay can reach the host's
UDP port directly. Networks that block UDP are covered by ICE-TCP on port
8444 instead. A separate TURN server (for example coturn on a VPS, with
`use-auth-secret`) can still be configured; the host then hands the browser
short-lived credentials for it in `welcome`.
