# web-stream

Low-latency remote access to a Windows PC from a web browser. Stream the
desktop to a phone, tablet or another computer and use it with mouse,
keyboard, touch and game controllers, with nothing to install on the device
you connect from.

A small host program on the PC captures the screen, encodes it on the GPU
and streams it over WebRTC. The host serves the web client, pairing and
signaling itself, so there is no separate server to run.

## Features

- **Fast enough for games.** Hardware encoding (NVENC: H.264, H.265, AV1),
  late frames dropped rather than queued, mouse motion on an unreliable
  channel.
- **Audio both ways.** Game audio to the browser; the browser's microphone
  to the PC as a virtual microphone.
- **Controllers.** Browser gamepads appear as Xbox 360 or DualShock 4
  controllers, with rumble.
- **Multiple displays.** Switch between the PC's displays from the browser.
- **File transfer.** Send files from the PC to the browser, and optionally
  back.
- **Phones and tablets.** Trackpad and direct-touch modes, pinch zoom,
  on-screen keyboard and extra keys.
- **Security.** Devices pair with a one-time code or QR code shown on the
  PC; nothing on the network can start pairing unattended. An optional
  Windows service reaches the sign-in screen and UAC prompts, with the
  network-facing part running unprivileged.

## Requirements

- **Host:** Windows 10 or 11 with an NVIDIA GPU.
- **Client:** a recent Chrome or Edge (desktop or Android), or Safari on
  iPhone and iPad.

## Get started

Download **web-stream-setup-X.Y.Z.exe** from the
[latest release](https://github.com/Gogan999/web-stream/releases/latest)
and follow [Getting started](docs/guide/getting-started.md).

## Documentation

**Using web-stream**

- [Getting started](docs/guide/getting-started.md): install, set up and
  pair a phone
- [Connecting from outside your home](docs/guide/remote-access.md)
- [Cloudflare Tunnel](docs/guide/cloudflare-tunnel.md)
- [Using the browser client](docs/guide/browser-client.md)
- [The host app](docs/guide/host-app.md)
- [Microphone, game audio and controllers](docs/guide/audio-and-gamepads.md)
- [Sign-in screen and administrator programs](docs/guide/windows-service.md)
- [Troubleshooting](docs/guide/troubleshooting.md)

**Developing web-stream**

- [Architecture](docs/dev/architecture.md)
- [Building from source](docs/dev/building.md)
- [Host options reference](docs/dev/host-options.md)
- [Signaling, pairing and transport](docs/dev/signaling.md)
- [Client and host wire protocol](docs/dev/protocol.md)
- [Browser client development](web/README.md)
- [Diagnostic logging](docs/dev/diagnostics.md)
- [Releasing](docs/dev/releasing.md)

## Building

```
cargo build --workspace
cargo test --workspace
```

The workspace builds on any OS; capture and encoding need Windows. See
[Building from source](docs/dev/building.md).
