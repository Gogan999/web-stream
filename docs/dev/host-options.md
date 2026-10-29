# Host options reference

The host app's settings window covers the common options. Anything else
goes in **Settings > More options** as command-line flags (quote paths that
contain spaces). This page lists every flag.

By default the host serves the web client built into it on TCP 8443, with
WebRTC media on UDP 8443 and the ICE-TCP fallback on TCP 8444, and keeps its
certificate and paired browsers in `%LOCALAPPDATA%\web-stream`.

## Network

| Flag | Meaning |
|---|---|
| `--port P` | HTTPS (TCP) and media (UDP) port. Default 8443 |
| `--ice-tcp-port P` | ICE-TCP fallback port. Default `port + 1` |
| `--no-ice-tcp` | Don't listen for ICE-TCP |
| `--ip IP` | Local address for media (repeatable). Default: every LAN IPv4 address |
| `--public-ip IP` | Public address that forwards to the host (repeatable) |
| `--allow-origin URL` | Extra origin allowed to use the API (repeatable), for a reverse proxy that rewrites the Host header |

## Certificates

| Flag | Meaning |
|---|---|
| `--hostname NAME` | Extra name or IP in the self-signed certificate (repeatable) |
| `--cert FILE --key FILE` | Use this PEM certificate and key instead of a self-signed one |

## Cloudflare Tunnel

See [Cloudflare Tunnel](../guide/cloudflare-tunnel.md).

| Flag | Meaning |
|---|---|
| `--tunnel named\|quick` | Serve the page, pairing and signaling through a Cloudflare Tunnel run by `cloudflared`. `named`: a tunnel made in the Cloudflare dashboard. `quick`: no account, a new trycloudflare.com name on every start |
| `--tunnel-token-file FILE` | The named tunnel's token. Default: `cloudflare-tunnel-token.txt` in the data folder |
| `--tunnel-hostname NAME` | The named tunnel's public name. Default: the one cloudflared reports |
| `--cloudflared FILE` | The cloudflared program. Default: `cloudflared.exe` next to the host (refused for the service) |

The old `--duckdns`, `--duckdns-token-file`, `--no-duckdns-address`,
`--acme-email` and `--acme-staging` flags are gone; a service installed
with them drops them with a warning.

## Pairing and data

| Flag | Meaning |
|---|---|
| `--pair` | Open pairing as soon as the host starts |
| `--data-dir DIR` | Folder for the certificate and paired browsers |
| `--forget-after DAYS` | Unpair a browser that hasn't connected for this long. Default 30 |
| `--web-root DIR` | Serve the web client from `DIR` instead of the built-in one (for web development) |

## Video

| Flag | Meaning |
|---|---|
| `--display N` | Display to stream first |
| `--codec h264\|h265\|av1` | Codec, unless the browser picks one. Falls back to H.264 when unsupported |
| `--size WxH` | Stream size |
| `--fps F` | Frame rate |
| `--kbps K` | Bitrate cap; the encoder follows the bandwidth estimate below it |
| `--quality speed\|balanced\|clarity` | Encoder effort per frame (see below). Default `balanced` |
| `--allow-resolution-change` | Let the browser switch the streamed display's resolution (temporarily) |

`--quality` maps to NVENC presets. `balanced` uses P4 up to 1080p60 and P3
up to 1440p60, stepping down above that: a few milliseconds more encode time
than `speed` (P1) for a much sharper picture in fast motion at the same
bitrate. `clarity` gives the most detail per bit.

## Audio and input

| Flag | Meaning |
|---|---|
| `--no-audio` | Don't capture game audio |
| `--audio-device NAME` | Capture this output device instead of the default one |
| `--audio-kbps K` | Opus bitrate, 32 to 510. Default 128 |
| `--audio-frame-ms 10\|20` | Opus frame length. Default 10 |
| `--mic-device NAME` | Playback device the browser mic plays into (part of its name). Default `auto` |
| `--no-mic` | Refuse the browser mic |
| `--gamepad xbox360\|ds4` | Virtual controller type. Default `xbox360` |

## Permissions

| Flag | Meaning |
|---|---|
| `--allow-upload` | Let the browser send files to this PC |
| `--upload-dir DIR` | Where received files go. Default `Downloads\web-stream` |
| `--allow-remote-update` | Let the browser run `scripts\update.ps1` (source installs; see [Building from source](building.md#updating-from-the-browser)) |

The Windows service refuses `--web-root`, `--data-dir`, `--allow-upload` and
`--allow-remote-update`; see [Security](../guide/windows-service.md#security).

## Session lifetime

Capture, game audio, the mic and input start when a browser connects and
stop when it leaves. Ending a session releases every held key and unplugs
the virtual gamepads. A resolution changed with `--allow-resolution-change`
is restored on disconnect, when the host quits, and after a crash in the
media code; if the process is killed, Windows should revert it too (not
documented by Microsoft), and a reboot always does.
